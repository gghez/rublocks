//! Rust source generation for the target Axum project.
//!
//! Code is built as a `proc_macro2::TokenStream` via `quote!`, parsed with
//! `syn`, and pretty-printed with `prettyplease`. This guarantees
//! syntactically valid, well-formatted output and avoids the fragility of
//! string templates. `Cargo.toml` is the only string-template exception
//! (TOML has no quote-equivalent).
//!
//! See `docs/architecture.md` and `docs/decisions.md`.

use crate::blocks::BlockInstance;
use crate::codegen_input;
use crate::layouts::{Layout, RequireType};
use crate::manifest::{DbKind, HttpConfig, Manifest, ServiceUrl};
use crate::migrations;
use crate::models::{FieldType, Model};
use crate::routes::{HttpMethod, Route, RouteKind, axum_path};
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

    let has_migrations = migrations::has_migration_files(project_dir);
    fs::write(
        dist_dir.join("Cargo.toml"),
        render_cargo_toml(manifest, has_migrations),
    )?;
    fs::write(
        dist_dir.join("src").join("main.rs"),
        render_main_rs(manifest, has_migrations)?,
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

/// True when at least one route declares a `guard` block — drives the
/// emission of the `_rb_runtime` module hosting the 403 helpers.
fn project_uses_guards(routes: &[Route]) -> bool {
    routes
        .iter()
        .any(|r| r.process.iter().any(|b| b.guard_if().is_some()))
}

/// Emit the dist-side `_rb_runtime` module — currently just the 403
/// response builders for the `guard` block. Page and API routes get
/// distinct shapes so the response is appropriate to the route kind.
fn render_rb_runtime_module() -> TokenStream {
    quote! {
        pub mod _rb_runtime {
            /// JSON `403 Forbidden` for `kind: api` routes. Body is a
            /// fixed `{"error":{"code":"forbidden"}}` to keep the dist
            /// dependency surface minimal (no serde_json::json! needed).
            pub fn api_403() -> axum::response::Response {
                use axum::response::IntoResponse as _;
                (
                    axum::http::StatusCode::FORBIDDEN,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    r#"{"error":{"code":"forbidden"}}"#,
                )
                    .into_response()
            }

            /// Plain-text `403 Forbidden` for `kind: page` routes. The
            /// full template-rendered surface (with `$errors` access)
            /// lands once process-block execution is wired.
            pub fn page_403() -> axum::response::Response {
                use axum::response::IntoResponse as _;
                (axum::http::StatusCode::FORBIDDEN, "403 Forbidden").into_response()
            }
        }
    }
}

/// Emit the guard-evaluation prelude for one route: build a CEL context
/// from the route's input fields, then for each `guard` block in the
/// process pipeline compile + cache + execute its `if` expression.
/// `Bool(true)` ⇒ pass; anything else short-circuits with `403`.
///
/// Returns `None` when the route has no guards so the handler body
/// stays free of the prelude.
fn render_guards(route: &Route) -> Option<TokenStream> {
    let guards: Vec<(usize, &str)> = route
        .process
        .iter()
        .enumerate()
        .filter_map(|(i, b)| b.guard_if().map(|expr| (i, expr)))
        .collect();
    if guards.is_empty() {
        return None;
    }
    let context = codegen_input::render_input_cel_bindings(route);
    let name_upper = route.name.to_uppercase();
    let forbidden_call = match route.kind {
        RouteKind::Api => quote! { return crate::_rb_runtime::api_403(); },
        RouteKind::Page => quote! { return crate::_rb_runtime::page_403(); },
    };
    let checks = guards.into_iter().map(|(i, expr)| {
        let prog_ident = format_ident!("__RB_GUARD_{}_{}", name_upper, i);
        let label = format!("process[{i}].if");
        quote! {
            {
                static #prog_ident: std::sync::OnceLock<cel_interpreter::Program> =
                    std::sync::OnceLock::new();
                let __prog = #prog_ident.get_or_init(|| {
                    cel_interpreter::Program::compile(#expr)
                        .expect("CEL was syntax-checked at build time")
                });
                let __pass = matches!(
                    __prog.execute(&__ctx),
                    Ok(cel_interpreter::Value::Bool(true)),
                );
                if !__pass {
                    let _ = #label;
                    #forbidden_call
                }
            }
        }
    });
    Some(quote! {
        let mut __ctx = cel_interpreter::Context::default();
        #context
        #(#checks)*
    })
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
fn render_cargo_toml(manifest: &Manifest, has_migrations: bool) -> String {
    let mut deps = String::from(
        "axum = \"0.8\"\n\
         tokio = { version = \"1\", features = [\"macros\", \"rt-multi-thread\"] }\n\
         anyhow = \"1\"\n\
         futures-util = \"0.3\"\n",
    );
    // `serde` is needed by both models (FromRow + Serialize derive) and the
    // input validator (Deserialize on extractor structs). Emitted once.
    let need_serde =
        !manifest.models.is_empty() || codegen_input::project_uses_input(&manifest.routes);
    if need_serde {
        deps.push_str("serde = { version = \"1\", features = [\"derive\"] }\n");
    }
    if !manifest.models.is_empty() {
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
    if let Some(db) = manifest.database.as_ref() {
        let backend_feat = sqlx_backend_feature(db.kind);
        let mut feats = vec!["runtime-tokio", "tls-rustls", backend_feat];
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
        // `sqlx::migrate!` is exported by `sqlx-macros`, gated by both the
        // `macros` and `migrate` features. Match the gate that emits the
        // `rb_migrate_*` helpers so the dist crate links cleanly.
        if has_migrations {
            feats.push("macros");
            feats.push("migrate");
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
    // Input validator wiring. `serde_json` powers the 400 JSON body the
    // dist-side `_rb_input::api_400` helper emits; `regex` enforces every
    // declared `pattern` constraint and is only pulled when at least one
    // route declares one.
    if codegen_input::project_uses_input(&manifest.routes) {
        deps.push_str("serde_json = \"1\"\n");
    }
    for extra in codegen_input::cargo_dependencies(&manifest.routes) {
        deps.push_str(extra);
    }
    // Routes that need uuid/chrono in their extractor structs add the
    // crate even when no model uses the type. Idempotent: we only push
    // the line when missing from the buffer so far.
    let any_input_field_uses = |kind: crate::input::FieldKind| -> bool {
        manifest.routes.iter().any(|r| {
            r.input.as_ref().is_some_and(|s| {
                let in_map = |m: &indexmap::IndexMap<String, crate::input::FieldSpec>| {
                    m.values().any(|f| f.ty == kind)
                };
                in_map(&s.path)
                    || in_map(&s.query)
                    || s.body.as_ref().is_some_and(|b| in_map(&b.fields))
            })
        })
    };
    if any_input_field_uses(crate::input::FieldKind::Uuid) && !deps.contains("uuid =") {
        deps.push_str("uuid = { version = \"1\", features = [\"serde\"] }\n");
    }
    if any_input_field_uses(crate::input::FieldKind::Timestamptz) && !deps.contains("chrono =") {
        deps.push_str("chrono = { version = \"0.4\", features = [\"serde\"] }\n");
    }
    // CEL runtime. Programs are evaluated at request time for the `guard`
    // block (→ 403 on false) and for every `validate` field expression
    // (→ 422 entry on false). Each compiled `Program` is cached in a
    // `OnceLock` so the parse cost is paid once per process.
    if crate::expressions::project_uses_cel(&manifest.routes, &manifest.models) {
        deps.push_str("cel-interpreter = { version = \"0.10\", default-features = false }\n");
    }
    // `X-App-Version` is stamped on every response, so `tower-http` with the
    // `set-header` feature is always pulled in. Anything the user declared
    // under `http.*` adds its features on top. `http` is always needed for
    // the `HeaderName::from_static` call site used by the version stamp.
    let mut tower_feats: Vec<&'static str> = vec!["set-header"];
    if let Some(http) = manifest.http.as_ref() {
        for extra in tower_http_features(http) {
            if !tower_feats.contains(&extra) {
                tower_feats.push(extra);
            }
        }
    }
    let feats_str = tower_feats
        .iter()
        .map(|f| format!("\"{f}\""))
        .collect::<Vec<_>>()
        .join(", ");
    deps.push_str(&format!(
        "tower-http = {{ version = \"0.6\", features = [{feats_str}] }}\n",
    ));
    deps.push_str("http = \"1\"\n");

    // Cargo expects a TOML string — embed the manifest description as a
    // properly escaped basic string so quotes/backslashes round-trip safely.
    let description = toml_escape_basic_string(&manifest.description);
    format!(
        "# Generated by rublocks. Do not edit by hand.\n\
         [package]\n\
         name = \"{name}\"\n\
         version = \"{version}\"\n\
         description = \"{description}\"\n\
         edition = \"2024\"\n\
         \n\
         [dependencies]\n\
         {deps}",
        name = manifest.name,
        version = manifest.version,
    )
}

/// Escape a string for use inside a TOML basic string (double-quoted).
///
/// Manifest validation already rejects newlines, but quotes and backslashes
/// stay legal and must be escaped so the generated `Cargo.toml` parses.
fn toml_escape_basic_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            other => out.push(other),
        }
    }
    out
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
fn render_main_rs(manifest: &Manifest, has_migrations: bool) -> Result<String> {
    let database = manifest.database.as_ref();
    let has_pg = database.is_some();
    let has_redis = manifest.services.redis.is_some();
    // sqlx::migrate! is only wired when there is at least one SQL file to
    // read — the macro fails at compile time on an empty directory, and
    // shipping a `migrate` subcommand with nothing to apply is confusing.
    let wire_migrations = has_pg && has_migrations;
    let pool_ty = database.map(|d| sqlx_pool_type(d.kind));

    let pg_field = pool_ty.as_ref().map(|ty| quote! { pub pg: #ty, });
    let redis_field = has_redis.then(|| quote! { pub redis: deadpool_redis::Pool, });

    let pg_init = database.map(|db| {
        let url = url_expr(&db.url);
        let ty = sqlx_pool_type(db.kind);
        quote! {
            let pg = #ty::connect(&#url).await?;
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
        let html = render_dev_index_html(&manifest.name, &manifest.description);
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
    let rb_util_module = render_rb_util_module(&manifest.models, database.map(|d| d.kind));
    let rb_input_module = codegen_input::project_uses_input(&manifest.routes)
        .then(codegen_input::render_rb_input_module);
    let rb_runtime_module = project_uses_guards(&manifest.routes).then(render_rb_runtime_module);
    let input_modules = manifest
        .routes
        .iter()
        .filter_map(codegen_input::render_per_route_module);
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

    // CLI dispatch: the dist binary recognizes `<bin> migrate [--list]` as a
    // one-shot subcommand and exits, instead of starting the HTTP server.
    let cli_dispatch = wire_migrations.then(|| {
        quote! {
            let args: Vec<String> = std::env::args().collect();
            if args.get(1).map(|s| s.as_str()) == Some("migrate") {
                let list = args.iter().skip(2).any(|a| a == "--list");
                if list {
                    rb_migrate_list(&pg).await?;
                } else {
                    let applied = rb_migrate_run(&pg).await?;
                    println!("rublocks: {applied} migration(s) applied");
                }
                return Ok(());
            }
            // In dev mode, apply any pending migrations on startup so the
            // browser-driven authoring loop doesn't require a manual step.
            if std::env::var("RUBLOCKS_DEV").is_ok() {
                let applied = rb_migrate_run(&pg).await?;
                if applied > 0 {
                    eprintln!("rublocks: applied {applied} pending migration(s)");
                }
            }
        }
    });

    let http_layer = render_http_layer(manifest.http.as_ref(), &manifest.version);

    let migrate_helpers = wire_migrations.then(|| {
        let pool_ty = pool_ty
            .clone()
            .expect("wire_migrations implies a database, hence a pool type");
        quote! {
            /// Apply every pending migration in `./migrations`. Returns the
            /// number of newly applied files; idempotent across restarts.
            async fn rb_migrate_run(pool: &#pool_ty) -> anyhow::Result<usize> {
                let before = rb_applied_versions(pool).await.unwrap_or_default().len();
                sqlx::migrate!("./migrations").run(pool).await?;
                let after = rb_applied_versions(pool).await.unwrap_or_default().len();
                Ok(after.saturating_sub(before))
            }

            /// Print every migration the binary knows about with its current
            /// state — "applied" or "pending". Tolerant of a missing
            /// `_sqlx_migrations` table (treated as "nothing applied yet").
            async fn rb_migrate_list(pool: &#pool_ty) -> anyhow::Result<()> {
                let applied: std::collections::HashSet<i64> =
                    rb_applied_versions(pool).await.unwrap_or_default().into_iter().collect();
                for m in sqlx::migrate!("./migrations").iter() {
                    let state = if applied.contains(&(m.version as i64)) {
                        "applied"
                    } else {
                        "pending"
                    };
                    println!("{:>10}  {:>8}  {}", m.version, state, m.description);
                }
                Ok(())
            }

            async fn rb_applied_versions(pool: &#pool_ty) -> anyhow::Result<Vec<i64>> {
                let rows: Result<Vec<i64>, _> = sqlx::query_scalar(
                    "SELECT version FROM _sqlx_migrations ORDER BY version",
                )
                .fetch_all(pool)
                .await;
                Ok(rows.unwrap_or_default())
            }
        }
    });

    let tokens = quote! {
        use axum::{routing::{#(#method_imports),*}, Router};

        #rb_util_module

        #rb_input_module

        #rb_runtime_module

        #(#input_modules)*

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

            #cli_dispatch

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

            #http_layer

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

        #migrate_helpers

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
/// The sqlx impls are gated on the dist actually wiring a database pool —
/// projects without a database don't need them — and on the kind so the
/// Decode/Type impls reference the right backend type.
fn render_rb_util_module(models: &[Model], db_kind: Option<DbKind>) -> Option<TokenStream> {
    if !has_nullable_model_field(models) {
        return None;
    }
    let sqlx_impls = db_kind.and_then(|kind| {
        let (db_ty, type_info) = match kind {
            DbKind::Postgres => (
                quote! { sqlx::Postgres },
                quote! { sqlx::postgres::PgTypeInfo },
            ),
            DbKind::Mysql | DbKind::Mariadb => (
                quote! { sqlx::MySql },
                quote! { sqlx::mysql::MySqlTypeInfo },
            ),
            // MSSQL: skip — sqlx 0.8 dropped the driver, the wider build
            // already fails at the sqlx dependency stage.
            DbKind::Mssql => return None,
        };
        Some(quote! {
            impl<'r, T> sqlx::Decode<'r, #db_ty> for NullDisplay<T>
            where
                Option<T>: sqlx::Decode<'r, #db_ty>,
            {
                fn decode(
                    value: <#db_ty as sqlx::Database>::ValueRef<'r>,
                ) -> std::result::Result<Self, sqlx::error::BoxDynError> {
                    <Option<T> as sqlx::Decode<'r, #db_ty>>::decode(value).map(NullDisplay)
                }
            }

            impl<T> sqlx::Type<#db_ty> for NullDisplay<T>
            where
                Option<T>: sqlx::Type<#db_ty>,
            {
                fn type_info() -> #type_info {
                    <Option<T> as sqlx::Type<#db_ty>>::type_info()
                }
                fn compatible(ty: &#type_info) -> bool {
                    <Option<T> as sqlx::Type<#db_ty>>::compatible(ty)
                }
            }
        })
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

    let has_input = route.input.as_ref().is_some_and(|s| !s.is_empty());
    let guards = render_guards(route);
    // The route still has no real body, but if it carries declarative
    // checks (input.validate, guard) the handler must extract, validate
    // and authorize before returning the slice-1 stub message. Switching
    // to `axum::response::Response` keeps both the early-exit branches
    // (422/403) and the late stub branch type-compatible.
    if !has_input && guards.is_none() {
        return quote! {
            async fn #handler() -> &'static str {
                #label
            }
        };
    }
    let extractor_params = codegen_input::handler_extractor_params(route);
    let validation_call = codegen_input::handler_validation_call(route);
    let validation_branch = if has_input {
        let helper = match route.kind {
            RouteKind::Api => quote! { crate::_rb_input::api_422 },
            RouteKind::Page => quote! { crate::_rb_input::page_422_text },
        };
        quote! {
            if !__rb_input_errors.is_empty() {
                return #helper(__rb_input_errors);
            }
        }
    } else {
        quote! {}
    };
    quote! {
        async fn #handler(#extractor_params) -> axum::response::Response {
            use axum::response::IntoResponse as _;
            #validation_call
            #validation_branch
            #guards
            (#label).into_response()
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

    let extractor_params = codegen_input::handler_extractor_params(route);
    let validation_call = codegen_input::handler_validation_call(route);
    let has_input = route.input.as_ref().is_some_and(|s| !s.is_empty());
    let guards = render_guards(route);
    let has_response = has_input || guards.is_some();
    let return_ty = if has_response {
        quote! { axum::response::Response }
    } else {
        quote! { axum::response::Html<String> }
    };
    let validation_branch = if has_input {
        // Auto-generated short-circuit: if any constraint failed, the
        // handler short-circuits with a 422 Unprocessable Content
        // response. Full template re-render with `$errors` / `$input` in
        // the page context lands in a follow-up; the contract — "you
        // declared the input, you get a validator without lifting a
        // finger" — already holds today.
        quote! {
            if !__rb_input_errors.is_empty() {
                return crate::_rb_input::page_422_text(__rb_input_errors);
            }
        }
    } else {
        quote! {}
    };
    let success_wrap = if has_response {
        // Promote Html<String> into the unified Response type so the
        // `Result` shape stays consistent on both branches.
        quote! {
            use axum::response::IntoResponse as _;
            axum::response::Html(maybe_inject_dev_snippet(rendered)).into_response()
        }
    } else {
        quote! { axum::response::Html(maybe_inject_dev_snippet(rendered)) }
    };

    quote! {
        pub mod #ctx_mod {
            #[derive(askama::Template, Default)]
            #[template(path = #template_path)]
            pub struct PageContext {
                #(#field_defs)*
            }
        }

        async fn #handler(#extractor_params) -> #return_ty {
            use askama::Template as _;
            #validation_call
            #validation_branch
            #guards
            let ctx = #ctx_mod::PageContext {
                #(#literal_assigns)*
                ..Default::default()
            };
            let rendered = ctx
                .render()
                .unwrap_or_else(|e| format!("rublocks: template render error: {e}"));
            #success_wrap
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
/// `$<name>` references that resolve to a registered block (via the block's
/// `output_type_tokens` helper) get their typed Rust type; everything else
/// falls back to `String` — the template renders them via `Display`, and the
/// runtime mapping ships once process execution lands.
fn infer_view_type(
    value: &str,
    processes: &[Box<dyn BlockInstance>],
    models: &[Model],
) -> TokenStream {
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
    let Some(block) = processes.iter().find(|p| p.name() == Some(head)) else {
        return quote! { String };
    };
    block
        .output_type(models)
        .unwrap_or_else(|| quote! { String })
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

/// Build the `router = router.layer(...)` block emitted right before
/// `Router::with_state`. Always installs the `X-App-Version` response
/// header (issue #15: the value is the single source of truth declared in
/// `main.json.version`) and then stacks every user-declared `http.*`
/// layer on top.
fn render_http_layer(http: Option<&HttpConfig>, version: &str) -> TokenStream {
    let mut steps: Vec<TokenStream> = Vec::new();

    // `X-App-Version` ships on every response (dev-mode + prod) so the
    // running build can always be identified — handy for cache-busting
    // and bug-report triage. See issue #15.
    steps.push(quote! {
        .layer(tower_http::set_header::SetResponseHeaderLayer::if_not_present(
            http::header::HeaderName::from_static("x-app-version"),
            http::HeaderValue::from_static(#version),
        ))
    });

    if let Some(http) = http {
        if http.compression {
            steps.push(quote! {
                .layer(tower_http::compression::CompressionLayer::new())
            });
        }

        if let Some(cors) = &http.cors {
            let cors_layer = render_cors_layer(&cors.origins);
            steps.push(quote! { .layer(#cors_layer) });
        }

        if let Some(ms) = http.timeout_ms {
            steps.push(quote! {
                .layer(tower_http::timeout::TimeoutLayer::new(
                    std::time::Duration::from_millis(#ms),
                ))
            });
        }

        if http.security_headers {
            steps.push(quote! {
                .layer(tower_http::set_header::SetResponseHeaderLayer::if_not_present(
                    http::header::HeaderName::from_static("x-content-type-options"),
                    http::HeaderValue::from_static("nosniff"),
                ))
                .layer(tower_http::set_header::SetResponseHeaderLayer::if_not_present(
                    http::header::HeaderName::from_static("x-frame-options"),
                    http::HeaderValue::from_static("DENY"),
                ))
                .layer(tower_http::set_header::SetResponseHeaderLayer::if_not_present(
                    http::header::HeaderName::from_static("referrer-policy"),
                    http::HeaderValue::from_static("strict-origin-when-cross-origin"),
                ))
                .layer(tower_http::set_header::SetResponseHeaderLayer::if_not_present(
                    http::header::HeaderName::from_static("strict-transport-security"),
                    http::HeaderValue::from_static("max-age=31536000; includeSubDomains"),
                ))
            });
        }
    }

    quote! {
        let router = router #(#steps)*;
    }
}

fn render_cors_layer(origins: &[String]) -> TokenStream {
    let any = origins.iter().any(|o| o == "*");
    if any {
        return quote! {
            tower_http::cors::CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any)
        };
    }
    let parsed = origins.iter().map(|o| {
        quote! {
            #o.parse::<axum::http::HeaderValue>().expect("valid CORS origin in main.json")
        }
    });
    quote! {
        tower_http::cors::CorsLayer::new()
            .allow_origin([#(#parsed),*])
            .allow_methods(tower_http::cors::Any)
            .allow_headers(tower_http::cors::Any)
    }
}

/// Cargo features needed from `tower-http` to back the declared layers.
fn tower_http_features(http: &HttpConfig) -> Vec<&'static str> {
    let mut feats: Vec<&'static str> = Vec::new();
    if http.compression {
        feats.push("compression-full");
    }
    if http.cors.is_some() {
        feats.push("cors");
    }
    if http.timeout_ms.is_some() {
        feats.push("timeout");
    }
    if http.security_headers {
        feats.push("set-header");
    }
    feats
}

/// The sqlx Cargo feature flag for a given backend.
fn sqlx_backend_feature(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Postgres => "postgres",
        DbKind::Mysql | DbKind::Mariadb => "mysql",
        // MSSQL was dropped from sqlx 0.8; the manifest still accepts the
        // value so codegen can fail with a clearer message than "unknown
        // feature `mssql`" once a pool is actually requested.
        DbKind::Mssql => "mssql",
    }
}

/// The concrete sqlx pool type for a given backend.
fn sqlx_pool_type(kind: DbKind) -> TokenStream {
    match kind {
        DbKind::Postgres => quote! { sqlx::PgPool },
        DbKind::Mysql | DbKind::Mariadb => quote! { sqlx::MySqlPool },
        DbKind::Mssql => quote! { sqlx::MssqlPool },
    }
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
///
/// The manifest `description` is embedded twice: as `<meta name="description">`
/// (every HTML rublocks emits carries the project synopsis for SEO/preview
/// parity) and as the visible subtitle so the user can confirm which project
/// is loaded at a glance.
fn render_dev_index_html(app_name: &str, description: &str) -> String {
    let name = html_escape(app_name);
    let desc = html_escape(description);
    format!(
        "<!DOCTYPE html>\n\
         <html lang=\"en\">\n\
         <head>\n  \
           <meta charset=\"utf-8\">\n  \
           <meta name=\"description\" content=\"{desc}\">\n  \
           <title>{name} \u{2014} rublocks dev</title>\n  \
           <script src=\"/__rublocks/livereload.js\"></script>\n\
         </head>\n\
         <body style=\"font-family: system-ui, sans-serif; max-width: 40rem; margin: 4rem auto; color: #222;\">\n  \
           <h1 style=\"margin-bottom: 0.25rem;\">{name}</h1>\n  \
           <p style=\"color: #666; margin: 0 0 0.5rem;\">{desc}</p>\n  \
           <p style=\"color: #888; margin-top: 0;\">rublocks dev mode</p>\n  \
           <p>Edit <code>main.json</code> and save \u{2014} this page will reload automatically.</p>\n\
         </body>\n\
         </html>\n"
    )
}

/// Minimal HTML escaper for attribute and text contexts.
///
/// Inline duplicate of the `dev_error::escape_html` helper: the dev-mode
/// overlay lives in a separate crate-internal module and re-importing across
/// modules to share five lines would be more friction than the duplication.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
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
    fn emit_wires_guard_block_to_403_at_runtime() {
        // A route with a `guard` block emits (a) the `_rb_runtime`
        // module with `api_403` / `page_403`, (b) the CEL context built
        // from the route's input, (c) a `Bool(true)` check + 403
        // short-circuit. The route kind picks page_403 vs api_403.
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("admin.json"),
            r#"{
                "path": "/admin",
                "method": "GET",
                "kind": "api",
                "input": { "query": { "token": { "type": "string", "required": true } } },
                "process": [
                    { "block": "guard", "if": "token == \"open-sesame\"" }
                ]
            }"#,
        )
        .unwrap();
        let manifest = manifest_from(dir.path(), r#"{ "name": "rb_test", "version": "0.0.0", "description": "test" }"#);
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");
        assert!(
            main_rs.contains("pub mod _rb_runtime"),
            "guard usage must pull in _rb_runtime:\n{main_rs}"
        );
        assert!(
            main_rs.contains("api_403"),
            "api route with guard must wire api_403:\n{main_rs}"
        );
        assert!(
            main_rs.contains("cel_interpreter :: Value :: Bool(true)")
                || main_rs.contains("cel_interpreter::Value::Bool(true)"),
            "guard check must look for Bool(true):\n{main_rs}"
        );
        assert!(
            main_rs.contains("\"token\""),
            "input field `token` must be bound in the CEL context:\n{main_rs}"
        );
        let toml = fs::read_to_string(dist.join("Cargo.toml")).unwrap();
        assert!(
            toml.contains("cel-interpreter"),
            "Cargo.toml must pull cel-interpreter:\n{toml}"
        );
    }

    #[test]
    fn emit_wires_page_guard_to_page_403() {
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("admin.json"),
            r#"{
                "path": "/admin",
                "method": "GET",
                "kind": "page",
                "template": "admin.html",
                "process": [
                    { "block": "guard", "if": "true" }
                ]
            }"#,
        )
        .unwrap();
        let templates = dir.path().join("templates");
        fs::create_dir_all(&templates).unwrap();
        fs::write(templates.join("admin.html"), "<p>admin</p>").unwrap();
        let manifest = manifest_from(dir.path(), r#"{ "name": "rb_test", "version": "0.0.0", "description": "test" }"#);
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");
        assert!(
            main_rs.contains("page_403"),
            "page route with guard must wire page_403:\n{main_rs}"
        );
    }

    #[test]
    fn emit_wires_cel_runtime_for_input_validate() {
        // A route with an `input.<field>.validate` CEL expression emits
        // (a) a static `OnceLock<cel_interpreter::Program>` per site,
        // (b) a `Context::default()` build with the field bound by name,
        // (c) a 422 push on non-`Bool(true)` results. The pipeline must
        // produce syntactically valid Rust.
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("post.json"),
            r#"{
                "path": "/posts",
                "method": "POST",
                "kind": "api",
                "input": {
                    "body": {
                        "title": { "type": "string", "required": true, "validate": "title.size() > 3" }
                    }
                }
            }"#,
        )
        .unwrap();
        let manifest = manifest_from(dir.path(), r#"{ "name": "rb_test", "version": "0.0.0", "description": "test" }"#);
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");
        assert!(
            main_rs.contains("cel_interpreter::Program::compile"),
            "validate must invoke cel_interpreter at runtime:\n{main_rs}"
        );
        assert!(
            main_rs.contains("OnceLock::<cel_interpreter::Program>")
                || main_rs.contains("OnceLock<cel_interpreter::Program>"),
            "compiled program must be cached in OnceLock:\n{main_rs}"
        );
        assert!(
            main_rs.contains("\"title\""),
            "field must be bound under its declared name in the CEL context:\n{main_rs}"
        );
        let toml = fs::read_to_string(dist.join("Cargo.toml")).unwrap();
        assert!(
            toml.contains("cel-interpreter"),
            "Cargo.toml must pull cel-interpreter:\n{toml}"
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
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test" }"#,
        );

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
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test" }"#,
        );
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
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test" }"#,
        );
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
    fn emit_wires_migrate_subcommand_when_pg_and_migrations_present() {
        let dir = TempDir::new().unwrap();
        // Add a migration so the generator wires the runner.
        let migrations = dir.path().join("migrations");
        fs::create_dir_all(&migrations).unwrap();
        fs::write(migrations.join("0001_init.sql"), "-- init\n").unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test", "services": { "postgres": { "url": "env:DATABASE_URL" } } }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");
        assert!(main_rs.contains("sqlx::migrate!(\"./migrations\")"));
        assert!(main_rs.contains("async fn rb_migrate_run"));
        assert!(main_rs.contains("async fn rb_migrate_list"));
        assert!(main_rs.contains("if args.get(1).map(|s| s.as_str()) == Some(\"migrate\")"));
        assert!(main_rs.contains("RUBLOCKS_DEV"));
    }

    #[test]
    fn emit_omits_migrate_subcommand_without_migrations() {
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test", "services": { "postgres": { "url": "env:DATABASE_URL" } } }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        assert!(!main_rs.contains("sqlx::migrate!"));
        assert!(!main_rs.contains("rb_migrate_run"));
    }

    #[test]
    fn emit_wires_http_middleware_layers() {
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{
                "name": "rb_test",
                "version": "0.0.0",
                "description": "test",
                "http": {
                    "compression": true,
                    "cors": { "origins": ["https://example.com"] },
                    "timeout_ms": 30000,
                    "security_headers": true
                }
            }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");
        assert!(main_rs.contains("CompressionLayer"));
        assert!(main_rs.contains("CorsLayer"));
        assert!(main_rs.contains("TimeoutLayer"));
        assert!(main_rs.contains("x-content-type-options"));
        let toml = render_cargo_toml(&manifest, false);
        assert!(toml.contains("tower-http"));
        assert!(toml.contains("compression-full"));
        assert!(toml.contains("\"cors\""));
        assert!(toml.contains("\"timeout\""));
        assert!(toml.contains("set-header"));
    }

    #[test]
    fn emit_skips_http_layer_block_when_unset() {
        // With no `http.*` config, only the always-on `X-App-Version`
        // stamp ships — none of the opt-in middleware layers do.
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test" }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        assert!(!main_rs.contains("CompressionLayer"));
        assert!(!main_rs.contains("CorsLayer"));
        assert!(!main_rs.contains("TimeoutLayer"));
        assert!(!main_rs.contains("x-content-type-options"));
        let toml = render_cargo_toml(&manifest, false);
        // `tower-http` is always pulled in for the `X-App-Version` layer.
        assert!(toml.contains("tower-http"));
        assert!(toml.contains("set-header"));
        // None of the opt-in features show up when their `http.*` knob is off.
        assert!(!toml.contains("compression-full"));
        assert!(!toml.contains("\"cors\""));
        assert!(!toml.contains("\"timeout\""));
    }

    #[test]
    fn emit_stamps_x_app_version_response_header() {
        // Acceptance criterion for issue #15: every project ships the
        // `X-App-Version` response header, taken verbatim from
        // `main.json.version`. The handler stack is built once at startup
        // via a `tower-http` `SetResponseHeaderLayer`.
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "1.4.2-rc.1", "description": "test" }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        assert!(
            main_rs.contains("x-app-version"),
            "x-app-version header must be wired: {main_rs}"
        );
        assert!(
            main_rs.contains("1.4.2-rc.1"),
            "header value must be the literal manifest version: {main_rs}"
        );
    }

    #[test]
    fn cargo_toml_pins_package_version_to_manifest_version() {
        // Issue #15 acceptance: generated `Cargo.toml` `package.version`
        // is the manifest value, not the hard-coded `0.1.0`.
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(dir.path(), r#"{ "name": "rb_test", "version": "2.3.4", "description": "test" }"#);
        let toml = render_cargo_toml(&manifest, false);
        assert!(
            toml.contains("version = \"2.3.4\""),
            "package.version must match the manifest: {toml}"
        );
        assert!(
            !toml.contains("version = \"0.1.0\""),
            "stale hard-coded 0.1.0 must be gone: {toml}"
        );
    }

    #[test]
    fn cors_wildcard_uses_any_origin() {
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test", "http": { "cors": { "origins": ["*"] } } }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        assert!(main_rs.contains("CorsLayer"));
        assert!(main_rs.contains("tower_http::cors::Any"));
    }

    #[test]
    fn cargo_toml_picks_mysql_feature_for_mysql_kind() {
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{
                "name": "rb_test",
                "version": "0.0.0",
                "description": "test",
                "services": { "db": { "kind": "mysql", "url": "env:MYSQL_URL" } }
            }"#,
        );
        let toml = render_cargo_toml(&manifest, false);
        assert!(toml.contains("\"mysql\""), "got: {toml}");
        assert!(!toml.contains("\"postgres\""));
    }

    #[test]
    fn main_rs_uses_mysql_pool_for_mysql_kind() {
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{
                "name": "rb_test",
                "version": "0.0.0",
                "description": "test",
                "services": { "db": { "kind": "mysql", "url": "env:MYSQL_URL" } }
            }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        assert!(
            main_rs.contains("sqlx::MySqlPool"),
            "expected MySqlPool, got:\n{main_rs}"
        );
        assert!(!main_rs.contains("sqlx::PgPool"));
    }

    #[test]
    fn cargo_toml_omits_uuid_when_no_model_uses_it() {
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test" }"#,
        );
        let toml = render_cargo_toml(&manifest, false);
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

        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test" }"#,
        );
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
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test" }"#,
        );
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
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test" }"#,
        );
        let toml = render_cargo_toml(&manifest, false);
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
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test" }"#,
        );
        let toml = render_cargo_toml(&manifest, false);
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
        fs::write(
            dir.path().join("main.json"),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test" }"#,
        )
        .unwrap();
        let err = Manifest::load(dir.path()).unwrap_err();
        assert_eq!(err.file, routes_dir.join("home.json"));
        assert!(
            err.message.contains("layout `nope`"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn cargo_toml_pulls_in_cel_interpreter_when_a_guard_block_is_used() {
        // The `guard` block evaluates a CEL predicate at request time, so
        // the dist crate must depend on `cel-interpreter`. Projects with
        // no CEL site stay free of the dependency.
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("admin.json"),
            r#"{
                "path": "/admin",
                "method": "GET",
                "kind": "page",
                "template": "admin.html",
                "process": [
                    { "block": "guard", "if": "true" }
                ]
            }"#,
        )
        .unwrap();
        let manifest = manifest_from(dir.path(), r#"{ "name": "rb_test", "version": "0.0.0", "description": "test" }"#);
        let toml = render_cargo_toml(&manifest, false);
        assert!(
            toml.contains("cel-interpreter"),
            "guard block must pull in cel-interpreter: {toml}"
        );
    }

    #[test]
    fn cargo_toml_omits_cel_interpreter_when_no_cel_site_exists() {
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("home.json"),
            r#"{"path":"/","method":"GET","kind":"page","template":"home.html"}"#,
        )
        .unwrap();
        let manifest = manifest_from(dir.path(), r#"{ "name": "rb_test", "version": "0.0.0", "description": "test" }"#);
        let toml = render_cargo_toml(&manifest, false);
        assert!(
            !toml.contains("cel-interpreter"),
            "CEL-free project must not pull in cel-interpreter: {toml}"
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
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test", "services": { "postgres": { "url": "env:DATABASE_URL" } } }"#,
        );
        let toml = render_cargo_toml(&manifest, false);
        assert!(toml.contains("uuid"));
        assert!(!toml.contains("chrono"));
        assert!(toml.contains("sqlx"));
        assert!(toml.contains("\"derive\""));
        assert!(toml.contains("\"uuid\""));
    }

    #[test]
    fn cargo_toml_carries_manifest_description() {
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.1.0", "description": "A blog with public posts." }"#,
        );
        let toml = render_cargo_toml(&manifest, false);
        assert!(
            toml.contains("description = \"A blog with public posts.\""),
            "got: {toml}"
        );
    }

    #[test]
    fn cargo_toml_escapes_quotes_and_backslashes_in_description() {
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.1.0", "description": "a \"b\" \\ c" }"#,
        );
        let toml = render_cargo_toml(&manifest, false);
        assert!(
            toml.contains(r#"description = "a \"b\" \\ c""#),
            "got: {toml}"
        );
    }

    #[test]
    fn dev_index_html_includes_description_meta_and_subtitle() {
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.1.0", "description": "A blog with public posts." }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");
        assert!(main_rs.contains("async fn dev_index"));
        assert!(main_rs.contains(
            r#"<meta name=\"description\" content=\"A blog with public posts.\">"#
        ));
        // Subtitle paragraph below the <h1> displays the same value.
        assert!(main_rs.contains("A blog with public posts."));
    }

    #[test]
    fn dev_index_html_escapes_description() {
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.1.0", "description": "a <b> & \"c\"" }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        assert!(main_rs.contains("a &lt;b&gt; &amp; &quot;c&quot;"));
    }
}
