//! Rust source generation for the target Axum project.
//!
//! Code is built as a `proc_macro2::TokenStream` via `quote!`, parsed with
//! `syn`, and pretty-printed with `prettyplease`. This guarantees
//! syntactically valid, well-formatted output and avoids the fragility of
//! string templates. `Cargo.toml` is the only string-template exception
//! (TOML has no quote-equivalent).
//!
//! See `docs/architecture.md` and `docs/decisions.md`.

use crate::layouts::{Layout, RequireType};
use crate::manifest::{Manifest, ServiceUrl};
use crate::models::{FieldType, Model};
use crate::routes::{HttpMethod, ProcessBlock, Route, RouteKind, axum_path};
use anyhow::{Context, Result};
use indexmap::IndexMap;
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use std::fs;
use std::path::Path;

/// Emit the full generated project under `dist_dir`.
///
/// `dist/target/` is intentionally preserved between regenerations so cargo
/// can rebuild incrementally — a clean wipe would force a full ~30s rebuild
/// and make `rublocks dev` unusable.
///
/// `project_dir` is the source directory containing the user's `templates/`
/// — Askama looks up template files relative to the dist crate root, so we
/// mirror the directory over on every codegen pass.
pub fn emit(manifest: &Manifest, project_dir: &Path, dist_dir: &Path) -> Result<()> {
    if dist_dir.exists() {
        let target = dist_dir.join("target");
        for entry in fs::read_dir(dist_dir)? {
            let path = entry?.path();
            if path == target {
                continue;
            }
            if path.is_dir() {
                fs::remove_dir_all(&path)
                    .with_context(|| format!("failed to clean {}", path.display()))?;
            } else {
                fs::remove_file(&path)
                    .with_context(|| format!("failed to clean {}", path.display()))?;
            }
        }
    }
    fs::create_dir_all(dist_dir.join("src"))
        .with_context(|| format!("failed to create {}", dist_dir.display()))?;

    fs::write(dist_dir.join("Cargo.toml"), render_cargo_toml(manifest))?;
    fs::write(
        dist_dir.join("src").join("main.rs"),
        render_main_rs(manifest)?,
    )?;
    copy_templates(project_dir, dist_dir)?;
    Ok(())
}

/// Mirror `<project>/templates/` into `<dist>/templates/` so Askama's path
/// resolver finds them at compile time.
///
/// Always wipes the destination first — leaving stale templates around could
/// mask a file the user just deleted on the source side.
fn copy_templates(project_dir: &Path, dist_dir: &Path) -> Result<()> {
    let src = project_dir.join("templates");
    if !src.is_dir() {
        return Ok(());
    }
    let dst = dist_dir.join("templates");
    copy_dir_recursive(&src, &dst)
        .with_context(|| format!("failed to copy templates from {}", src.display()))
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Does this manifest declare at least one route that renders an HTML page?
fn has_page_routes(manifest: &Manifest) -> bool {
    manifest
        .routes
        .iter()
        .any(|r| r.kind == RouteKind::Page && r.method == HttpMethod::Get)
}

/// Build the generated `Cargo.toml` as a plain string.
///
/// Only services declared in the manifest add their crate to `[dependencies]`,
/// keeping the dist project minimal. `axum`, `tokio`, `anyhow`, `futures-util`
/// are always present (they back the `/health` route, the runtime, error
/// propagation, and the dev-mode SSE stream respectively).
fn render_cargo_toml(manifest: &Manifest) -> String {
    let mut deps = String::from(
        "axum = \"0.8\"\n\
         tokio = { version = \"1\", features = [\"macros\", \"rt-multi-thread\"] }\n\
         anyhow = \"1\"\n\
         futures-util = \"0.3\"\n",
    );
    // Models require uuid + chrono + serde regardless of services, and the
    // sqlx feature set must include any column type they reference (so the
    // FromRow derive compiles even before any handler queries the database).
    if !manifest.models.is_empty() {
        deps.push_str("serde = { version = \"1\", features = [\"derive\"] }\n");
        if manifest
            .models
            .iter()
            .any(|m| uses_type(m, FieldType::Uuid))
        {
            deps.push_str("uuid = { version = \"1\", features = [\"serde\"] }\n");
        }
        if manifest
            .models
            .iter()
            .any(|m| uses_type(m, FieldType::Timestamptz))
        {
            deps.push_str("chrono = { version = \"0.4\", features = [\"serde\"] }\n");
        }
    }
    if manifest.services.postgres.is_some() {
        let mut feats = vec!["runtime-tokio", "tls-rustls", "postgres"];
        if !manifest.models.is_empty() {
            // `derive` exposes `sqlx::FromRow`, which every generated model
            // depends on for future db query mapping.
            feats.push("derive");
        }
        if manifest
            .models
            .iter()
            .any(|m| uses_type(m, FieldType::Uuid))
        {
            feats.push("uuid");
        }
        if manifest
            .models
            .iter()
            .any(|m| uses_type(m, FieldType::Timestamptz))
        {
            feats.push("chrono");
        }
        let feats_str = feats
            .iter()
            .map(|f| format!("\"{f}\""))
            .collect::<Vec<_>>()
            .join(", ");
        deps.push_str(&format!(
            "sqlx = {{ version = \"0.8\", default-features = false, features = [{feats_str}] }}\n",
        ));
    }
    if manifest.services.redis.is_some() {
        deps.push_str("deadpool-redis = { version = \"0.18\", features = [\"rt_tokio_1\"] }\n");
    }
    // Askama lives in the dist crate only when a page route actually needs it
    // — projects that ship pure JSON APIs keep their dependency surface small.
    if has_page_routes(manifest) {
        deps.push_str("askama = \"0.14\"\n");
    }

    format!(
        "# Generated by rublocks. Do not edit by hand.\n\
         [package]\n\
         name = \"{name}\"\n\
         version = \"0.1.0\"\n\
         edition = \"2024\"\n\
         \n\
         [dependencies]\n\
         {deps}",
        name = manifest.name,
    )
}

/// Build the generated `src/main.rs` as a formatted Rust string.
///
/// The output is structured the same way for every project:
///
/// 1. `AppState` struct (fields conditional on declared services).
/// 2. `#[tokio::main] async fn main` — initializes pools, registers routes,
///    enables dev routes if `RUBLOCKS_DEV=1`, then `axum::serve`.
/// 3. The `/health` handler (always present).
/// 4. Dev-mode handlers (always compiled, mounted only when env var set —
///    see `docs/dev-mode.md`).
///
/// The `quote!` macro handles `Option<TokenStream>` interpolation natively:
/// `None` expands to nothing, so the conditional blocks for postgres/redis
/// disappear cleanly when those services are absent.
fn render_main_rs(manifest: &Manifest) -> Result<String> {
    let has_pg = manifest.services.postgres.is_some();
    let has_redis = manifest.services.redis.is_some();

    let pg_field = has_pg.then(|| quote! { pub pg: sqlx::PgPool, });
    let redis_field = has_redis.then(|| quote! { pub redis: deadpool_redis::Pool, });

    let pg_init = manifest.services.postgres.as_ref().map(|svc| {
        let url = url_expr(&svc.url);
        quote! {
            let pg = sqlx::PgPool::connect(&#url).await?;
        }
    });
    let redis_init = manifest.services.redis.as_ref().map(|svc| {
        let url = url_expr(&svc.url);
        quote! {
            let redis = deadpool_redis::Config::from_url(&#url)
                .create_pool(Some(deadpool_redis::Runtime::Tokio1))?;
        }
    });

    let pg_state = has_pg.then(|| quote! { pg, });
    let redis_state = has_redis.then(|| quote! { redis, });

    let user_owns_root_get = manifest
        .routes
        .iter()
        .any(|r| r.method == HttpMethod::Get && r.path == "/");
    let dev_index_fn = (!user_owns_root_get).then(|| {
        let html = render_dev_index_html(&manifest.name);
        quote! {
            async fn dev_index() -> axum::response::Html<&'static str> {
                axum::response::Html(#html)
            }
        }
    });
    let dev_root_route = (!user_owns_root_get).then(|| {
        quote! { router = router.route("/", get(dev_index)); }
    });

    let method_imports = used_method_imports(&manifest.routes);
    let route_registrations = manifest.routes.iter().map(render_route_registration);
    let route_handlers = manifest
        .routes
        .iter()
        .map(|r| render_route_handler(r, &manifest.layouts, &manifest.models));
    let models_module = render_models_module(&manifest.models, has_pg);
    let rb_util_module = render_rb_util_module(&manifest.models, has_pg);
    let dev_inject_fn = has_page_routes(manifest).then(|| {
        quote! {
            /// Inject the livereload snippet into a rendered page when
            /// `RUBLOCKS_DEV=1` — so editing a template auto-reloads the
            /// browser. The non-dev path is a single env-var lookup.
            fn maybe_inject_dev_snippet(html: String) -> String {
                if std::env::var("RUBLOCKS_DEV").is_err() {
                    return html;
                }
                const SNIPPET: &str = "<script src=\"/__rublocks/livereload.js\"></script>";
                match html.rfind("</body>") {
                    Some(idx) => format!("{}{}{}", &html[..idx], SNIPPET, &html[idx..]),
                    None => format!("{html}{SNIPPET}"),
                }
            }
        }
    });

    let tokens = quote! {
        use axum::{routing::{#(#method_imports),*}, Router};

        #rb_util_module

        #models_module

        #[derive(Clone)]
        pub struct AppState {
            #pg_field
            #redis_field
        }

        #[tokio::main]
        async fn main() -> anyhow::Result<()> {
            #pg_init
            #redis_init

            let state = AppState {
                #pg_state
                #redis_state
            };

            let mut router = Router::new().route("/health", get(health));

            #(#route_registrations)*

            if std::env::var("RUBLOCKS_DEV").is_ok() {
                #dev_root_route
                router = router
                    .route("/__rublocks/livereload.js", get(dev_snippet))
                    .route("/__rublocks/events", get(dev_events));
                eprintln!("rublocks: dev endpoints enabled at /__rublocks/*");
            }

            let app = router.with_state(state);

            let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
            println!("rublocks app listening on http://0.0.0.0:3000");
            axum::serve(listener, app).await?;
            Ok(())
        }

        async fn health() -> &'static str {
            "ok"
        }

        #(#route_handlers)*

        #dev_index_fn

        #dev_inject_fn

        async fn dev_snippet() -> impl axum::response::IntoResponse {
            (
                [(axum::http::header::CONTENT_TYPE, "application/javascript")],
                LIVERELOAD_JS,
            )
        }

        async fn dev_events() -> axum::response::Sse<
            impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
        > {
            let stream = futures_util::stream::pending::<
                Result<axum::response::sse::Event, std::convert::Infallible>,
            >();
            axum::response::Sse::new(stream)
                .keep_alive(axum::response::sse::KeepAlive::default())
        }

        const LIVERELOAD_JS: &str = #LIVERELOAD_JS_SOURCE;
    };

    let file: syn::File = syn::parse2(tokens).context("failed to parse generated tokens")?;
    let body = prettyplease::unparse(&file);
    Ok(format!(
        "// Generated by rublocks. Do not edit by hand.\n\n{body}"
    ))
}

fn uses_type(model: &Model, ty: FieldType) -> bool {
    model.fields.values().any(|f| f.ty == ty)
}

/// Emit `pub mod models { ... }` with one struct per declared entity.
///
/// Field order follows the JSON source (preserved via `IndexMap`) so the
/// generated struct reads the same way the user wrote it. `sqlx::FromRow` is
/// only derived when the project actually wires a postgres pool — projects
/// without a database still get usable serializable structs.
fn render_models_module(models: &[Model], has_pg: bool) -> Option<TokenStream> {
    if models.is_empty() {
        return None;
    }
    let structs = models.iter().map(|m| render_model_struct(m, has_pg));
    Some(quote! {
        pub mod models {
            #(#structs)*
        }
    })
}

fn render_model_struct(model: &Model, has_pg: bool) -> TokenStream {
    let name = format_ident!("{}", model.name);
    let fields = model.fields.iter().map(|(field_name, def)| {
        let ident = format_ident!("{}", field_name);
        let ty = model_field_type(def.ty, def.nullable);
        quote! { pub #ident: #ty, }
    });
    let from_row = has_pg.then(|| quote! { , sqlx::FromRow });
    // `Default` is required so page-context structs can mint a fully default
    // instance until process-block execution lands (slice 5). Every supported
    // FieldType has a Default impl, including uuid::Uuid (nil) and chrono::DateTime<Utc>.
    quote! {
        #[derive(Debug, Clone, Default, serde::Serialize #from_row)]
        pub struct #name {
            #(#fields)*
        }
    }
}

fn model_field_type(ty: FieldType, nullable: bool) -> TokenStream {
    let base = match ty {
        FieldType::Uuid => quote! { uuid::Uuid },
        FieldType::String | FieldType::Text | FieldType::Email => quote! { String },
        FieldType::Int => quote! { i32 },
        FieldType::Bigint => quote! { i64 },
        FieldType::Bool => quote! { bool },
        FieldType::Timestamptz => quote! { chrono::DateTime<chrono::Utc> },
    };
    if nullable {
        // `Option<T>` doesn't impl `Display`, so Askama refuses to print it
        // directly. Wrap nullable fields in a tiny newtype that delegates
        // serde + sqlx to the inner Option but renders as the inner
        // `Display` (or empty) for templates. See `render_rb_util_module`.
        quote! { crate::_rb_util::NullDisplay<#base> }
    } else {
        base
    }
}

/// True when at least one model declares a nullable field. Gates the
/// emission of the `_rb_util` helper module: there's no point shipping
/// `NullDisplay<T>` when no field uses it.
fn has_nullable_model_field(models: &[Model]) -> bool {
    models.iter().any(|m| m.fields.values().any(|f| f.nullable))
}

/// Emit the `pub mod _rb_util { ... }` helper module used by nullable model
/// fields. Bound to one purpose only: bridge `Option<T>` to Askama's
/// `Display`-based interpolation without leaking through serde/sqlx.
///
/// The sqlx impls are conditional on the dist actually wiring a Postgres
/// pool — projects without a database don't need them.
fn render_rb_util_module(models: &[Model], has_pg: bool) -> Option<TokenStream> {
    if !has_nullable_model_field(models) {
        return None;
    }
    let sqlx_impls = has_pg.then(|| {
        quote! {
            impl<'r, T> sqlx::Decode<'r, sqlx::Postgres> for NullDisplay<T>
            where
                Option<T>: sqlx::Decode<'r, sqlx::Postgres>,
            {
                fn decode(
                    value: <sqlx::Postgres as sqlx::Database>::ValueRef<'r>,
                ) -> std::result::Result<Self, sqlx::error::BoxDynError> {
                    <Option<T> as sqlx::Decode<'r, sqlx::Postgres>>::decode(value).map(NullDisplay)
                }
            }

            impl<T> sqlx::Type<sqlx::Postgres> for NullDisplay<T>
            where
                Option<T>: sqlx::Type<sqlx::Postgres>,
            {
                fn type_info() -> sqlx::postgres::PgTypeInfo {
                    <Option<T> as sqlx::Type<sqlx::Postgres>>::type_info()
                }
                fn compatible(ty: &sqlx::postgres::PgTypeInfo) -> bool {
                    <Option<T> as sqlx::Type<sqlx::Postgres>>::compatible(ty)
                }
            }
        }
    });

    Some(quote! {
        pub mod _rb_util {
            #[derive(Debug, Clone, Default)]
            pub struct NullDisplay<T>(pub Option<T>);

            impl<T: std::fmt::Display> std::fmt::Display for NullDisplay<T> {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    match &self.0 {
                        Some(v) => v.fmt(f),
                        None => Ok(()),
                    }
                }
            }

            impl<T: serde::Serialize> serde::Serialize for NullDisplay<T> {
                fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                    self.0.serialize(s)
                }
            }

            #sqlx_impls
        }
    })
}

/// Wire one `Route` into the generated Axum router.
///
/// Slice 1 ignores everything past `path`/`method`/`kind` — handlers are
/// stubs that announce their identity. Subsequent slices flesh out template
/// rendering, input parsing, db access, and view/output mapping.
fn render_route_registration(route: &Route) -> TokenStream {
    let path = axum_path(&route.path);
    let handler = format_ident!("route_{}", route.name);
    let method = method_ident(route.method);
    quote! {
        router = router.route(#path, #method(#handler));
    }
}

fn render_route_handler(route: &Route, layouts: &[Layout], models: &[Model]) -> TokenStream {
    // GET pages with a template get the full Askama treatment. Everything
    // else (POST/PUT/... pages, every API route) keeps the slice-1 stub —
    // those handlers belong to later slices.
    if route.kind == RouteKind::Page && route.method == HttpMethod::Get && route.template.is_some()
    {
        return render_page_route(route, layouts, models);
    }
    let handler = format_ident!("route_{}", route.name);
    let label = match route.kind {
        RouteKind::Page => format!("rublocks: page route `{}` not yet rendered", route.path),
        RouteKind::Api => format!("rublocks: api route `{}` not yet implemented", route.path),
    };
    quote! {
        async fn #handler() -> &'static str {
            #label
        }
    }
}

/// Emit a typed Askama context + Axum handler for one `kind: page` GET route.
///
/// The struct lives in a `ctx_<name>` sibling module so the handler can keep
/// its short `route_<name>` name (matching the slice-1 naming and the router
/// registration). Fields come from the union of the layout's `requires` and
/// `view`, plus the route's `view` (which wins on conflict). Literal view
/// values are baked in; everything else falls through `Default::default()`.
fn render_page_route(route: &Route, layouts: &[Layout], models: &[Model]) -> TokenStream {
    let handler = format_ident!("route_{}", route.name);
    let ctx_mod = format_ident!("ctx_{}", route.name);
    let template_path = route.template.as_deref().unwrap_or_default();

    let fields = page_context_fields(route, layouts, models);
    let field_defs = fields.iter().map(|f| {
        let ident = format_ident!("{}", f.name);
        let ty = &f.ty;
        quote! { pub #ident: #ty, }
    });
    let literal_assigns = fields.iter().filter_map(|f| {
        let lit = f.literal.as_ref()?;
        let ident = format_ident!("{}", f.name);
        Some(quote! { #ident: #lit.to_string(), })
    });

    quote! {
        pub mod #ctx_mod {
            #[derive(askama::Template, Default)]
            #[template(path = #template_path)]
            pub struct PageContext {
                #(#field_defs)*
            }
        }

        async fn #handler() -> axum::response::Html<String> {
            use askama::Template as _;
            let ctx = #ctx_mod::PageContext {
                #(#literal_assigns)*
                ..Default::default()
            };
            let rendered = ctx
                .render()
                .unwrap_or_else(|e| format!("rublocks: template render error: {e}"));
            axum::response::Html(maybe_inject_dev_snippet(rendered))
        }
    }
}

/// One context-struct field, ready to embed in the generated module.
struct ContextField {
    name: String,
    ty: TokenStream,
    /// Literal value declared in route.view (e.g. `"Recent posts"`); when set,
    /// the handler initializes the field with `.to_string()` so the page text
    /// shows up immediately, even before slice 5 ships full view mapping.
    literal: Option<String>,
}

fn page_context_fields(route: &Route, layouts: &[Layout], models: &[Model]) -> Vec<ContextField> {
    let mut fields: IndexMap<String, ContextField> = IndexMap::new();

    if let Some(layout_name) = &route.layout
        && let Some(layout) = Layout::find(layouts, layout_name)
    {
        for (k, req) in &layout.requires {
            let ty = match req.ty {
                RequireType::String => quote! { String },
            };
            fields.insert(
                k.clone(),
                ContextField {
                    name: k.clone(),
                    ty,
                    literal: None,
                },
            );
        }
        for (k, v) in &layout.view {
            let entry = ContextField {
                name: k.clone(),
                ty: infer_view_type(v, &layout.process, models),
                literal: view_literal(v),
            };
            fields.entry(k.clone()).or_insert(entry);
        }
    }

    for (k, v) in &route.view {
        fields.insert(
            k.clone(),
            ContextField {
                name: k.clone(),
                ty: infer_view_type(v, &route.process, models),
                literal: view_literal(v),
            },
        );
    }

    fields.into_iter().map(|(_, v)| v).collect()
}

/// Best-effort type inference for a view binding.
///
/// `$<name>` references with a known `db.find_many` / `db.find_one` process
/// block resolve to `Vec<models::T>` / `models::T`. Field access (`$x.y`)
/// and unrecognized references fall back to `String` — the template just
/// renders them via `Display`, and slice 5 will fill in real values.
fn infer_view_type(value: &str, processes: &[ProcessBlock], models: &[Model]) -> TokenStream {
    let Some(rest) = value.strip_prefix('$') else {
        return quote! { String };
    };
    let (head, has_field) = match rest.split_once('.') {
        Some((h, _)) => (h, true),
        None => (rest, false),
    };
    if has_field {
        return quote! { String };
    }
    let Some(block) = processes.iter().find(|p| p.name.as_deref() == Some(head)) else {
        return quote! { String };
    };
    let Some(table) = block.table.as_deref() else {
        return quote! { String };
    };
    let Some(model) = models.iter().find(|m| m.table == table) else {
        return quote! { String };
    };
    let ident = format_ident!("{}", model.name);
    match block.block.as_str() {
        "db.find_many" => quote! { Vec<crate::models::#ident> },
        "db.find_one" => quote! { crate::models::#ident },
        _ => quote! { String },
    }
}

/// Pull the literal string out of a view value, if any. `$<ref>` values
/// return `None` — those come from process blocks, not from the route file.
fn view_literal(value: &str) -> Option<String> {
    if value.starts_with('$') {
        None
    } else {
        Some(value.to_string())
    }
}

fn method_ident(method: HttpMethod) -> proc_macro2::Ident {
    format_ident!("{}", method_name(method))
}

fn method_name(method: HttpMethod) -> &'static str {
    match method {
        HttpMethod::Get => "get",
        HttpMethod::Post => "post",
        HttpMethod::Put => "put",
        HttpMethod::Delete => "delete",
        HttpMethod::Patch => "patch",
    }
}

/// Build the `axum::routing::{...}` import list, including only methods the
/// generated code actually references. `get` is always present (`/health`).
fn used_method_imports(routes: &[Route]) -> Vec<proc_macro2::Ident> {
    let mut names: std::collections::BTreeSet<&'static str> = std::collections::BTreeSet::new();
    names.insert("get");
    for r in routes {
        names.insert(method_name(r.method));
    }
    names.into_iter().map(|n| format_ident!("{}", n)).collect()
}

/// Translate a `ServiceUrl` into the Rust expression used at startup.
///
/// `Literal` is embedded as-is; `Env` becomes a `std::env::var(...)?` call,
/// so the binary fails fast at startup if the variable is unset.
fn url_expr(url: &ServiceUrl) -> TokenStream {
    match url {
        ServiceUrl::Literal(s) => quote! { #s.to_string() },
        ServiceUrl::Env(var) => quote! { std::env::var(#var)? },
    }
}

/// Render the dev-mode HTML demo page.
///
/// This page is a placeholder served at `GET /` whenever `RUBLOCKS_DEV=1`.
/// It exists so the user has something to load in a browser before any
/// user-defined routes exist, exercising the livereload pipeline end-to-end.
fn render_dev_index_html(app_name: &str) -> String {
    format!(
        "<!DOCTYPE html>\n\
         <html lang=\"en\">\n\
         <head>\n  \
           <meta charset=\"utf-8\">\n  \
           <title>{app_name} \u{2014} rublocks dev</title>\n  \
           <script src=\"/__rublocks/livereload.js\"></script>\n\
         </head>\n\
         <body style=\"font-family: system-ui, sans-serif; max-width: 40rem; margin: 4rem auto; color: #222;\">\n  \
           <h1 style=\"margin-bottom: 0.25rem;\">{app_name}</h1>\n  \
           <p style=\"color: #666; margin-top: 0;\">rublocks dev mode</p>\n  \
           <p>Edit <code>main.json</code> and save \u{2014} this page will reload automatically.</p>\n\
         </body>\n\
         </html>\n"
    )
}

/// Browser-side livereload snippet, embedded as a constant in the generated
/// project.
///
/// Protocol: open an `EventSource` to `/__rublocks/events`. On the first
/// successful open, just record `everConnected = true`. On disconnect,
/// retry with a 500ms backoff. Once a reconnect succeeds (server came back
/// after restart), call `location.reload()`. The SSE stream itself never
/// emits payload events — the connect/disconnect cycle is the signal.
const LIVERELOAD_JS_SOURCE: &str = r#"(function () {
  let everConnected = false;
  function connect() {
    const es = new EventSource('/__rublocks/events');
    es.onopen = function () {
      if (everConnected) {
        location.reload();
      }
      everConnected = true;
    };
    es.onerror = function () {
      es.close();
      setTimeout(connect, 500);
    };
  }
  connect();
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn manifest_from(project_dir: &Path, main_json: &str) -> Manifest {
        fs::write(project_dir.join("main.json"), main_json).unwrap();
        Manifest::load(project_dir).expect("manifest")
    }

    #[test]
    fn model_field_type_maps_known_scalars() {
        assert_eq!(
            model_field_type(FieldType::Uuid, false).to_string(),
            "uuid :: Uuid"
        );
        assert_eq!(
            model_field_type(FieldType::String, false).to_string(),
            "String"
        );
        assert_eq!(
            model_field_type(FieldType::Text, false).to_string(),
            "String"
        );
        assert_eq!(
            model_field_type(FieldType::Email, false).to_string(),
            "String"
        );
        assert_eq!(model_field_type(FieldType::Int, false).to_string(), "i32");
        assert_eq!(
            model_field_type(FieldType::Bigint, false).to_string(),
            "i64"
        );
        assert_eq!(model_field_type(FieldType::Bool, false).to_string(), "bool");
        assert_eq!(
            model_field_type(FieldType::Timestamptz, false).to_string(),
            "chrono :: DateTime < chrono :: Utc >"
        );
    }

    #[test]
    fn model_field_type_wraps_nullable_in_null_display() {
        let ts = model_field_type(FieldType::String, true).to_string();
        assert_eq!(ts, "crate :: _rb_util :: NullDisplay < String >");
    }

    #[test]
    fn used_method_imports_always_includes_get() {
        let imports = used_method_imports(&[]);
        let names: Vec<String> = imports.iter().map(|i| i.to_string()).collect();
        assert_eq!(names, vec!["get".to_string()]);
    }

    #[test]
    fn used_method_imports_deduplicates_and_sorts() {
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("a.json"),
            r#"{"path":"/a","method":"POST","kind":"page","template":"x.html"}"#,
        )
        .unwrap();
        fs::write(
            routes_dir.join("b.json"),
            r#"{"path":"/b","method":"POST","kind":"api"}"#,
        )
        .unwrap();
        fs::write(
            routes_dir.join("c.json"),
            r#"{"path":"/c","method":"DELETE","kind":"api"}"#,
        )
        .unwrap();
        let routes = Route::load_all(dir.path()).unwrap();
        let imports = used_method_imports(&routes);
        let names: Vec<String> = imports.iter().map(|i| i.to_string()).collect();
        assert_eq!(
            names,
            vec!["delete".to_string(), "get".to_string(), "post".to_string()]
        );
    }

    #[test]
    fn emit_produces_parseable_main_rs() {
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("home.json"),
            r#"{"path":"/","method":"GET","kind":"page","template":"home.html"}"#,
        )
        .unwrap();
        let manifest = manifest_from(dir.path(), r#"{ "name": "rb_test" }"#);

        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();

        // Generated code must be syntactically valid Rust.
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");

        // Slice 1: route registration + handler stubs.
        assert!(main_rs.contains(r#"router.route("/", get(route_home))"#));
        assert!(main_rs.contains("async fn route_home"));
        // No user route owns /health, but the placeholder for / is suppressed
        // because the user does own /.
        assert!(!main_rs.contains("dev_index"));
    }

    #[test]
    fn emit_emits_dev_index_when_no_user_route_owns_root() {
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(dir.path(), r#"{ "name": "rb_test" }"#);
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        assert!(main_rs.contains("async fn dev_index"));
    }

    #[test]
    fn emit_inlines_model_structs_with_field_order() {
        let dir = TempDir::new().unwrap();
        let models_dir = dir.path().join("models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("post.json"),
            r#"{
                "name": "Post",
                "table": "posts",
                "fields": {
                    "id":    { "type": "uuid" },
                    "title": { "type": "string" }
                }
            }"#,
        )
        .unwrap();
        let manifest = manifest_from(dir.path(), r#"{ "name": "rb_test" }"#);
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();

        let _: syn::File = syn::parse_str(&main_rs).expect("must parse");
        assert!(main_rs.contains("pub mod models"));
        assert!(main_rs.contains("pub struct Post"));
        let id_pos = main_rs.find("pub id").expect("id field present");
        let title_pos = main_rs.find("pub title").expect("title field present");
        assert!(id_pos < title_pos, "id must come before title");
    }

    #[test]
    fn cargo_toml_omits_uuid_when_no_model_uses_it() {
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(dir.path(), r#"{ "name": "rb_test" }"#);
        let toml = render_cargo_toml(&manifest);
        assert!(!toml.contains("uuid"));
        assert!(!toml.contains("chrono"));
    }

    #[test]
    fn emit_generates_askama_context_for_page_route() {
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("home.json"),
            r#"{
                "path": "/",
                "method": "GET",
                "kind": "page",
                "template": "home.html",
                "layout": "main",
                "process": [
                    { "name": "posts", "block": "db.find_many", "table": "posts" }
                ],
                "view": { "page_title": "Recent posts", "posts": "$posts" }
            }"#,
        )
        .unwrap();
        let layouts_dir = dir.path().join("layouts");
        fs::create_dir_all(&layouts_dir).unwrap();
        fs::write(
            layouts_dir.join("main.json"),
            r#"{
                "name": "main",
                "template": "layout.html",
                "requires": { "page_title": { "type": "string" } },
                "view": { "current_year": "$year" }
            }"#,
        )
        .unwrap();
        let models_dir = dir.path().join("models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("post.json"),
            r#"{
                "name": "Post",
                "table": "posts",
                "fields": { "id": { "type": "uuid" }, "title": { "type": "string" } }
            }"#,
        )
        .unwrap();

        let manifest = manifest_from(dir.path(), r#"{ "name": "rb_test" }"#);
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");

        // The page handler is wired with an Askama-derived context struct.
        assert!(main_rs.contains("pub mod ctx_home"));
        assert!(main_rs.contains(r#"#[template(path = "home.html")]"#));
        assert!(main_rs.contains("pub struct PageContext"));
        // Layout requires + layout view + route view all surface as fields.
        assert!(main_rs.contains("pub page_title: String"));
        assert!(main_rs.contains("pub current_year: String"));
        assert!(main_rs.contains("pub posts: Vec<crate::models::Post>"));
        // Literal view values are baked into the handler's context init.
        assert!(main_rs.contains(r#"page_title: "Recent posts".to_string(),"#));
        // Handler returns Html<String> rendered via Askama, with livereload
        // injection wrapping the rendered template.
        assert!(main_rs.contains("axum::response::Html"));
        assert!(main_rs.contains("ctx.render()") || main_rs.contains("ctx\n        .render()"));
        assert!(main_rs.contains("maybe_inject_dev_snippet"));
    }

    #[test]
    fn emit_copies_templates_dir_into_dist() {
        let dir = TempDir::new().unwrap();
        let templates = dir.path().join("templates");
        fs::create_dir_all(templates.join("posts")).unwrap();
        fs::write(templates.join("home.html"), "<h1>home</h1>").unwrap();
        fs::write(templates.join("posts/show.html"), "<h2>show</h2>").unwrap();
        let manifest = manifest_from(dir.path(), r#"{ "name": "rb_test" }"#);
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        assert!(dist.join("templates/home.html").is_file());
        assert!(dist.join("templates/posts/show.html").is_file());
    }

    #[test]
    fn cargo_toml_pulls_in_askama_when_page_route_present() {
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("home.json"),
            r#"{"path":"/","method":"GET","kind":"page","template":"home.html"}"#,
        )
        .unwrap();
        let manifest = manifest_from(dir.path(), r#"{ "name": "rb_test" }"#);
        let toml = render_cargo_toml(&manifest);
        assert!(toml.contains("askama"));
    }

    #[test]
    fn cargo_toml_omits_askama_for_api_only_projects() {
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("posts.json"),
            r#"{"path":"/api/posts","method":"GET","kind":"api"}"#,
        )
        .unwrap();
        let manifest = manifest_from(dir.path(), r#"{ "name": "rb_test" }"#);
        let toml = render_cargo_toml(&manifest);
        assert!(!toml.contains("askama"));
    }

    #[test]
    fn manifest_load_rejects_route_referencing_unknown_layout() {
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("home.json"),
            r#"{"path":"/","method":"GET","kind":"page","template":"home.html","layout":"nope"}"#,
        )
        .unwrap();
        fs::write(dir.path().join("main.json"), r#"{ "name": "rb_test" }"#).unwrap();
        let err = Manifest::load(dir.path()).unwrap_err();
        assert_eq!(err.file, routes_dir.join("home.json"));
        assert!(
            err.message.contains("layout `nope`"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn cargo_toml_pulls_in_only_needed_features() {
        let dir = TempDir::new().unwrap();
        let models_dir = dir.path().join("models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("a.json"),
            r#"{
                "name": "A",
                "table": "a",
                "fields": { "id": { "type": "uuid" }, "name": { "type": "string" } }
            }"#,
        )
        .unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "services": { "postgres": { "url": "env:DATABASE_URL" } } }"#,
        );
        let toml = render_cargo_toml(&manifest);
        assert!(toml.contains("uuid"));
        assert!(!toml.contains("chrono"));
        assert!(toml.contains("sqlx"));
        assert!(toml.contains("\"derive\""));
        assert!(toml.contains("\"uuid\""));
    }
}
