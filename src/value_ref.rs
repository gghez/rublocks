//! Reference expressions used across `where:`, `values:`, `view:`,
//! `output:`, `redirect.to`, `limit:`, `offset:`.
//!
//! Every rublocks site that accepts either a literal or a `$ref` to a
//! prior binding goes through this single parser. The parser is JSON-shape
//! agnostic — it accepts a `&serde_json::Value` and returns a typed
//! [`ValueRef`] that codegen turns into a Rust expression with the right
//! scope-aware ident.
//!
//! Supported syntactic forms:
//!
//! - `"$input.path.<field>"` / `"$input.query.<field>"` / `"$input.body.<field>"` —
//!   the extracted input value for the named field.
//! - `"$<block_name>"` — the whole bound output of a prior `process` block.
//! - `"$<block_name>.<field>"` — one field of a prior `db.find_one` binding.
//! - Any other JSON value (string / number / bool) — a literal.
//!
//! The mapping from [`ValueRef`] to a Rust [`proc_macro2::TokenStream`]
//! lives on [`ValueRef::emit_expr`]. Callers that need the static Rust
//! type (e.g. to decide how to bind a sqlx parameter) read it from the
//! returned [`EmittedExpr`].

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use serde_json::Value;

use crate::input::{FieldKind, InputSpec};
use crate::models::{FieldType, Model};

/// One parsed reference expression.
#[derive(Debug, Clone)]
pub enum ValueRef {
    /// Literal JSON value (string / number / bool / null).
    Literal(Value),
    /// `$input.<section>.<field>` — an extracted input field.
    Input {
        section: InputSection,
        field: String,
    },
    /// `$<block_name>` — whole bound value of a prior process block.
    Block { name: String },
    /// `$<block_name>.<field>` — one field of a `db.find_one` row binding.
    BlockField { name: String, field: String },
}

/// Which input section a `$input.X.Y` reference targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputSection {
    Path,
    Query,
    Body,
}

impl InputSection {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "path" => Some(Self::Path),
            "query" => Some(Self::Query),
            "body" => Some(Self::Body),
            _ => None,
        }
    }

    fn ident(self) -> proc_macro2::Ident {
        match self {
            Self::Path => format_ident!("_path"),
            Self::Query => format_ident!("_query"),
            Self::Body => format_ident!("_body"),
        }
    }
}

impl ValueRef {
    /// Parse a JSON value into a [`ValueRef`].
    ///
    /// Strings starting with `$` are treated as references; every other
    /// shape is taken as a literal. The error path lists the malformed
    /// token so the manifest error points the user at the right spot.
    pub fn parse(v: &Value) -> Result<Self, String> {
        let Value::String(s) = v else {
            return Ok(Self::Literal(v.clone()));
        };
        match s.strip_prefix('$') {
            None => Ok(Self::Literal(v.clone())),
            Some(rest) => Self::parse_ref(rest),
        }
    }

    fn parse_ref(rest: &str) -> Result<Self, String> {
        if rest.is_empty() {
            return Err("empty `$` reference".to_string());
        }
        // `$input.<section>.<field>` is the only three-segment form;
        // everything else is `$<name>` or `$<name>.<field>`.
        let parts: Vec<&str> = rest.split('.').collect();
        if parts[0] == "input" {
            if parts.len() != 3 {
                return Err(format!(
                    "expected `$input.<section>.<field>`, got `${rest}`"
                ));
            }
            let section = InputSection::from_str(parts[1]).ok_or_else(|| {
                format!(
                    "unknown input section `{}` — expected path/query/body",
                    parts[1]
                )
            })?;
            return Ok(Self::Input {
                section,
                field: parts[2].to_string(),
            });
        }
        match parts.len() {
            1 => Ok(Self::Block {
                name: parts[0].to_string(),
            }),
            2 => Ok(Self::BlockField {
                name: parts[0].to_string(),
                field: parts[1].to_string(),
            }),
            _ => Err(format!(
                "unsupported reference `${rest}` — expected `$<name>` or `$<name>.<field>`"
            )),
        }
    }
}

/// Rust expression + static type emitted for a [`ValueRef`].
///
/// The type annotation is metadata the caller can consult to make
/// type-aware decisions (e.g. picking a sqlx bind variant) — most
/// emission paths use the expression directly and let Rust's
/// inference do the rest. The field is retained on the struct so the
/// surface stays stable when future blocks want to discriminate.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct EmittedExpr {
    /// The Rust expression yielding the value at request time.
    pub expr: TokenStream,
    pub ty: TokenStream,
}

/// Resolved scope at one block-emission site.
///
/// Holds the typed input spec and the running list of prior block
/// bindings. Codegen builds one per route, mutating `bindings` as each
/// block in `process` emits.
#[derive(Debug, Default)]
pub struct ValueScope<'a> {
    pub input: Option<&'a InputSpec>,
    pub bindings: indexmap::IndexMap<String, ScopeBinding>,
    pub models: &'a [Model],
}

/// One prior process-block binding. The `ident` is the Rust local that
/// codegen emitted (e.g. `__block_post`), and `kind` lets the value-ref
/// resolver know what type the binding carries.
#[derive(Debug, Clone)]
pub struct ScopeBinding {
    pub ident: proc_macro2::Ident,
    pub kind: BindingKind,
}

/// Static shape of a prior process-block binding.
#[derive(Debug, Clone)]
pub enum BindingKind {
    /// `db.find_one` — single row of the named table.
    FindOne { table: String },
    /// `db.find_many` — `Vec` of rows of the named table.
    FindMany { table: String },
    /// `time.now` — formatted timestamp string.
    Scalar { ty: TokenStream },
}

impl ValueRef {
    /// Emit the Rust expression resolving this reference in `scope`.
    ///
    /// Returns the expression and its static Rust type so callers can
    /// drive sqlx-bind dispatch or context-field typing without
    /// re-parsing the reference.
    pub fn emit_expr(&self, scope: &ValueScope) -> Result<EmittedExpr, String> {
        match self {
            Self::Literal(v) => Ok(literal_expr(v)),
            Self::Input { section, field } => emit_input(scope, *section, field),
            Self::Block { name } => emit_block(scope, name),
            Self::BlockField { name, field } => emit_block_field(scope, name, field),
        }
    }
}

fn literal_expr(v: &Value) -> EmittedExpr {
    match v {
        Value::Null => EmittedExpr {
            expr: quote! { Option::<String>::None },
            ty: quote! { Option<String> },
        },
        Value::String(s) => EmittedExpr {
            expr: quote! { #s.to_string() },
            ty: quote! { String },
        },
        Value::Bool(b) => EmittedExpr {
            expr: quote! { #b },
            ty: quote! { bool },
        },
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                EmittedExpr {
                    expr: quote! { #i },
                    ty: quote! { i64 },
                }
            } else if let Some(f) = n.as_f64() {
                EmittedExpr {
                    expr: quote! { #f },
                    ty: quote! { f64 },
                }
            } else {
                let s = n.to_string();
                EmittedExpr {
                    expr: quote! { #s.to_string() },
                    ty: quote! { String },
                }
            }
        }
        // Arrays / objects can land here as literals when callers (e.g.
        // `output:` JSON projection) pass through compound values. They
        // resolve to a `serde_json::Value` at runtime so the projection
        // can splice them as-is.
        other => {
            let s = other.to_string();
            EmittedExpr {
                expr: quote! { serde_json::from_str::<serde_json::Value>(#s).expect("validated at build time") },
                ty: quote! { serde_json::Value },
            }
        }
    }
}

fn emit_input(
    scope: &ValueScope,
    section: InputSection,
    field: &str,
) -> Result<EmittedExpr, String> {
    let spec = scope.input.ok_or_else(|| {
        format!("`$input.{section:?}.{field}` referenced but route declares no input")
    })?;
    let map = match section {
        InputSection::Path => &spec.path,
        InputSection::Query => &spec.query,
        InputSection::Body => spec.body.as_ref().map(|b| &b.fields).ok_or_else(|| {
            format!("`$input.body.{field}` referenced but route has no body section")
        })?,
    };
    let f = map.get(field).ok_or_else(|| {
        format!(
            "`$input.{}.{field}` references an undeclared field — declared: {}",
            section_name(section),
            map.keys().cloned().collect::<Vec<_>>().join(", "),
        )
    })?;
    let section_ident = section.ident();
    let field_ident = format_ident!("{}", field);
    let access = quote! { #section_ident.#field_ident };
    let is_optional = !f.required && f.default.is_none();
    let (expr, base_ty) = input_field_runtime(f.ty);
    if is_optional {
        Ok(EmittedExpr {
            // For optional fields we hand back the `Option<T>` so the
            // call site can decide what to do (sqlx binds `Option<T>`
            // natively as nullable).
            expr: quote! { #access.clone() },
            ty: quote! { Option<#base_ty> },
        })
    } else {
        Ok(EmittedExpr {
            expr: clone_or_copy(&access, f.ty, &expr),
            ty: base_ty,
        })
    }
}

fn section_name(section: InputSection) -> &'static str {
    match section {
        InputSection::Path => "path",
        InputSection::Query => "query",
        InputSection::Body => "body",
    }
}

fn input_field_runtime(kind: FieldKind) -> (TokenStream, TokenStream) {
    match kind {
        FieldKind::String | FieldKind::Text | FieldKind::Email => {
            (quote! { String }, quote! { String })
        }
        FieldKind::Int => (quote! { i32 }, quote! { i32 }),
        FieldKind::Bigint => (quote! { i64 }, quote! { i64 }),
        FieldKind::Bool => (quote! { bool }, quote! { bool }),
        FieldKind::Uuid => (quote! { uuid::Uuid }, quote! { uuid::Uuid }),
        FieldKind::Timestamptz => (
            quote! { chrono::DateTime<chrono::Utc> },
            quote! { chrono::DateTime<chrono::Utc> },
        ),
    }
}

fn clone_or_copy(access: &TokenStream, kind: FieldKind, _ty: &TokenStream) -> TokenStream {
    // `Copy` types pass through by value; owned types clone so the
    // referenced field stays usable for downstream blocks.
    match kind {
        FieldKind::Int | FieldKind::Bigint | FieldKind::Bool => quote! { #access },
        _ => quote! { #access.clone() },
    }
}

fn emit_block(scope: &ValueScope, name: &str) -> Result<EmittedExpr, String> {
    let binding = scope.bindings.get(name).ok_or_else(|| {
        format!(
            "`${name}` references an unbound block — known: {}",
            known(scope)
        )
    })?;
    let ident = &binding.ident;
    let ty = match &binding.kind {
        BindingKind::FindOne { table } => {
            let m = model_for_table(scope.models, table)?;
            let mi = format_ident!("{}", m.name);
            quote! { crate::models::#mi }
        }
        BindingKind::FindMany { table } => {
            let m = model_for_table(scope.models, table)?;
            let mi = format_ident!("{}", m.name);
            quote! { Vec<crate::models::#mi> }
        }
        BindingKind::Scalar { ty } => ty.clone(),
    };
    Ok(EmittedExpr {
        expr: quote! { #ident.clone() },
        ty,
    })
}

fn emit_block_field(scope: &ValueScope, name: &str, field: &str) -> Result<EmittedExpr, String> {
    let binding = scope.bindings.get(name).ok_or_else(|| {
        format!(
            "`${name}.{field}` references an unbound block — known: {}",
            known(scope)
        )
    })?;
    match &binding.kind {
        BindingKind::FindOne { table } => {
            let m = model_for_table(scope.models, table)?;
            let def = m.fields.get(field).ok_or_else(|| {
                format!("`${name}.{field}` references unknown field on `{}`", m.name)
            })?;
            let fi = format_ident!("{}", field);
            let ident = &binding.ident;
            let (expr, ty) = field_runtime(def.ty, def.nullable, ident, &fi);
            Ok(EmittedExpr { expr, ty })
        }
        BindingKind::FindMany { .. } => Err(format!(
            "`${name}.{field}` cannot reference a field on a list binding — use `${name}` to project the whole collection"
        )),
        BindingKind::Scalar { .. } => Err(format!(
            "`${name}.{field}` cannot reference a field on a scalar binding"
        )),
    }
}

fn field_runtime(
    ty: FieldType,
    nullable: bool,
    ident: &proc_macro2::Ident,
    field: &proc_macro2::Ident,
) -> (TokenStream, TokenStream) {
    if nullable {
        // The model struct wraps nullable columns in `_rb_util::NullDisplay<T>`
        // (which holds an `Option<T>`). Extract the inner option so callers
        // bind a nullable value at the sqlx layer.
        let base = base_ty(ty);
        (quote! { #ident.#field.0.clone() }, quote! { Option<#base> })
    } else {
        let t = base_ty(ty);
        let expr = match ty {
            FieldType::Int | FieldType::Bigint | FieldType::Bool => quote! { #ident.#field },
            _ => quote! { #ident.#field.clone() },
        };
        (expr, t)
    }
}

fn base_ty(ty: FieldType) -> TokenStream {
    match ty {
        FieldType::Uuid => quote! { uuid::Uuid },
        FieldType::String | FieldType::Text | FieldType::Email => quote! { String },
        FieldType::Int => quote! { i32 },
        FieldType::Bigint => quote! { i64 },
        FieldType::Bool => quote! { bool },
        FieldType::Timestamptz => quote! { chrono::DateTime<chrono::Utc> },
    }
}

fn known(scope: &ValueScope) -> String {
    let v: Vec<&str> = scope.bindings.keys().map(String::as_str).collect();
    if v.is_empty() {
        "(none)".to_string()
    } else {
        v.join(", ")
    }
}

fn model_for_table<'a>(models: &'a [Model], table: &str) -> Result<&'a Model, String> {
    models
        .iter()
        .find(|m| m.table == table)
        .ok_or_else(|| format!("no model declares table `{table}`"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_literal_string() {
        let r = ValueRef::parse(&json!("hello")).unwrap();
        assert!(matches!(r, ValueRef::Literal(Value::String(_))));
    }

    #[test]
    fn parses_literal_number() {
        let r = ValueRef::parse(&json!(42)).unwrap();
        assert!(matches!(r, ValueRef::Literal(Value::Number(_))));
    }

    #[test]
    fn parses_input_path_field() {
        let r = ValueRef::parse(&json!("$input.path.slug")).unwrap();
        match r {
            ValueRef::Input { section, field } => {
                assert_eq!(section, InputSection::Path);
                assert_eq!(field, "slug");
            }
            other => panic!("expected Input, got {other:?}"),
        }
    }

    #[test]
    fn parses_block_field() {
        let r = ValueRef::parse(&json!("$post.author_id")).unwrap();
        match r {
            ValueRef::BlockField { name, field } => {
                assert_eq!(name, "post");
                assert_eq!(field, "author_id");
            }
            other => panic!("expected BlockField, got {other:?}"),
        }
    }

    #[test]
    fn parses_whole_block() {
        let r = ValueRef::parse(&json!("$posts")).unwrap();
        match r {
            ValueRef::Block { name } => assert_eq!(name, "posts"),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_input_section() {
        let err = ValueRef::parse(&json!("$input.cookies.tok")).unwrap_err();
        assert!(err.contains("unknown input section"), "{err}");
    }

    #[test]
    fn rejects_input_with_wrong_arity() {
        let err = ValueRef::parse(&json!("$input.path")).unwrap_err();
        assert!(err.contains("expected `$input"), "{err}");
    }

    #[test]
    fn rejects_too_deep_reference() {
        let err = ValueRef::parse(&json!("$post.author.name")).unwrap_err();
        assert!(err.contains("unsupported reference"), "{err}");
    }
}
