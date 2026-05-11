//! Code generation for the auto-derived input validator.
//!
//! Given a route's typed [`InputSpec`], this module emits:
//!
//! - Per-section extractor structs (`PathInput`, `QueryInput`, `BodyInput`)
//!   that Axum populates from the request.
//! - A `validate_input` function returning `Vec<rublocks_input::FieldError>`.
//!   Each declared constraint (required / default / min / max /
//!   min_length / max_length / pattern) maps to one `if`-test.
//!
//! Declaring the input typed is the **only** thing the author has to do —
//! the validator follows automatically. See `docs/input.md`.
//!
//! The dist-side `_rb_input` module hosts `FieldError`. It is emitted by
//! `render_rb_input_module` and gated on at least one route declaring an
//! input.

use indexmap::IndexMap;
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use serde_json::Value;

use crate::input::{FieldKind, FieldSpec, InputSpec};
use crate::routes::{HttpMethod, Route};

/// Does this manifest need the `_rb_input` helper module + the `regex`
/// dependency? True as soon as at least one route declares an input spec.
pub fn project_uses_input(routes: &[Route]) -> bool {
    routes
        .iter()
        .any(|r| r.input.as_ref().is_some_and(|s| !s.is_empty()))
}

/// True when at least one route declares a `pattern` constraint —
/// gates the inclusion of the `regex` crate in the dist `Cargo.toml`
/// and the `OnceLock<Regex>` cache in the generated code.
pub fn project_uses_pattern(routes: &[Route]) -> bool {
    routes
        .iter()
        .any(|r| r.input.as_ref().map(input_has_pattern).unwrap_or(false))
}

fn input_has_pattern(spec: &InputSpec) -> bool {
    fields_have_pattern(&spec.path)
        || fields_have_pattern(&spec.query)
        || spec
            .body
            .as_ref()
            .map(|b| fields_have_pattern(&b.fields))
            .unwrap_or(false)
}

fn fields_have_pattern(map: &IndexMap<String, FieldSpec>) -> bool {
    map.values().any(|f| f.pattern.is_some())
}

/// Emit `pub mod _rb_input { ... }` — the dist-side hosting of `FieldError`
/// and the JSON serialisation helpers shared by every per-route handler.
pub fn render_rb_input_module() -> TokenStream {
    quote! {
        pub mod _rb_input {
            /// One declarative validation failure. Shape matches what
            /// `kind: api` handlers emit as the JSON `errors[]` body and
            /// what `kind: page` handlers expose as `$errors` in the
            /// re-rendered template.
            #[derive(Debug, Clone, serde::Serialize, Default)]
            pub struct FieldError {
                pub field: String,
                pub code: String,
                pub message: String,
            }

            impl std::fmt::Display for FieldError {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    write!(f, "{}: {}", self.field, self.message)
                }
            }

            /// Render a `422 Unprocessable Content` response for `kind: api`
            /// routes — JSON body with the `errors` array. 422 is the
            /// semantically correct status for validation failures: the
            /// request was well-formed (Axum's extractors already accepted
            /// it; 400 covers the parse-error path), it just doesn't meet
            /// the declared constraints.
            pub fn api_422(errors: Vec<FieldError>) -> axum::response::Response {
                use axum::response::IntoResponse as _;
                (
                    axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                    axum::Json(serde_json::json!({ "errors": errors })),
                )
                    .into_response()
            }

            /// Render a `422 Unprocessable Content` response for `kind: page`
            /// routes whose handler does not yet re-render their template —
            /// surfaces a plain text dump of the errors so the failure is
            /// visible in the browser during development. Same status code
            /// rationale as `api_422`.
            pub fn page_422_text(errors: Vec<FieldError>) -> axum::response::Response {
                use axum::response::IntoResponse as _;
                let body = errors
                    .iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                (axum::http::StatusCode::UNPROCESSABLE_ENTITY, body).into_response()
            }
        }
    }
}

/// Cargo dependencies the input layer adds to the generated `Cargo.toml`.
pub fn cargo_dependencies(routes: &[Route]) -> Vec<&'static str> {
    let mut out = Vec::new();
    if project_uses_pattern(routes) {
        // `regex` is needed to enforce `pattern` constraints. The
        // dist-side instances are cached via `OnceLock` so we pay the
        // compile cost once per process.
        out.push("regex = \"1\"\n");
    }
    out
}

/// Emit the per-route bindings that populate a CEL `Context` with the
/// route's input fields. Names are top-level: `_path.slug` binds to
/// `slug`, `_body.title` binds to `title`. The build-time scope-checker
/// rejects collisions across sections so this is unambiguous.
///
/// Caller must define `__ctx: cel_interpreter::Context` already.
pub fn render_input_cel_bindings_raw(spec: &InputSpec) -> TokenStream {
    if spec.is_empty() {
        return quote! {};
    }
    let mut stmts: Vec<TokenStream> = Vec::new();
    for (name, f) in &spec.path {
        stmts.push(emit_section_binding("_path", name, f));
    }
    for (name, f) in &spec.query {
        stmts.push(emit_section_binding("_query", name, f));
    }
    if let Some(body) = spec.body.as_ref() {
        for (name, f) in &body.fields {
            stmts.push(emit_section_binding("_body", name, f));
        }
    }
    quote! { #(#stmts)* }
}

/// One `__ctx.add_variable_from_value("name", <expr>);` statement. The
/// expression is keyed on the field's Rust type so we land on a
/// `cel_interpreter::Value` that's natural to compare against from CEL.
/// Optional fields bind `Null` automatically through CEL's
/// `From<Option<T>>` impl.
fn emit_section_binding(section: &str, name: &str, f: &FieldSpec) -> TokenStream {
    let section_ident = format_ident!("{}", section);
    let field_ident = format_ident!("{}", name);
    let access = quote! { #section_ident.#field_ident };
    let is_optional = !f.required && f.default.is_none();
    let expr = match (f.ty, is_optional) {
        // `i32` widens to CEL's `i64`. `Option<i32>` maps via `.map`.
        (FieldKind::Int, false) => quote! { #access as i64 },
        (FieldKind::Int, true) => quote! { #access.map(|__v| __v as i64) },
        (FieldKind::Bigint, _) | (FieldKind::Bool, _) => quote! { #access },
        (FieldKind::String | FieldKind::Text | FieldKind::Email, _) => quote! { #access.clone() },
        (FieldKind::Uuid, false) => quote! { #access.to_string() },
        (FieldKind::Uuid, true) => quote! { #access.as_ref().map(|__v| __v.to_string()) },
        (FieldKind::Timestamptz, false) => quote! { #access.to_rfc3339() },
        (FieldKind::Timestamptz, true) => quote! { #access.as_ref().map(|__v| __v.to_rfc3339()) },
    };
    quote! {
        __ctx.add_variable_from_value(#name, #expr);
    }
}

/// Emit the per-route `ctx_input_<name>` module containing the extractor
/// structs (PathInput / QueryInput / BodyInput) and the `validate_input`
/// function. Returns `None` when the route has no input spec.
pub fn render_per_route_module(route: &Route) -> Option<TokenStream> {
    let spec = route.input.as_ref()?;
    if spec.is_empty() {
        return None;
    }
    let mod_name = format_ident!("ctx_input_{}", route.name);
    let path_struct =
        (!spec.path.is_empty()).then(|| render_section_struct("PathInput", &spec.path));
    let query_struct =
        (!spec.query.is_empty()).then(|| render_section_struct("QueryInput", &spec.query));
    let body_struct = spec
        .body
        .as_ref()
        .filter(|b| !b.fields.is_empty())
        .map(|b| render_section_struct("BodyInput", &b.fields));

    let validate_fn = render_validate_fn(spec);

    Some(quote! {
        pub mod #mod_name {
            #path_struct
            #query_struct
            #body_struct
            #validate_fn
        }
    })
}

/// Emit one extractor struct with the right `#[derive]` so Axum can
/// populate it. `serde::Deserialize` is mandatory; `Default` simplifies
/// the page re-render path (TODO in a follow-up).
fn render_section_struct(struct_name: &str, fields: &IndexMap<String, FieldSpec>) -> TokenStream {
    let ident = format_ident!("{}", struct_name);
    let field_defs = fields.iter().map(|(name, f)| {
        let fi = format_ident!("{}", name);
        let ty = rust_type_for(f.ty, !f.required && f.default.is_none());
        let default_attr = f.default.as_ref().map(|d| {
            let fn_name = format_ident!("__rb_default_{}_{}", struct_name.to_lowercase(), name);
            // Inject `#[serde(default = "fn_path")]` so an absent field is
            // filled in from the declared default. The function is emitted
            // alongside the struct below.
            let path = fn_name.to_string();
            let _ = d;
            quote! { #[serde(default = #path)] }
        });
        quote! {
            #default_attr
            pub #fi: #ty,
        }
    });
    let default_fns = fields.iter().filter_map(|(name, f)| {
        let d = f.default.as_ref()?;
        let fn_name = format_ident!("__rb_default_{}_{}", struct_name.to_lowercase(), name);
        let ty = rust_type_for(f.ty, false);
        let lit = default_literal(d, f.ty);
        Some(quote! {
            fn #fn_name() -> #ty { #lit }
        })
    });
    quote! {
        #[derive(Debug, Clone, Default, serde::Deserialize)]
        pub struct #ident {
            #(#field_defs)*
        }
        #(#default_fns)*
    }
}

/// Rust type for one field of one input section.
///
/// `wrap_optional` is `true` for fields that are neither required nor have
/// a default — those become `Option<T>` so Axum's Path / Query / Json
/// extractors accept their absence without erroring out before the
/// generated validator gets a chance to report a clean field-level message.
fn rust_type_for(kind: FieldKind, wrap_optional: bool) -> TokenStream {
    let base = match kind {
        FieldKind::String | FieldKind::Text | FieldKind::Email => quote! { String },
        FieldKind::Int => quote! { i32 },
        FieldKind::Bigint => quote! { i64 },
        FieldKind::Bool => quote! { bool },
        FieldKind::Uuid => quote! { uuid::Uuid },
        FieldKind::Timestamptz => quote! { chrono::DateTime<chrono::Utc> },
    };
    if wrap_optional {
        quote! { Option<#base> }
    } else {
        base
    }
}

/// Render the `validate_input` function for a route. The function takes
/// references to the extracted sections and returns `Vec<FieldError>`.
/// Every constraint declared in the input spec yields one check.
fn render_validate_fn(spec: &InputSpec) -> TokenStream {
    let mut checks: Vec<TokenStream> = Vec::new();
    for (name, f) in &spec.path {
        emit_field_checks(&mut checks, "path", name, f);
    }
    for (name, f) in &spec.query {
        emit_field_checks(&mut checks, "query", name, f);
    }
    if let Some(body) = spec.body.as_ref() {
        for (name, f) in &body.fields {
            emit_field_checks(&mut checks, "body", name, f);
        }
    }
    let has_path = !spec.path.is_empty();
    let has_query = !spec.query.is_empty();
    let has_body = spec
        .body
        .as_ref()
        .map(|b| !b.fields.is_empty())
        .unwrap_or(false);
    let path_param = has_path.then(|| quote! { _path: &PathInput, });
    let query_param = has_query.then(|| quote! { _query: &QueryInput, });
    let body_param = has_body.then(|| quote! { _body: &BodyInput, });
    quote! {
        /// Auto-generated validator. The body is derived from the typed
        /// input spec — every constraint declared in the manifest maps
        /// to exactly one check below.
        pub fn validate_input(
            #path_param
            #query_param
            #body_param
        ) -> Vec<super::_rb_input::FieldError> {
            #[allow(unused_mut)]
            let mut errors: Vec<super::_rb_input::FieldError> = Vec::new();
            #(#checks)*
            errors
        }
    }
}

/// Push every check matching the declared constraints on one field into
/// the accumulating vec. Field path in the error report is
/// `<section>.<name>`.
fn emit_field_checks(out: &mut Vec<TokenStream>, section: &str, name: &str, f: &FieldSpec) {
    let field_path = format!("{section}.{name}");
    let section_ident = format_ident!("_{}", section);
    let field_ident = format_ident!("{}", name);
    let access = quote! { #section_ident.#field_ident };
    let is_optional = !f.required && f.default.is_none();
    let is_string_shaped = f.ty.is_string_shaped();

    // Numeric bounds (only on `int` / `bigint`).
    let mut numeric_body: Vec<TokenStream> = Vec::new();
    if let Some(min) = f.min {
        numeric_body.push(quote! {
            if __n < #min as _ {
                errors.push(super::_rb_input::FieldError {
                    field: #field_path.to_string(),
                    code: "min".to_string(),
                    message: format!("must be >= {}", #min),
                });
            }
        });
    }
    if let Some(max) = f.max {
        numeric_body.push(quote! {
            if __n > #max as _ {
                errors.push(super::_rb_input::FieldError {
                    field: #field_path.to_string(),
                    code: "max".to_string(),
                    message: format!("must be <= {}", #max),
                });
            }
        });
    }
    if !numeric_body.is_empty() {
        out.push(wrap_numeric(
            &access,
            is_optional,
            quote! { #(#numeric_body)* },
        ));
    }

    // String-shaped checks: length + pattern + email shorthand.
    let mut string_body: Vec<TokenStream> = Vec::new();
    if let Some(min_len) = f.min_length {
        string_body.push(quote! {
            if __s.chars().count() < #min_len as usize {
                errors.push(super::_rb_input::FieldError {
                    field: #field_path.to_string(),
                    code: "min_length".to_string(),
                    message: format!("must be at least {} chars", #min_len),
                });
            }
        });
    }
    if let Some(max_len) = f.max_length {
        string_body.push(quote! {
            if __s.chars().count() > #max_len as usize {
                errors.push(super::_rb_input::FieldError {
                    field: #field_path.to_string(),
                    code: "max_length".to_string(),
                    message: format!("must be at most {} chars", #max_len),
                });
            }
        });
    }
    if let Some(pattern) = f.pattern.as_ref() {
        let src = &pattern.source;
        let cache_ident = format_ident!(
            "__RB_PATTERN_{}_{}",
            section.to_uppercase(),
            name.to_uppercase()
        );
        string_body.push(quote! {
            static #cache_ident: std::sync::OnceLock<regex::Regex> =
                std::sync::OnceLock::new();
            let __re = #cache_ident.get_or_init(|| {
                regex::Regex::new(#src).expect("regex was syntax-checked at build time")
            });
            if !__re.is_match(__s) {
                errors.push(super::_rb_input::FieldError {
                    field: #field_path.to_string(),
                    code: "pattern".to_string(),
                    message: format!("does not match {}", #src),
                });
            }
        });
    }
    if matches!(f.ty, FieldKind::Email) {
        // Structural sanity check — keeps the dependency surface small.
        // Stricter rules go through a CEL `validate` expression.
        string_body.push(quote! {
            let __parts: Vec<&str> = __s.split('@').collect();
            if __parts.len() != 2 || __parts[0].is_empty() || !__parts[1].contains('.') {
                errors.push(super::_rb_input::FieldError {
                    field: #field_path.to_string(),
                    code: "email".to_string(),
                    message: "must be a valid email address".to_string(),
                });
            }
        });
    }
    if !string_body.is_empty() && is_string_shaped {
        out.push(wrap_string(
            &access,
            is_optional,
            quote! { #(#string_body)* },
        ));
    }

    // CEL `validate` — evaluate the predicate at request time, with the
    // field value bound to its own name. False ⇒ push a FieldError so the
    // 422 path the handler already wires picks it up. The compiled
    // `Program` is cached in a `OnceLock` so the parse cost is one-shot.
    if let Some(cel_src) = f.validate.as_ref() {
        let prog_ident = format_ident!(
            "__RB_CEL_{}_{}",
            section.to_uppercase(),
            name.to_uppercase()
        );
        let binding = render_cel_binding(name, f.ty);
        out.push(render_cel_validate(
            &access,
            is_optional,
            &field_path,
            cel_src,
            &prog_ident,
            binding,
        ));
    }
}

/// Build the `cel_interpreter::Context` binding for one input field. The
/// value lives under the field's own name — the same convention the
/// scope-check at build time enforces.
///
/// `__v` is the bound Rust value at this point in the generated code;
/// the caller wraps `__v` in either `let __v = #access;` (non-optional)
/// or `if let Some(__v) = #access` (optional).
fn render_cel_binding(name: &str, kind: FieldKind) -> TokenStream {
    // `__v` is always a `&T` at the call site (either `&_section.field`
    // or the Some-bound reference from an optional field). We deref into
    // the right CEL `Value`-convertible Rust type per kind.
    match kind {
        // CEL `Int` is `i64`; the `i32`-shaped Rust value is widened.
        FieldKind::Int => quote! {
            __ctx.add_variable_from_value(#name, *__v as i64);
        },
        FieldKind::Bigint => quote! {
            __ctx.add_variable_from_value(#name, *__v);
        },
        FieldKind::Bool => quote! {
            __ctx.add_variable_from_value(#name, *__v);
        },
        FieldKind::String | FieldKind::Text | FieldKind::Email => quote! {
            __ctx.add_variable_from_value(#name, __v.clone());
        },
        // UUIDs and timestamps land as their canonical string form so CEL
        // string operators (`size`, equality, `matches`) work uniformly.
        FieldKind::Uuid => quote! {
            __ctx.add_variable_from_value(#name, __v.to_string());
        },
        FieldKind::Timestamptz => quote! {
            __ctx.add_variable_from_value(#name, __v.to_rfc3339());
        },
    }
}

/// Render one full CEL validation block. Cached `Program` + per-request
/// `Context` build + result classification (Bool true ⇒ pass, anything
/// else ⇒ push a structured FieldError).
fn render_cel_validate(
    access: &TokenStream,
    is_optional: bool,
    field_path: &str,
    cel_src: &str,
    prog_ident: &proc_macro2::Ident,
    binding: TokenStream,
) -> TokenStream {
    let eval = quote! {
        let __prog = #prog_ident.get_or_init(|| {
            cel_interpreter::Program::compile(#cel_src)
                .expect("CEL was syntax-checked at build time")
        });
        let mut __ctx = cel_interpreter::Context::default();
        #binding
        match __prog.execute(&__ctx) {
            Ok(cel_interpreter::Value::Bool(true)) => {}
            Ok(_) => {
                errors.push(super::_rb_input::FieldError {
                    field: #field_path.to_string(),
                    code: "validate".to_string(),
                    message: format!("must satisfy `{}`", #cel_src),
                });
            }
            Err(e) => {
                errors.push(super::_rb_input::FieldError {
                    field: #field_path.to_string(),
                    code: "validate".to_string(),
                    message: format!("evaluation failed: {e}"),
                });
            }
        }
    };
    let body = quote! {
        static #prog_ident: std::sync::OnceLock<cel_interpreter::Program> =
            std::sync::OnceLock::new();
        #eval
    };
    if is_optional {
        quote! {
            if let Some(__v) = #access.as_ref() {
                #body
            }
        }
    } else {
        quote! {
            {
                let __v = &#access;
                #body
            }
        }
    }
}

/// Wrap a numeric-check body in the appropriate scoping construct so
/// `__n` binds to either the unwrapped field value (non-optional) or the
/// `Some`-bound copy (optional). Generates no code when the body is empty.
fn wrap_numeric(access: &TokenStream, is_optional: bool, body: TokenStream) -> TokenStream {
    if is_optional {
        quote! {
            if let Some(__n) = #access {
                #body
            }
        }
    } else {
        quote! {
            {
                let __n = #access;
                #body
            }
        }
    }
}

/// Wrap a string-check body in the appropriate scoping construct so
/// `__s` binds to a `&str` view of the field, regardless of whether the
/// underlying field is `String` or `Option<String>`.
fn wrap_string(access: &TokenStream, is_optional: bool, body: TokenStream) -> TokenStream {
    if is_optional {
        quote! {
            if let Some(__s) = #access.as_deref() {
                #body
            }
        }
    } else {
        quote! {
            {
                let __s: &str = #access.as_str();
                #body
            }
        }
    }
}

/// Render a literal JSON default into a Rust expression of the right type.
///
/// Defaults are validated at load time so the kind matches the JSON value
/// — we can `expect` confidently here.
fn default_literal(value: &Value, kind: FieldKind) -> TokenStream {
    match kind {
        FieldKind::String | FieldKind::Text | FieldKind::Email => {
            let s = value.as_str().expect("string default validated at load");
            quote! { #s.to_string() }
        }
        FieldKind::Int => {
            let n = value.as_i64().expect("int default validated at load") as i32;
            quote! { #n }
        }
        FieldKind::Bigint => {
            let n = value.as_i64().expect("bigint default validated at load");
            quote! { #n }
        }
        FieldKind::Bool => {
            let b = value.as_bool().expect("bool default validated at load");
            quote! { #b }
        }
        FieldKind::Uuid => {
            let s = value.as_str().expect("uuid default validated at load");
            quote! { #s.parse::<uuid::Uuid>().expect("invalid uuid default") }
        }
        FieldKind::Timestamptz => {
            let s = value
                .as_str()
                .expect("timestamptz default validated at load");
            quote! { #s.parse::<chrono::DateTime<chrono::Utc>>().expect("invalid timestamptz default") }
        }
    }
}

/// Render the handler-side parameter list for a route's extractors.
/// Used by [`crate::codegen`] to splice the right Axum extractors into
/// the handler signature when an input spec is declared. Empty when the
/// route has no input — the handler signature stays parameterless.
pub fn handler_extractor_params(route: &Route) -> TokenStream {
    let Some(spec) = route.input.as_ref() else {
        return quote! {};
    };
    if spec.is_empty() {
        return quote! {};
    }
    let mod_name = format_ident!("ctx_input_{}", route.name);
    let mut params: Vec<TokenStream> = Vec::new();
    if !spec.path.is_empty() {
        params.push(quote! {
            axum::extract::Path(_path): axum::extract::Path<#mod_name::PathInput>
        });
    }
    if !spec.query.is_empty() {
        params.push(quote! {
            axum::extract::Query(_query): axum::extract::Query<#mod_name::QueryInput>
        });
    }
    if let Some(body) = spec.body.as_ref()
        && !body.fields.is_empty()
    {
        if body.form {
            params.push(quote! {
                axum::extract::Form(_body): axum::extract::Form<#mod_name::BodyInput>
            });
        } else {
            // Json must come last — Axum's extractor ordering rule.
            params.push(quote! {
                axum::extract::Json(_body): axum::extract::Json<#mod_name::BodyInput>
            });
        }
    }
    quote! { #(#params),* }
}

/// Render the validator call inside a handler. Returns a TokenStream that
/// binds `__rb_input_errors: Vec<FieldError>` in the handler scope.
pub fn handler_validation_call(route: &Route) -> TokenStream {
    let Some(spec) = route.input.as_ref() else {
        return quote! { let __rb_input_errors: Vec<crate::_rb_input::FieldError> = Vec::new(); };
    };
    if spec.is_empty() {
        return quote! { let __rb_input_errors: Vec<crate::_rb_input::FieldError> = Vec::new(); };
    }
    let mod_name = format_ident!("ctx_input_{}", route.name);
    let mut args: Vec<TokenStream> = Vec::new();
    if !spec.path.is_empty() {
        args.push(quote! { &_path });
    }
    if !spec.query.is_empty() {
        args.push(quote! { &_query });
    }
    if let Some(body) = spec.body.as_ref()
        && !body.fields.is_empty()
    {
        args.push(quote! { &_body });
    }
    quote! {
        let __rb_input_errors = #mod_name::validate_input(#(#args),*);
    }
}

/// Methods that carry a body in their request — those are the only ones
/// where a body extractor is meaningful. Used by codegen to skip the
/// body section for GET / DELETE routes (Axum would reject the handler
/// signature otherwise).
#[allow(dead_code)]
pub fn method_accepts_body(method: HttpMethod) -> bool {
    matches!(
        method,
        HttpMethod::Post | HttpMethod::Put | HttpMethod::Patch
    )
}
