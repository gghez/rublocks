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
use crate::blocks::runtime::{BlockCodegenCtx, emit_block_with_logging};
use crate::codegen_input;
use crate::language;
use crate::layouts::{Layout, RequireType};
use crate::manifest::{
    DbKind, HttpConfig, LogIncludeValue, LogLevel, Logging, Manifest, ResolvedSftpService,
    ServiceUrl,
};
use crate::migrations;
use crate::models::{FieldType, Model};
use crate::routes::{
    HttpMethod, OutputNode, RedirectSegment, RedirectSpec, Route, RouteKind, axum_path,
};
use crate::sftp::SftpAuthMethod;
use crate::value_ref::{ValueRef, ValueScope};
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
    // Issue #14: warn when the project ships a BCP 47 tag we don't have a
    // localized dev-string table for. The overlay then falls back to English
    // — the warning prevents silent degradation when an agent picks an
    // unsupported locale.
    if !language::has_dev_strings(&manifest.language) {
        eprintln!(
            "rublocks: no localized dev-mode strings for language `{}` \u{2014} falling back to English in the dev overlay",
            manifest.language
        );
    }
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
    crate::manifest::write_text_utf8(
        &dist_dir.join("Cargo.toml"),
        &render_cargo_toml(manifest, has_migrations),
    )?;
    crate::manifest::write_text_utf8(
        &dist_dir.join("src").join("main.rs"),
        &render_main_rs(manifest, has_migrations)?,
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

/// Always emitted now that every route executes its process pipeline at
/// runtime — every handler may early-return a guard/error response via
/// the helpers in [`render_rb_runtime_module`].
fn project_uses_runtime(_routes: &[Route]) -> bool {
    true
}

/// Emit the dist-side `_rb_runtime` module — response builders shared by
/// every generated handler: 403 short-circuits, structured error
/// responses, sqlx error mapping, runtime validation failures, redirects.
fn render_rb_runtime_module() -> TokenStream {
    quote! {
        pub mod _rb_runtime {
            use axum::response::IntoResponse as _;

            /// JSON `403 Forbidden` for `kind: api` routes. Body is a
            /// fixed `{"error":{"code":"forbidden"}}` to keep the dist
            /// dependency surface minimal.
            pub fn api_403() -> axum::response::Response {
                (
                    axum::http::StatusCode::FORBIDDEN,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    r#"{"error":{"code":"forbidden"}}"#,
                )
                    .into_response()
            }

            /// Plain-text `403 Forbidden` for `kind: page` routes.
            pub fn page_403() -> axum::response::Response {
                (axum::http::StatusCode::FORBIDDEN, "403 Forbidden").into_response()
            }

            /// Structured JSON error for `kind: api` routes. Drives the
            /// `error` block's response shape.
            pub fn api_error(
                status: u16,
                code: String,
                description: Option<String>,
            ) -> axum::response::Response {
                let status = axum::http::StatusCode::from_u16(status)
                    .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
                let mut body = serde_json::Map::new();
                let mut err = serde_json::Map::new();
                err.insert("code".to_string(), serde_json::Value::String(code));
                if let Some(d) = description {
                    err.insert("description".to_string(), serde_json::Value::String(d));
                }
                body.insert("error".to_string(), serde_json::Value::Object(err));
                (status, axum::Json(serde_json::Value::Object(body))).into_response()
            }

            /// Plain-text error response for `kind: page` routes.
            pub fn page_error(status: u16, body: String) -> axum::response::Response {
                let status = axum::http::StatusCode::from_u16(status)
                    .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
                (status, body).into_response()
            }

            /// Map a sqlx error to a `500 Internal Server Error`. The
            /// generated handler surfaces the message in plain text so
            /// dev-mode users see the cause without digging into logs.
            pub fn db_error(e: sqlx::Error) -> axum::response::Response {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("rublocks: db error: {e}"),
                )
                    .into_response()
            }

            /// `422 Unprocessable Content` for a CEL `validate` predicate
            /// that returned `false` at request time. Used by both input
            /// validation and `db.insert` field-level validators.
            pub fn field_validation_error(
                field: String,
                expr: String,
            ) -> axum::response::Response {
                let mut err = serde_json::Map::new();
                err.insert("field".to_string(), serde_json::Value::String(field));
                err.insert("code".to_string(), serde_json::Value::String("validate".to_string()));
                err.insert(
                    "message".to_string(),
                    serde_json::Value::String(format!("must satisfy `{expr}`")),
                );
                let mut body = serde_json::Map::new();
                body.insert(
                    "errors".to_string(),
                    serde_json::Value::Array(vec![serde_json::Value::Object(err)]),
                );
                (
                    axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                    axum::Json(serde_json::Value::Object(body)),
                )
                    .into_response()
            }

            /// Emit a redirect response with the given HTTP status and
            /// Location header. `kind: page` POST handlers terminate with
            /// this after their `process` succeeds.
            pub fn redirect(status: u16, location: String) -> axum::response::Response {
                let status = axum::http::StatusCode::from_u16(status)
                    .unwrap_or(axum::http::StatusCode::SEE_OTHER);
                (
                    status,
                    [(axum::http::header::LOCATION, location)],
                )
                    .into_response()
            }
        }
    }
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
    // SFTP foundation (issue #27). `russh` + `russh-sftp` are pure-Rust
    // async SSH/SFTP crates; they land in the dist `Cargo.toml` only when at
    // least one `services.<name>.kind == "sftp"` is declared (operation
    // blocks will add their own trigger in follow-up issues).
    if !manifest.sftp_services.is_empty() {
        deps.push_str("russh = \"0.60\"\n");
        deps.push_str("russh-sftp = \"2\"\n");
    }
    // Askama lives in the dist crate only when a page route actually needs it
    // — projects that ship pure JSON APIs keep their dependency surface small.
    if has_page_routes(manifest) {
        deps.push_str("askama = \"0.14\"\n");
    }
    // `serde_json` is unconditional now that `_rb_runtime` (always emitted)
    // and the API response builder (every `kind: api` route) lean on it for
    // structured error/output JSON. The historical gating on
    // `project_uses_input` left bare API projects broken at link time.
    deps.push_str("serde_json = \"1\"\n");
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
        deps.push_str("cel = { version = \"0.13\", default-features = false }\n");
    }
    // Logging (issue #17). `tracing` + `tracing-subscriber` are always pulled
    // in — `main.json.logging` is mandatory, so every project ships the
    // structured-logging pipeline. The `trace` feature on `tower-http`
    // backs the per-request span (request_id / route / status / duration).
    deps.push_str("tracing = \"0.1\"\n");
    deps.push_str(
        "tracing-subscriber = { version = \"0.3\", features = [\"json\", \"env-filter\"] }\n",
    );

    // `X-App-Version` is stamped on every response, so `tower-http` with the
    // `set-header` feature is always pulled in. Anything the user declared
    // under `http.*` adds its features on top. `http` is always needed for
    // the `HeaderName::from_static` call site used by the version stamp. The
    // `trace` feature carries the per-request span — always on now that
    // structured logging is mandatory.
    let mut tower_feats: Vec<&'static str> = vec!["set-header", "trace"];
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
    let sftp_fields = manifest
        .sftp_services
        .iter()
        .map(render_sftp_state_field)
        .collect::<Vec<_>>();
    let sftp_inits = manifest
        .sftp_services
        .iter()
        .map(render_sftp_init)
        .collect::<Vec<_>>();
    let sftp_state_items = manifest
        .sftp_services
        .iter()
        .map(|svc| {
            let ident = format_ident!("{}", svc.name);
            quote! { #ident, }
        })
        .collect::<Vec<_>>();
    let sftp_module = (!manifest.sftp_services.is_empty()).then(render_rb_sftp_module);

    let pg_init = database.map(|db| {
        let url = url_expr(&db.url);
        let ty = sqlx_pool_type(db.kind);
        // Postgres alone gets a forced `client_encoding=UTF8` query
        // parameter so the session matches the project-wide encoding
        // contract regardless of whatever locale the server was
        // initialized with (issue #13). MySQL/MariaDB negotiate encoding
        // via the `charset` URL parameter — left to user opt-in for now;
        // MSSQL has no equivalent knob.
        if db.kind == DbKind::Postgres {
            quote! {
                let pg = {
                    let base = #url;
                    let sep = if base.contains('?') { "&" } else { "?" };
                    let url = format!("{base}{sep}client_encoding=UTF8");
                    #ty::connect(&url).await?
                };
            }
        } else {
            quote! {
                let pg = #ty::connect(&#url).await?;
            }
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
    let language = manifest.language.as_str();
    let dev_index_fn = (!user_owns_root_get).then(|| {
        let html = render_dev_index_html(&manifest.name, &manifest.description, language);
        quote! {
            async fn dev_index() -> impl axum::response::IntoResponse {
                (
                    [(axum::http::header::CONTENT_LANGUAGE, #language)],
                    axum::response::Html(#html),
                )
            }
        }
    });
    let dev_root_route = (!user_owns_root_get).then(|| {
        quote! { router = router.route("/", get(dev_index)); }
    });

    let method_imports = used_method_imports(&manifest.routes);
    let route_registrations = manifest.routes.iter().map(render_route_registration);
    let db_kind = manifest.database.as_ref().map(|d| d.kind);
    let route_handlers: Vec<TokenStream> = manifest
        .routes
        .iter()
        .map(|r| render_route_handler(r, &manifest.layouts, &manifest.models, db_kind, language))
        .collect::<Result<Vec<_>>>()?;
    let models_module = render_models_module(&manifest.models, has_pg);
    let rb_util_module = render_rb_util_module(&manifest.models, database.map(|d| d.kind));
    let rb_input_module = codegen_input::project_uses_input(&manifest.routes)
        .then(codegen_input::render_rb_input_module);
    let rb_runtime_module = project_uses_runtime(&manifest.routes).then(render_rb_runtime_module);
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
    let encoding_module = render_rb_encoding_module(manifest);
    let encoding_layer = render_encoding_layer(manifest);
    let rb_log_module = render_rb_log_module();
    let tracing_init = render_tracing_init(&manifest.logging);
    let trace_layer = render_trace_layer();

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

        #encoding_module

        #rb_log_module

        #rb_util_module

        #rb_input_module

        #rb_runtime_module

        #(#input_modules)*

        #models_module

        #sftp_module

        #[derive(Clone)]
        pub struct AppState {
            #pg_field
            #redis_field
            #(#sftp_fields)*
        }

        #[tokio::main]
        async fn main() -> anyhow::Result<()> {
            #tracing_init

            #pg_init
            #redis_init
            #(#sftp_inits)*

            #cli_dispatch

            let state = AppState {
                #pg_state
                #redis_state
                #(#sftp_state_items)*
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

            #trace_layer

            #http_layer

            #encoding_layer

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
    // `Default` is required so page-context structs can fill any field not
    // bound by a process block with the type's default. Every supported
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
/// The handler body is fully derived from the route declaration: input
/// extraction + validation, process-block execution against the registry,
/// view/output assembly, and either Askama rendering (`kind: page`) or
/// `axum::Json` (`kind: api`) — including `redirect:` short-circuits.
fn render_route_registration(route: &Route) -> TokenStream {
    let path = axum_path(&route.path);
    let handler = format_ident!("route_{}", route.name);
    let method = method_ident(route.method);
    quote! {
        router = router.route(#path, #method(#handler));
    }
}

fn render_route_handler(
    route: &Route,
    layouts: &[Layout],
    models: &[Model],
    db_kind: Option<DbKind>,
    language: &str,
) -> Result<TokenStream> {
    emit_handler(route, layouts, models, db_kind, language)
        .map_err(|e| anyhow::anyhow!("route `{}` ({}): {e}", route.name, route.path))
}

/// Emit one route handler — orchestrates input validation, layout +
/// route process execution, and response construction. Every route kind
/// runs through this single pipeline so view/output/redirect resolution
/// shares its source of truth.
fn emit_handler(
    route: &Route,
    layouts: &[Layout],
    models: &[Model],
    db_kind: Option<DbKind>,
    language: &str,
) -> Result<TokenStream, String> {
    let handler = format_ident!("route_{}", route.name);
    let has_input = route.input.as_ref().is_some_and(|s| !s.is_empty());
    let needs_state = handler_needs_state(route, layouts);
    let state_param = needs_state.then(|| {
        quote! { axum::extract::State(__state): axum::extract::State<AppState>, }
    });
    let extractor_params = codegen_input::handler_extractor_params(route);
    // Tail param list — state always comes first to keep Axum's
    // extractor ordering happy, then the input extractors.
    let params = match (state_param.as_ref(), extractor_params.is_empty()) {
        (Some(state), false) => quote! { #state #extractor_params },
        (Some(state), true) => quote! { #state },
        (None, false) => quote! { #extractor_params },
        (None, true) => quote! {},
    };

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

    // Execute layout.process then route.process, growing the value scope
    // as bindings appear. Layout bindings are visible to route blocks
    // and to view/output resolution.
    let mut scope = ValueScope {
        input: route.input.as_ref(),
        bindings: IndexMap::new(),
        models,
    };
    let layout = route
        .layout
        .as_deref()
        .and_then(|name| Layout::find(layouts, name));

    let mut block_pieces: Vec<TokenStream> = Vec::new();
    let mut block_index = 0;
    if let Some(l) = layout {
        for b in &l.process {
            let ctx = BlockCodegenCtx {
                models,
                db_kind,
                route_kind: route.kind,
                index: block_index,
            };
            block_pieces.push(emit_block_with_logging(b.as_ref(), &ctx, &mut scope)?);
            block_index += 1;
        }
    }
    for b in &route.process {
        let ctx = BlockCodegenCtx {
            models,
            db_kind,
            route_kind: route.kind,
            index: block_index,
        };
        block_pieces.push(emit_block_with_logging(b.as_ref(), &ctx, &mut scope)?);
        block_index += 1;
    }

    let response = build_response(route, layout, models, &scope, language)?;

    let ctx_module = if route.kind == RouteKind::Page
        && route.method == HttpMethod::Get
        && route.template.is_some()
    {
        Some(render_page_context_module(route, layout, models, &scope)?)
    } else {
        None
    };

    Ok(quote! {
        #ctx_module
        async fn #handler(#params) -> axum::response::Response {
            #validation_call
            #validation_branch
            #(#block_pieces)*
            #response
        }
    })
}

/// True when the handler must accept `axum::extract::State<AppState>` —
/// the case as soon as any block runs (they may read the database pool)
/// or the route declares any input the validator inspects.
fn handler_needs_state(route: &Route, layouts: &[Layout]) -> bool {
    !route.process.is_empty()
        || route
            .layout
            .as_deref()
            .and_then(|n| Layout::find(layouts, n))
            .map(|l| !l.process.is_empty())
            .unwrap_or(false)
}

/// Build the response tail for one route. Kind drives the shape:
/// page+GET+template renders via Askama; redirect short-circuits with a
/// `Location` header; API returns a JSON projection; everything else
/// terminates with a `200 OK` empty body.
fn build_response(
    route: &Route,
    layout: Option<&Layout>,
    models: &[Model],
    scope: &ValueScope,
    language: &str,
) -> Result<TokenStream, String> {
    if let Some(redirect) = route.redirect.as_ref() {
        return build_redirect(redirect, scope);
    }
    match route.kind {
        RouteKind::Api => build_api_response(route, scope, language),
        RouteKind::Page => {
            if route.method == HttpMethod::Get && route.template.is_some() {
                build_page_template_response(route, layout, models, scope, language)
            } else {
                // Page POST/PUT/... without redirect: return a thin 200
                // so the handler still terminates with a well-typed
                // response. Authors who want a different shape declare
                // `redirect:` explicitly.
                Ok(quote! {
                    axum::response::IntoResponse::into_response(
                        (axum::http::StatusCode::OK, ())
                    )
                })
            }
        }
    }
}

fn build_redirect(redirect: &RedirectSpec, scope: &ValueScope) -> Result<TokenStream, String> {
    let status = redirect.status;
    if redirect.to.is_empty() {
        return Err("redirect.to: must not be empty".to_string());
    }
    // Build the `format!` template + argument list.
    let mut fmt = String::new();
    let mut args: Vec<TokenStream> = Vec::new();
    for seg in &redirect.to {
        match seg {
            RedirectSegment::Literal(s) => {
                // Escape `{` / `}` for format! literals.
                for c in s.chars() {
                    if c == '{' {
                        fmt.push_str("{{");
                    } else if c == '}' {
                        fmt.push_str("}}");
                    } else {
                        fmt.push(c);
                    }
                }
            }
            RedirectSegment::Ref(r) => {
                let emitted = r.emit_expr(scope)?;
                let expr = &emitted.expr;
                fmt.push_str("{}");
                args.push(quote! { #expr });
            }
        }
    }
    Ok(quote! {
        return crate::_rb_runtime::redirect(#status, format!(#fmt #(, #args)*));
    })
}

fn build_api_response(
    route: &Route,
    scope: &ValueScope,
    language: &str,
) -> Result<TokenStream, String> {
    let body = match route.output.as_ref() {
        Some(spec) => render_output_node(&spec.root, scope)?,
        None => quote! { ::serde_json::Value::Object(::serde_json::Map::new()) },
    };
    Ok(quote! {
        axum::response::IntoResponse::into_response((
            [(axum::http::header::CONTENT_LANGUAGE, #language)],
            axum::Json(#body),
        ))
    })
}

fn render_output_node(node: &OutputNode, scope: &ValueScope) -> Result<TokenStream, String> {
    match node {
        OutputNode::Leaf(r) => {
            let emitted = r.emit_expr(scope)?;
            let expr = &emitted.expr;
            Ok(quote! { ::serde_json::to_value(&(#expr)).unwrap_or(::serde_json::Value::Null) })
        }
        OutputNode::Object(map) => {
            let entries = map
                .iter()
                .map(|(k, v)| {
                    let v_tokens = render_output_node(v, scope)?;
                    Ok::<TokenStream, String>(quote! {
                        __obj.insert(#k.to_string(), #v_tokens);
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(quote! {
                {
                    let mut __obj = ::serde_json::Map::new();
                    #(#entries)*
                    ::serde_json::Value::Object(__obj)
                }
            })
        }
    }
}

fn build_page_template_response(
    route: &Route,
    layout: Option<&Layout>,
    models: &[Model],
    scope: &ValueScope,
    language: &str,
) -> Result<TokenStream, String> {
    let ctx_mod = format_ident!("ctx_{}", route.name);
    let fields = page_context_fields(route, layout, models);
    let assigns = fields
        .iter()
        .map(|f| {
            let ident = format_ident!("{}", f.name);
            let value = resolve_view_expr(&f.source, scope)?;
            Ok::<TokenStream, String>(quote! { #ident: #value, })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(quote! {
        use askama::Template as _;
        let ctx = #ctx_mod::PageContext {
            #(#assigns)*
        };
        let rendered = ctx
            .render()
            .unwrap_or_else(|e| format!("rublocks: template render error: {e}"));
        axum::response::IntoResponse::into_response((
            [(axum::http::header::CONTENT_LANGUAGE, #language)],
            axum::response::Html(maybe_inject_dev_snippet(rendered)),
        ))
    })
}

/// Resolve a view binding's raw JSON value into the Rust expression used
/// to initialise a page context field. Literals stringify; refs go
/// through the value-ref resolver.
fn resolve_view_expr(raw: &str, scope: &ValueScope) -> Result<TokenStream, String> {
    if raw.starts_with('$') {
        let r = ValueRef::parse(&serde_json::Value::String(raw.to_string()))?;
        let e = r.emit_expr(scope)?;
        let expr = e.expr;
        Ok(quote! { #expr })
    } else {
        Ok(quote! { #raw.to_string() })
    }
}

fn render_page_context_module(
    route: &Route,
    layout: Option<&Layout>,
    models: &[Model],
    _scope: &ValueScope,
) -> Result<TokenStream, String> {
    let ctx_mod = format_ident!("ctx_{}", route.name);
    let template_path = route.template.as_deref().unwrap_or_default();
    let fields = page_context_fields(route, layout, models);
    let field_defs = fields.iter().map(|f| {
        let ident = format_ident!("{}", f.name);
        let ty = &f.ty;
        quote! { pub #ident: #ty, }
    });
    Ok(quote! {
        pub mod #ctx_mod {
            #[derive(askama::Template)]
            #[template(path = #template_path)]
            pub struct PageContext {
                #(#field_defs)*
            }
        }
    })
}

/// One context-struct field, ready to embed in the generated module.
struct ContextField {
    name: String,
    ty: TokenStream,
    /// Raw JSON expression as declared in `view:` — literal or `$ref`.
    /// Drives both the field type (via best-effort inference) and the
    /// handler's runtime assignment.
    source: String,
}

fn page_context_fields(
    route: &Route,
    layout: Option<&Layout>,
    models: &[Model],
) -> Vec<ContextField> {
    let mut fields: IndexMap<String, ContextField> = IndexMap::new();

    if let Some(layout) = layout {
        for (k, req) in &layout.requires {
            let ty = match req.ty {
                RequireType::String => quote! { String },
            };
            fields.insert(
                k.clone(),
                ContextField {
                    name: k.clone(),
                    ty,
                    source: String::new(),
                },
            );
        }
        for (k, v) in &layout.view {
            let entry = ContextField {
                name: k.clone(),
                ty: infer_view_type(v, &layout.process, models),
                source: v.clone(),
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
                source: v.clone(),
            },
        );
    }

    fields.into_iter().map(|(_, v)| v).collect()
}

/// Best-effort type inference for a view binding.
///
/// `$<name>` references resolve through the block's `output_type`;
/// `$<name>.<field>` resolves to the model field's Rust type;
/// everything else falls back to `String` — the template renders them
/// via `Display`.
fn infer_view_type(
    value: &str,
    processes: &[Box<dyn BlockInstance>],
    models: &[Model],
) -> TokenStream {
    let Some(rest) = value.strip_prefix('$') else {
        return quote! { String };
    };
    let (head, tail) = match rest.split_once('.') {
        Some((h, t)) => (h, Some(t)),
        None => (rest, None),
    };
    let Some(block) = processes.iter().find(|p| p.name() == Some(head)) else {
        return quote! { String };
    };
    match tail {
        None => block
            .output_type(models)
            .unwrap_or_else(|| quote! { String }),
        Some(field) => infer_block_field_type(block.as_ref(), field, models),
    }
}

/// Resolve `$block.field`'s Rust type by looking up the field on the
/// model the block targets. Fields are typed exactly the same way the
/// generated model struct types them (including the `NullDisplay<T>`
/// wrapper for nullable columns).
fn infer_block_field_type(block: &dyn BlockInstance, field: &str, models: &[Model]) -> TokenStream {
    let Some(table) = block.target_table() else {
        return quote! { String };
    };
    let Some(model) = models.iter().find(|m| m.table == table) else {
        return quote! { String };
    };
    let Some(def) = model.fields.get(field) else {
        return quote! { String };
    };
    model_field_type(def.ty, def.nullable)
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

/// Emit the `pub mod _rb_encoding` helper that enforces the manifest's
/// declared encoding on the HTTP boundary at runtime.
///
/// Two guarantees the module embodies (issue #13):
/// - **Inbound, strict.** Reject incoming requests whose `Content-Type`
///   advertises a non-UTF-8 charset for JSON / form-urlencoded / text bodies
///   with `415 Unsupported Media Type`. A missing charset is taken to mean
///   "the project default applies" and accepted.
/// - **Outbound, explicit.** Append `charset=utf-8` to outgoing
///   `application/json` and `text/*` responses that don't already carry a
///   `charset=` parameter, so HTTP clients never have to guess.
fn render_rb_encoding_module(manifest: &Manifest) -> TokenStream {
    let charset = manifest.encoding.charset_label();
    let charset_eq = format!("charset={charset}");
    quote! {
        pub mod _rb_encoding {
            use axum::extract::Request;
            use axum::http::{HeaderValue, StatusCode, header::CONTENT_TYPE};
            use axum::middleware::Next;
            use axum::response::{IntoResponse, Response};

            /// Project-wide character encoding declared in `main.json` and
            /// woven into every HTTP boundary by the layer below.
            pub const CHARSET: &str = #charset;

            /// Middleware function applied as the outermost router layer.
            /// Runs before any handler on the request side and after every
            /// handler on the response side.
            pub async fn enforce(req: Request, next: Next) -> Response {
                if let Some(value) = req.headers().get(CONTENT_TYPE) {
                    if let Ok(s) = value.to_str() {
                        if mime_carries_text(s) {
                            if let Some(cs) = extract_charset(s) {
                                if !cs.eq_ignore_ascii_case(CHARSET) {
                                    return (
                                        StatusCode::UNSUPPORTED_MEDIA_TYPE,
                                        format!(
                                            "rublocks: only charset={} is accepted (got `{}`)",
                                            CHARSET, cs,
                                        ),
                                    )
                                        .into_response();
                                }
                            }
                        }
                    }
                }
                let mut res = next.run(req).await;
                let needs_label = res
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| mime_should_be_labelled(s) && extract_charset(s).is_none())
                    .unwrap_or(false);
                if needs_label {
                    let current = res
                        .headers()
                        .get(CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    if let Some(current) = current {
                        let labelled = format!("{current}; {}", #charset_eq);
                        if let Ok(hv) = HeaderValue::from_str(&labelled) {
                            res.headers_mut().insert(CONTENT_TYPE, hv);
                        }
                    }
                }
                res
            }

            /// MIME types where a charset parameter is meaningful — i.e. the
            /// body is interpreted as text. Binary types (`image/*`,
            /// `application/octet-stream`, …) are left untouched.
            fn mime_carries_text(content_type: &str) -> bool {
                let mime = mime_of(content_type);
                mime.eq_ignore_ascii_case("application/json")
                    || mime.eq_ignore_ascii_case("application/x-www-form-urlencoded")
                    || mime.to_ascii_lowercase().starts_with("text/")
            }

            /// Same predicate restricted to response bodies. Form-encoded
            /// payloads are not a server response shape, so we exclude them
            /// from the labelling pass.
            fn mime_should_be_labelled(content_type: &str) -> bool {
                let mime = mime_of(content_type);
                mime.eq_ignore_ascii_case("application/json")
                    || mime.to_ascii_lowercase().starts_with("text/")
            }

            fn mime_of(content_type: &str) -> &str {
                content_type.split(';').next().unwrap_or(content_type).trim()
            }

            /// Extract the `charset=` parameter from a Content-Type, if any.
            /// Case-insensitive on the parameter name; the value is returned
            /// as-is (callers compare with `eq_ignore_ascii_case`).
            fn extract_charset(content_type: &str) -> Option<&str> {
                for part in content_type.split(';').skip(1) {
                    let part = part.trim();
                    if part.len() < "charset=".len() {
                        continue;
                    }
                    let (head, tail) = part.split_at("charset=".len());
                    if head.eq_ignore_ascii_case("charset=") {
                        let value = tail.trim().trim_matches('"');
                        return Some(value);
                    }
                }
                None
            }
        }
    }
}

/// Apply the `_rb_encoding::enforce` middleware as the outermost router
/// layer. Always emitted: the manifest's `encoding` field is mandatory, so
/// every generated app has a declared encoding to enforce.
fn render_encoding_layer(_manifest: &Manifest) -> TokenStream {
    quote! {
        router = router.layer(axum::middleware::from_fn(_rb_encoding::enforce));
    }
}

/// Emit the dist-side `_rb_log` module — issue #17.
///
/// Provides the small runtime surface used by the generated block
/// instrumentation: `error_chain` (the `source()` walk surfaced on every
/// block-failure event), `error_backtrace` (best-effort
/// `RUST_BACKTRACE`-gated capture), and `rand_request_id` (fallback id
/// when the inbound request lacks an `x-request-id` header).
fn render_rb_log_module() -> TokenStream {
    quote! {
        /// Structured-logging support. Generated by rublocks; do not edit.
        pub mod _rb_log {
            /// Walk an error's `source()` chain into a Vec<String> so the
            /// block-failure event ships the full cause stack (matches the
            /// `error.chain` field documented in `docs/logging.md`).
            pub fn error_chain(err: &(dyn ::std::error::Error + 'static)) -> ::std::vec::Vec<::std::string::String> {
                let mut chain: ::std::vec::Vec<::std::string::String> = ::std::vec::Vec::new();
                chain.push(::std::format!("{err}"));
                let mut cur: ::std::option::Option<&(dyn ::std::error::Error + 'static)> = err.source();
                while let ::std::option::Option::Some(next) = cur {
                    chain.push(::std::format!("{next}"));
                    cur = next.source();
                }
                chain
            }

            /// Best-effort backtrace capture. Returns `None` unless the
            /// process was started with `RUST_BACKTRACE=1` (or `=full`),
            /// matching std's own gating policy. Stringified through
            /// `Display` so it lands in the JSON line as a single string.
            pub fn error_backtrace() -> ::std::option::Option<::std::string::String> {
                let bt = ::std::backtrace::Backtrace::capture();
                match bt.status() {
                    ::std::backtrace::BacktraceStatus::Captured => {
                        ::std::option::Option::Some(::std::format!("{bt}"))
                    }
                    _ => ::std::option::Option::None,
                }
            }

            /// Lightweight per-request id used when the inbound request does
            /// not carry an `x-request-id` header. Folds a monotonic counter
            /// into the wall-clock nanoseconds-since-epoch so two concurrent
            /// requests on the same tick still get distinct ids.
            ///
            /// Not cryptographically strong on purpose: the id is for
            /// correlation across log lines, not for security. A pre-existing
            /// `x-request-id` (from a reverse proxy / load balancer) wins.
            pub fn rand_request_id() -> u128 {
                use ::std::sync::atomic::{AtomicU64, Ordering};
                static COUNTER: AtomicU64 = AtomicU64::new(0);
                let seq = COUNTER.fetch_add(1, Ordering::Relaxed) as u128;
                let now = ::std::time::SystemTime::now()
                    .duration_since(::std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                (now << 32) ^ seq
            }
        }
    }
}

/// Tokens that bootstrap the `tracing-subscriber` JSON layer in `main()`.
///
/// The level comes from `main.json.logging.level` — no env-var override is
/// wired today; the project author commits to the level in source. Output
/// is NDJSON on stdout: one compact JSON object per event line, no
/// internal newline. Event-level fields flatten to the top level; span
/// fields stay nested under `span` / `spans`.
///
/// `logging.include` is resolved at startup (env-prefixed values via
/// `std::env::var`) and folded into a root span entered for the lifetime
/// of the server, so every event inherits the configured key/value pairs.
fn render_tracing_init(logging: &Logging) -> TokenStream {
    let level_ident = format_ident!(
        "{}",
        match logging.level {
            LogLevel::Trace => "TRACE",
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    );

    let include_list: Vec<TokenStream> = logging
        .include
        .iter()
        .map(|(k, v)| {
            let key_ident = format_ident!("{k}");
            match v {
                LogIncludeValue::Literal(s) => {
                    let s = s.as_str();
                    quote! { #key_ident = #s }
                }
                LogIncludeValue::Env(var) => {
                    let var = var.as_str();
                    quote! { #key_ident = %::std::env::var(#var).unwrap_or_default() }
                }
            }
        })
        .collect();

    let root_span = if include_list.is_empty() {
        quote! {}
    } else {
        quote! {
            let __rb_root_span = ::tracing::info_span!(target: "rublocks::app", "app", #(#include_list),*);
            let _rb_root_guard = __rb_root_span.entered();
        }
    };

    quote! {
        // NDJSON on stdout: one compact JSON object per event. Event fields
        // flatten to the top of every line so `jq` / `grep` / aggregators
        // see `duration_us`, `message`, ... as siblings of `target` and
        // `level`. Span fields stay nested under `span` / `spans`. The
        // `set_global_default` error (re-entrancy in tests) is intentionally
        // swallowed.
        let __rb_log_subscriber = ::tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(true)
            .with_target(true)
            .with_max_level(::tracing::Level::#level_ident)
            .finish();
        let _ = ::tracing::subscriber::set_global_default(__rb_log_subscriber);

        #root_span
    }
}

/// Tokens for the request-scope `tower-http` TraceLayer.
///
/// Builds a `tracing::info_span!` per request carrying `request_id`,
/// `method`, `path`, and (post-match) the `route` pattern. Block-level
/// spans are children of this span so their events inherit those fields.
fn render_trace_layer() -> TokenStream {
    quote! {
        router = router.layer(
            tower_http::trace::TraceLayer::new_for_http()
                .make_span_with(|request: &axum::http::Request<_>| {
                    let request_id: ::std::string::String = request
                        .headers()
                        .get("x-request-id")
                        .and_then(|v| v.to_str().ok())
                        .map(::std::string::String::from)
                        .unwrap_or_else(|| ::std::format!("{:032x}", crate::_rb_log::rand_request_id()));
                    let method = ::std::format!("{}", request.method());
                    let path = request.uri().path().to_string();
                    let route = request
                        .extensions()
                        .get::<axum::extract::MatchedPath>()
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_else(|| path.clone());
                    ::tracing::info_span!(
                        target: "rublocks::request",
                        "http_request",
                        request_id = %request_id,
                        method = %method,
                        path = %path,
                        route = %route,
                    )
                })
        );
    }
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

    // Reassign `router` instead of shadowing — the encoding layer below
    // assumes the binding stays mutable, and `let router = ...` would
    // silently kill that for any layer wired in after.
    quote! {
        router = router #(#steps)*;
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

/// Same `Literal | Env` lift as [`url_expr`] but typed for `Option<String>`
/// fields (e.g. `host_key_fingerprint`, `passphrase`). `None` carries through
/// at the call site so missing optionals stay `None` at runtime.
fn optional_url_expr(url: &ServiceUrl) -> TokenStream {
    match url {
        ServiceUrl::Literal(s) => quote! { Some(#s.to_string()) },
        ServiceUrl::Env(var) => quote! { Some(std::env::var(#var)?) },
    }
}

/// `pub <name>: std::sync::Arc<crate::_rb_sftp::SftpService>,` on `AppState`.
fn render_sftp_state_field(svc: &ResolvedSftpService) -> TokenStream {
    let ident = format_ident!("{}", svc.name);
    quote! {
        pub #ident: std::sync::Arc<crate::_rb_sftp::SftpService>,
    }
}

/// Startup snippet that resolves the config (env/literal), enforces the
/// release-mode `host_key_fingerprint` rule, and wraps the config in an
/// `Arc` ready to clone into request handlers.
///
/// `host_key_fingerprint` enforcement: if the manifest left it unset, codegen
/// emits a runtime check that warns in dev (`RUBLOCKS_DEV=1`) and errors out
/// otherwise — this is the "trust on first use vs. require explicit pinning"
/// contract described in `docs/blocks/sftp.md`.
fn render_sftp_init(svc: &ResolvedSftpService) -> TokenStream {
    let ident = format_ident!("{}", svc.name);
    let name = svc.name.as_str();
    let host = url_expr(&svc.config.host);
    let user = url_expr(&svc.config.user);
    let port = svc.config.port;
    let fingerprint_expr = match &svc.config.host_key_fingerprint {
        Some(v) => optional_url_expr(v),
        None => {
            // Missing-fingerprint policy is decided at runtime so the dist
            // binary can adapt to the dev/release flag. Both branches return a
            // typed value codegen can splice in unconditionally.
            let context = format!(
                "services.{name}.host_key_fingerprint is required in release builds \u{2014} pin the server fingerprint or run under `rublocks dev` to TOFU"
            );
            let warn = format!(
                "rublocks: services.{name}.host_key_fingerprint is unset \u{2014} dev mode is trusting on first use; pin it before shipping"
            );
            quote! {
                {
                    if std::env::var("RUBLOCKS_DEV").is_err() {
                        return Err(anyhow::anyhow!(#context));
                    }
                    eprintln!(#warn);
                    Option::<String>::None
                }
            }
        }
    };
    let auth_expr = render_sftp_auth_expr(svc);
    quote! {
        let #ident = std::sync::Arc::new(crate::_rb_sftp::SftpService {
            host: #host,
            port: #port,
            user: #user,
            auth: #auth_expr,
            host_key_fingerprint: #fingerprint_expr,
        });
    }
}

fn render_sftp_auth_expr(svc: &ResolvedSftpService) -> TokenStream {
    let passphrase = match &svc.config.auth.passphrase {
        Some(v) => optional_url_expr(v),
        None => quote! { None },
    };
    match svc.auth_method {
        SftpAuthMethod::Password => {
            let pw = url_expr(
                svc.config
                    .auth
                    .password
                    .as_ref()
                    .expect("auth_method == Password implies password is set"),
            );
            quote! { crate::_rb_sftp::SftpAuth::Password { password: #pw } }
        }
        SftpAuthMethod::PrivateKey => {
            let path = url_expr(
                svc.config
                    .auth
                    .private_key
                    .as_ref()
                    .expect("auth_method == PrivateKey implies private_key is set"),
            );
            quote! { crate::_rb_sftp::SftpAuth::PrivateKey { path: #path, passphrase: #passphrase } }
        }
        SftpAuthMethod::PrivateKeyPem => {
            let pem = url_expr(
                svc.config
                    .auth
                    .private_key_pem
                    .as_ref()
                    .expect("auth_method == PrivateKeyPem implies private_key_pem is set"),
            );
            quote! { crate::_rb_sftp::SftpAuth::PrivateKeyPem { pem: #pem, passphrase: #passphrase } }
        }
    }
}

/// Emit the `_rb_sftp` module hosting `SftpService` + `SftpAuth` on the
/// dist side. Kept private (no `pub` re-export) — only `AppState` references
/// the types today; future `sftp.*` blocks will reach in through this path.
///
/// The types are intentionally inert in v1: they own the config and expose
/// `connect()` so a future operation block can open an SFTP session. v1 is
/// "one session per call" — pooling lands later when a real workload needs it.
fn render_rb_sftp_module() -> TokenStream {
    quote! {
        #[allow(dead_code)]
        pub mod _rb_sftp {
            /// Resolved configuration for a single SFTP target. Cheap to clone
            /// via `Arc<SftpService>`; one such handle per declared service
            /// lives on `AppState`. v1 opens one session per call — pooling
            /// is deferred until a real workload justifies it.
            #[derive(Debug, Clone)]
            pub struct SftpService {
                pub host: String,
                pub port: u16,
                pub user: String,
                pub auth: SftpAuth,
                /// `SHA256:...` server fingerprint. `None` means "trust on
                /// first use" (only reached in dev mode \u{2014} release
                /// startup errors out when this is absent).
                pub host_key_fingerprint: Option<String>,
            }

            /// Three concrete auth methods the manifest accepts; mirror of
            /// the `SftpAuthMethod` enum on the rublocks codegen side.
            #[derive(Debug, Clone)]
            pub enum SftpAuth {
                Password {
                    password: String,
                },
                PrivateKey {
                    path: String,
                    passphrase: Option<String>,
                },
                PrivateKeyPem {
                    pem: String,
                    passphrase: Option<String>,
                },
            }
        }
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
/// is loaded at a glance. `language` is baked into `<html lang>` so the HTML
/// is self-describing without any per-route override.
fn render_dev_index_html(app_name: &str, description: &str, language: &str) -> String {
    let name = html_escape(app_name);
    let desc = html_escape(description);
    format!(
        "<!DOCTYPE html>\n\
         <html lang=\"{language}\">\n\
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

    /// Test helper: writes `main.json` and loads the manifest.
    ///
    /// Injects `"encoding": "utf-8"` into the JSON body when missing so the
    /// codegen tests don't have to spell it out — the manifest module owns
    /// the dedicated tests that exercise the encoding contract directly.
    fn manifest_from(project_dir: &Path, main_json: &str) -> Manifest {
        let body = inject_required_fields(main_json);
        fs::write(project_dir.join("main.json"), body).unwrap();
        Manifest::load(project_dir).expect("manifest")
    }

    /// Backfill every now-mandatory top-level field a codegen-focused test
    /// would otherwise have to spell out: `version`, `description`,
    /// `language`, `encoding`. Tests that exercise the validation of a
    /// specific field set it explicitly and the helper leaves the original
    /// value alone.
    fn inject_required_fields(main_json: &str) -> String {
        let trimmed = main_json.trim_start();
        debug_assert!(trimmed.starts_with('{'));
        let mut out = String::from("{ ");
        if !main_json.contains("\"version\"") {
            out.push_str("\"version\": \"0.0.0\", ");
        }
        if !main_json.contains("\"description\"") {
            out.push_str("\"description\": \"test\", ");
        }
        if !main_json.contains("\"language\"") {
            out.push_str("\"language\": \"en-US\", ");
        }
        if !main_json.contains("\"encoding\"") {
            out.push_str("\"encoding\": \"utf-8\", ");
        }
        if !main_json.contains("\"logging\"") {
            out.push_str("\"logging\": { \"level\": \"info\" }, ");
        }
        out.push_str(&trimmed[1..]);
        out
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
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test" }"#,
        );
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
            main_rs.contains("cel :: Value :: Bool(true)")
                || main_rs.contains("cel::Value::Bool(true)"),
            "guard check must look for Bool(true):\n{main_rs}"
        );
        assert!(
            main_rs.contains("\"token\""),
            "input field `token` must be bound in the CEL context:\n{main_rs}"
        );
        let toml = fs::read_to_string(dist.join("Cargo.toml")).unwrap();
        assert!(
            toml.contains("cel = "),
            "Cargo.toml must pull the cel crate:\n{toml}"
        );
        assert!(
            !toml.contains("cel-interpreter"),
            "Cargo.toml must not pull the legacy cel-interpreter crate:\n{toml}"
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
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test" }"#,
        );
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
        // (a) a static `OnceLock<cel::Program>` per site,
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
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test" }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");
        assert!(
            main_rs.contains("cel::Program::compile"),
            "validate must invoke cel at runtime:\n{main_rs}"
        );
        assert!(
            main_rs.contains("OnceLock::<cel::Program>")
                || main_rs.contains("OnceLock<cel::Program>"),
            "compiled program must be cached in OnceLock:\n{main_rs}"
        );
        assert!(
            main_rs.contains("\"title\""),
            "field must be bound under its declared name in the CEL context:\n{main_rs}"
        );
        let toml = fs::read_to_string(dist.join("Cargo.toml")).unwrap();
        assert!(
            toml.contains("cel = "),
            "Cargo.toml must pull the cel crate:\n{toml}"
        );
        assert!(
            !toml.contains("cel-interpreter"),
            "Cargo.toml must not pull the legacy cel-interpreter crate:\n{toml}"
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

        // Route is registered against the Axum router and the handler is
        // emitted under the derived name.
        assert!(main_rs.contains(r#"router.route("/", get(route_home))"#));
        assert!(main_rs.contains("async fn route_home"));
        // No user route owns /health, but the placeholder for / is suppressed
        // because the user does own /.
        assert!(!main_rs.contains("dev_index"));
    }

    #[test]
    fn emit_forces_client_encoding_utf8_on_postgres() {
        // Postgres connections must carry `client_encoding=UTF8` so the
        // session encoding matches the project-wide contract, regardless
        // of the cluster's locale setting (issue #13).
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "services": { "postgres": { "url": "env:DATABASE_URL" } } }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        assert!(
            main_rs.contains("client_encoding=UTF8"),
            "postgres URL should carry client_encoding=UTF8"
        );
        // The augmentation uses '?' or '&' depending on prior query params,
        // so both separators appear in the conditional. Match on the
        // separator pick to lock the runtime concat path.
        assert!(main_rs.contains(r#"if base.contains('?')"#));
    }

    #[test]
    fn emit_does_not_force_client_encoding_on_mysql() {
        // MySQL/MariaDB negotiate encoding via a different URL parameter
        // (`charset=utf8mb4`), left to the user for now. Lock the negative.
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "services": { "db": { "kind": "mysql", "url": "env:DATABASE_URL" } } }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        assert!(!main_rs.contains("client_encoding=UTF8"));
    }

    #[test]
    fn emit_wires_encoding_middleware_and_module() {
        // Every generated app must carry the encoding contract — the
        // `_rb_encoding` module is emitted and applied as a router layer,
        // and the `CHARSET` constant reflects the manifest declaration
        // (only `utf-8` accepted today, see `docs/encoding.md`).
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(dir.path(), r#"{ "name": "rb_test" }"#);
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        assert!(main_rs.contains("pub mod _rb_encoding"));
        assert!(main_rs.contains("axum::middleware::from_fn(_rb_encoding::enforce)"));
        assert!(main_rs.contains(r#"pub const CHARSET: &str = "utf-8""#));
        assert!(
            main_rs.contains("StatusCode :: UNSUPPORTED_MEDIA_TYPE")
                || main_rs.contains("StatusCode::UNSUPPORTED_MEDIA_TYPE"),
        );
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

    /// Acceptance criterion for issue #14: the project `language` is baked
    /// into `<html lang="...">` and emitted as `Content-Language` on every
    /// generated HTML response.
    #[test]
    fn emit_threads_language_into_dev_index_and_pages() {
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("about.json"),
            r#"{ "path": "/about", "method": "GET", "kind": "page", "template": "about.html" }"#,
        )
        .unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test", "language": "fr-FR" }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");
        // Dev index page declares the project locale on <html>.
        assert!(
            main_rs.contains("<html lang=\\\"fr-FR\\\""),
            "expected <html lang=\\\"fr-FR\\\"> in dev_index, got:\n{main_rs}"
        );
        // Both the dev_index and the page route emit Content-Language with
        // the project tag. We assert the header constant + the literal
        // appear, which only the codegen-side wiring would produce.
        assert!(main_rs.contains("CONTENT_LANGUAGE"));
        assert!(
            main_rs.matches("\"fr-FR\"").count() >= 2,
            "language tag should be quoted in both the dev_index and the page route, got:\n{main_rs}"
        );
    }

    /// Verify the codegen rejects a missing `language` field upstream via
    /// the manifest loader so the dev overlay can point at `main.json`.
    #[test]
    fn manifest_load_rejects_missing_language() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("main.json"),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test" }"#,
        )
        .unwrap();
        let err = Manifest::load(dir.path()).unwrap_err();
        assert_eq!(err.file, dir.path().join("main.json"));
        assert!(
            err.message.contains("language"),
            "missing-field error should name `language`, got: {}",
            err.message
        );
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
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "2.3.4", "description": "test" }"#,
        );
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
        // Layout binds `current_year` from a `time.now` process block —
        // exercises layout-side block execution in the same test.
        fs::write(
            layouts_dir.join("main.json"),
            r#"{
                "name": "main",
                "template": "layout.html",
                "requires": { "page_title": { "type": "string" } },
                "process": [
                    { "name": "year", "block": "time.now", "format": "%Y" }
                ],
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
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test", "services": { "postgres": { "url": "env:DATABASE_URL" } } }"#,
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
        // `db.find_many` executes at request time against the declared
        // postgres pool: the handler emits a sqlx query builder bound to
        // `__state.pg` and assigns the result to the `posts`
        // page-context field.
        assert!(
            main_rs.contains("sqlx::QueryBuilder"),
            "find_many must emit a sqlx QueryBuilder:\n{main_rs}"
        );
        assert!(
            main_rs.contains("posts: __block_posts.clone()"),
            "page ctx must read from the block binding:\n{main_rs}"
        );
        // Handler returns Html<String> rendered via Askama, with livereload
        // injection wrapping the rendered template.
        assert!(main_rs.contains("axum::response::Html"));
        assert!(main_rs.contains("ctx.render()") || main_rs.contains("ctx\n        .render()"));
        assert!(main_rs.contains("maybe_inject_dev_snippet"));
    }

    #[test]
    fn emit_wires_db_find_one_with_on_missing_short_circuit() {
        // `db.find_one` emits a `fetch_optional` + match arm. The
        // `on_missing` sub-block renders inline as the None branch, so a
        // 404 `error` block short-circuits the handler in-place.
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("show.json"),
            r#"{
                "path": "/api/posts/:slug",
                "method": "GET",
                "kind": "api",
                "input": { "path": { "slug": { "type": "string", "required": true } } },
                "process": [
                    {
                        "name": "post",
                        "block": "db.find_one",
                        "table": "posts",
                        "where": { "slug": "$input.path.slug" },
                        "on_missing": {
                            "block": "error",
                            "status": 404,
                            "code": "post_not_found"
                        }
                    }
                ],
                "output": { "slug": "$post.slug" }
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
                "fields": { "id": { "type": "uuid" }, "slug": { "type": "string" } }
            }"#,
        )
        .unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test", "services": { "postgres": { "url": "env:DATABASE_URL" } } }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");
        assert!(
            main_rs.contains("fetch_optional"),
            "find_one uses fetch_optional:\n{main_rs}"
        );
        assert!(
            main_rs.contains("404u16") && main_rs.contains("\"post_not_found\""),
            "on_missing emits structured 404:\n{main_rs}"
        );
        assert!(
            main_rs.contains("axum::Json"),
            "api response wraps output in Json:\n{main_rs}"
        );
    }

    #[test]
    fn emit_wires_db_insert_with_value_refs() {
        // db.insert binds `$input.body.X` and `$<block>.X` references
        // into the INSERT statement via QueryBuilder::push_bind.
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("create.json"),
            r#"{
                "path": "/posts/:slug/comments",
                "method": "POST",
                "kind": "page",
                "input": {
                    "path": { "slug": { "type": "string", "required": true } },
                    "body": {
                        "form": true,
                        "fields": { "body": { "type": "text", "required": true } }
                    }
                },
                "process": [
                    { "name": "post", "block": "db.find_one", "table": "posts", "where": { "slug": "$input.path.slug" } },
                    {
                        "block": "db.insert",
                        "table": "comments",
                        "values": { "post_id": "$post.id", "body": "$input.body.body" }
                    }
                ],
                "redirect": { "to": "/posts/$input.path.slug", "status": 303 }
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
                "fields": { "id": { "type": "uuid" }, "slug": { "type": "string" } }
            }"#,
        )
        .unwrap();
        fs::write(
            models_dir.join("comment.json"),
            r#"{
                "name": "Comment",
                "table": "comments",
                "fields": {
                    "id": { "type": "uuid" },
                    "post_id": { "type": "uuid" },
                    "body": { "type": "text" }
                }
            }"#,
        )
        .unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test", "services": { "postgres": { "url": "env:DATABASE_URL" } } }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");
        assert!(
            main_rs.contains("INSERT INTO \\\"comments\\\""),
            "INSERT statement emitted:\n{main_rs}"
        );
        assert!(
            main_rs.contains("__block_post.id"),
            "post_id binds the prior block's id field:\n{main_rs}"
        );
        assert!(
            main_rs.contains("_body.body"),
            "body binds the input body field:\n{main_rs}"
        );
        assert!(
            main_rs.contains("_rb_runtime::redirect")
                && main_rs.contains("303u16")
                && main_rs.contains("_path.slug"),
            "redirect substitutes the path slug at request time:\n{main_rs}"
        );
    }

    #[test]
    fn emit_wires_api_output_projection() {
        // kind:api routes serialise their `output:` spec as a JSON map
        // built at request time from prior block bindings. Nested
        // objects recurse; literal strings round-trip as JSON strings.
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("show.json"),
            r#"{
                "path": "/api/posts/:slug",
                "method": "GET",
                "kind": "api",
                "input": { "path": { "slug": { "type": "string", "required": true } } },
                "process": [
                    { "name": "post", "block": "db.find_one", "table": "posts", "where": { "slug": "$input.path.slug" } }
                ],
                "output": {
                    "id": "$post.id",
                    "meta": { "kind": "post" }
                }
            }"#,
        )
        .unwrap();
        let models_dir = dir.path().join("models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("post.json"),
            r#"{ "name": "Post", "table": "posts", "fields": { "id": { "type": "uuid" }, "slug": { "type": "string" } } }"#,
        )
        .unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test", "services": { "postgres": { "url": "env:DATABASE_URL" } } }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");
        assert!(
            main_rs.contains("\"id\".to_string()"),
            "output key id round-trips as string:\n{main_rs}"
        );
        assert!(
            main_rs.contains("__block_post.id"),
            "leaf resolves to the block binding's field:\n{main_rs}"
        );
        assert!(
            main_rs.contains("\"meta\".to_string()") && main_rs.contains("\"kind\".to_string()"),
            "nested object recursively renders into the json projection:\n{main_rs}"
        );
    }

    #[test]
    fn emit_wires_field_validation_at_insert_time() {
        // A model field with a CEL `validate:` predicate gets a runtime
        // check before db.insert binds it. Failure short-circuits with
        // a 422 + the field name.
        let dir = TempDir::new().unwrap();
        let models_dir = dir.path().join("models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("comment.json"),
            r#"{
                "name": "Comment",
                "table": "comments",
                "fields": {
                    "body": { "type": "text", "validate": "size(body) > 0" }
                }
            }"#,
        )
        .unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("create.json"),
            r#"{
                "path": "/comments",
                "method": "POST",
                "kind": "api",
                "input": { "body": { "fields": { "body": { "type": "text", "required": true } } } },
                "process": [
                    {
                        "block": "db.insert",
                        "table": "comments",
                        "values": { "body": "$input.body.body" }
                    }
                ]
            }"#,
        )
        .unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test", "services": { "postgres": { "url": "env:DATABASE_URL" } } }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");
        assert!(
            main_rs.contains("cel::Program::compile(\"size(body) > 0\")"),
            "insert site compiles the field's CEL validator:\n{main_rs}"
        );
        assert!(
            main_rs.contains("field_validation_error"),
            "false eval triggers the 422 helper:\n{main_rs}"
        );
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
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test", "language": "en-US", "encoding": "utf-8", "logging": { "level": "info" } }"#,
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
    fn cargo_toml_pulls_in_cel_when_a_guard_block_is_used() {
        // The `guard` block evaluates a CEL predicate at request time, so
        // the dist crate must depend on the `cel` crate. Projects with no
        // CEL site stay free of the dependency.
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
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "test" }"#,
        );
        let toml = render_cargo_toml(&manifest, false);
        assert!(
            toml.contains("cel = "),
            "guard block must pull in the cel crate: {toml}"
        );
        assert!(
            !toml.contains("cel-interpreter"),
            "guard block must not pull the legacy cel-interpreter crate: {toml}"
        );
    }

    #[test]
    fn cargo_toml_omits_cel_when_no_cel_site_exists() {
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
        assert!(
            !toml.contains("cel = ") && !toml.contains("cel-interpreter"),
            "CEL-free project must not pull in the cel crate: {toml}"
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
        assert!(
            main_rs
                .contains(r#"<meta name=\"description\" content=\"A blog with public posts.\">"#)
        );
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

    #[test]
    fn cargo_toml_pulls_in_russh_when_sftp_service_declared() {
        // Acceptance: russh + russh-sftp must land in the dist `Cargo.toml`
        // only when an SFTP service is present (or — in follow-up issues —
        // when an `sftp.*` block is referenced). This test pins the former.
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{
                "name": "rb_test",
                "version": "0.1.0",
                "description": "test",
                "services": {
                    "files": {
                        "kind": "sftp",
                        "host": "env:SFTP_HOST",
                        "user": "env:SFTP_USER",
                        "auth": { "password": "env:SFTP_PASSWORD" },
                        "host_key_fingerprint": "SHA256:abc"
                    }
                }
            }"#,
        );
        let toml = render_cargo_toml(&manifest, false);
        assert!(toml.contains("russh ="), "russh dep missing: {toml}");
        assert!(
            toml.contains("russh-sftp ="),
            "russh-sftp dep missing: {toml}"
        );
    }

    #[test]
    fn cargo_toml_omits_russh_when_no_sftp_service() {
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.1.0", "description": "test" }"#,
        );
        let toml = render_cargo_toml(&manifest, false);
        assert!(
            !toml.contains("russh"),
            "russh leaked into clean project: {toml}"
        );
    }

    #[test]
    fn emit_wires_arc_sftp_service_on_appstate_per_declared_service() {
        // Acceptance: one `Arc<SftpService>` field per declared SFTP service,
        // emitted on AppState with the user-chosen name as the field id.
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{
                "name": "rb_test",
                "version": "0.1.0",
                "description": "test",
                "services": {
                    "files": {
                        "kind": "sftp",
                        "host": "sftp.example.com",
                        "user": "rublocks",
                        "auth": { "password": "env:SFTP_PASSWORD" },
                        "host_key_fingerprint": "SHA256:abc"
                    },
                    "backups": {
                        "kind": "sftp",
                        "host": "env:BACKUP_HOST",
                        "user": "env:BACKUP_USER",
                        "auth": { "private_key": "env:BACKUP_KEY_PATH" },
                        "host_key_fingerprint": "env:BACKUP_FINGERPRINT"
                    }
                }
            }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");
        // Module hosting the SftpService + SftpAuth types.
        assert!(main_rs.contains("pub mod _rb_sftp"));
        assert!(main_rs.contains("pub struct SftpService"));
        assert!(main_rs.contains("pub enum SftpAuth"));
        // One Arc<SftpService> field per declared service. prettyplease
        // emits the canonical `Arc<crate::_rb_sftp::SftpService>` form.
        assert!(main_rs.contains("pub files: std::sync::Arc<crate::_rb_sftp::SftpService>"));
        assert!(main_rs.contains("pub backups: std::sync::Arc<crate::_rb_sftp::SftpService>"));
    }

    #[test]
    fn emit_initialises_sftp_service_with_password_auth() {
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{
                "name": "rb_test",
                "version": "0.1.0",
                "description": "test",
                "services": {
                    "files": {
                        "kind": "sftp",
                        "host": "sftp.example.com",
                        "user": "rublocks",
                        "auth": { "password": "env:SFTP_PASSWORD" },
                        "host_key_fingerprint": "SHA256:abc"
                    }
                }
            }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");
        // Password env-var resolution lands in main().
        assert!(main_rs.contains("std::env::var(\"SFTP_PASSWORD\")"));
        // Auth variant is the password form.
        assert!(main_rs.contains("SftpAuth::Password"));
        // Fingerprint literal is embedded.
        assert!(main_rs.contains("SHA256:abc"));
    }

    #[test]
    fn emit_initialises_sftp_service_with_private_key_auth() {
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{
                "name": "rb_test",
                "version": "0.1.0",
                "description": "test",
                "services": {
                    "files": {
                        "kind": "sftp",
                        "host": "sftp.example.com",
                        "user": "rublocks",
                        "auth": {
                            "private_key": "/keys/id_ed25519",
                            "passphrase": "env:SFTP_KEY_PASSPHRASE"
                        },
                        "host_key_fingerprint": "SHA256:abc"
                    }
                }
            }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");
        assert!(main_rs.contains("SftpAuth::PrivateKey"));
        assert!(main_rs.contains("/keys/id_ed25519"));
        assert!(main_rs.contains("std::env::var(\"SFTP_KEY_PASSPHRASE\")"));
    }

    #[test]
    fn emit_initialises_sftp_service_with_private_key_pem_auth() {
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{
                "name": "rb_test",
                "version": "0.1.0",
                "description": "test",
                "services": {
                    "files": {
                        "kind": "sftp",
                        "host": "sftp.example.com",
                        "user": "rublocks",
                        "auth": { "private_key_pem": "env:SFTP_KEY_PEM" },
                        "host_key_fingerprint": "SHA256:abc"
                    }
                }
            }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");
        assert!(main_rs.contains("SftpAuth::PrivateKeyPem"));
        assert!(main_rs.contains("std::env::var(\"SFTP_KEY_PEM\")"));
    }

    #[test]
    fn emit_warns_and_tofus_when_fingerprint_missing_in_dev() {
        // host_key_fingerprint omitted: dev mode must TOFU (warn + None);
        // release startup must error out. The branch lives in the dist
        // binary so the same generated code adapts to either runtime.
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{
                "name": "rb_test",
                "version": "0.1.0",
                "description": "test",
                "services": {
                    "files": {
                        "kind": "sftp",
                        "host": "sftp.example.com",
                        "user": "rublocks",
                        "auth": { "password": "env:SFTP_PASSWORD" }
                    }
                }
            }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");
        // Both branches of the dev/release fork are baked in.
        assert!(
            main_rs.contains("RUBLOCKS_DEV"),
            "missing-fingerprint snippet must guard on dev flag: {main_rs}"
        );
        assert!(
            main_rs.contains("host_key_fingerprint is required in release builds"),
            "release-mode error must be present: {main_rs}"
        );
        assert!(
            main_rs.contains("trusting on first use"),
            "dev-mode TOFU warning must be present: {main_rs}"
        );
    }

    // ---- issue #17: structured logging ----

    #[test]
    fn cargo_toml_pulls_tracing_and_subscriber() {
        // Every generated crate ships the tracing pipeline — `main.json.logging`
        // is mandatory so no project can opt out.
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "t" }"#,
        );
        let toml = render_cargo_toml(&manifest, false);
        assert!(toml.contains("tracing = \"0.1\""), "got: {toml}");
        assert!(toml.contains("tracing-subscriber"), "got: {toml}");
        assert!(toml.contains("\"json\""), "got: {toml}");
        assert!(toml.contains("\"env-filter\""), "got: {toml}");
        // tower-http always pulls the `trace` feature on top of `set-header`.
        assert!(toml.contains("\"trace\""), "got: {toml}");
    }

    #[test]
    fn emit_initialises_subscriber_with_declared_level() {
        // The level chosen in `main.json.logging.level` flows into
        // `tracing_subscriber::fmt().with_max_level(Level::X)`.
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "t", "logging": { "level": "debug" } }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");
        assert!(
            main_rs.contains("tracing_subscriber::fmt()")
                || main_rs.contains("tracing_subscriber :: fmt ()"),
            "got: {main_rs}"
        );
        assert!(
            main_rs.contains("Level::DEBUG") || main_rs.contains("Level :: DEBUG"),
            "with_max_level must match the manifest level, got: {main_rs}"
        );
        assert!(
            main_rs.contains("flatten_event(true)") || main_rs.contains("flatten_event (true)"),
            "NDJSON contract: event fields flatten to the top of every line, got: {main_rs}"
        );
    }

    #[test]
    fn emit_wires_request_trace_layer() {
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "t" }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");
        assert!(
            main_rs.contains("TraceLayer") || main_rs.contains("tower_http :: trace"),
            "got: {main_rs}"
        );
        assert!(
            main_rs.contains("request_id"),
            "request span must carry request_id, got: {main_rs}"
        );
        assert!(
            main_rs.contains("MatchedPath"),
            "request span must use MatchedPath to capture the route pattern, got: {main_rs}"
        );
    }

    #[test]
    fn emit_resolves_logging_include_env_at_startup() {
        // `include.env: "env:RUST_ENV"` becomes `std::env::var("RUST_ENV")...`
        // inside the root span set up at startup. Literal values pass through.
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "t", "logging": { "level": "info", "include": { "service": "rb_test", "env": "env:RUST_ENV" } } }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");
        assert!(
            main_rs.contains("\"RUST_ENV\""),
            "env: prefix must resolve to env::var(VAR), got: {main_rs}"
        );
        assert!(
            main_rs.contains("\"rb_test\""),
            "literal include value must appear verbatim, got: {main_rs}"
        );
    }

    #[test]
    fn emit_wraps_each_block_in_a_tracing_span() {
        // A route with one guard + one db.find_many. Each block gets a
        // `info_span!("block", block = "...", table = "...")` prelude plus a
        // success `info!` event at the end of its body. Error returns inside
        // a block emit their own `error!` event before returning the response.
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        let models_dir = dir.path().join("models");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("post.json"),
            r#"{ "name": "Post", "table": "posts", "fields": { "id": { "type": "uuid" } } }"#,
        )
        .unwrap();
        fs::write(
            routes_dir.join("posts.json"),
            r#"{
                "path": "/api/posts",
                "method": "GET",
                "kind": "api",
                "process": [
                    { "block": "guard", "if": "true" },
                    { "name": "posts", "block": "db.find_many", "table": "posts" }
                ]
            }"#,
        )
        .unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{
                "name": "rb_test",
                "version": "0.0.0",
                "description": "t",
                "services": { "postgres": { "url": "env:DATABASE_URL" } }
            }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");
        assert!(
            main_rs.contains("info_span!")
                || main_rs.contains("info_span !")
                || main_rs.contains("tracing :: info_span"),
            "each block must open an info_span!, got: {main_rs}"
        );
        assert!(
            main_rs.contains("\"guard\""),
            "guard block must surface block=\"guard\", got: {main_rs}"
        );
        assert!(
            main_rs.contains("\"db.find_many\""),
            "db.find_many block must surface its kind id, got: {main_rs}"
        );
        assert!(
            main_rs.contains("\"posts\""),
            "db.find_many's `table` must appear as a span field, got: {main_rs}"
        );
        assert!(
            main_rs.contains("\"ok\""),
            "success info! must emit msg=\"ok\", got: {main_rs}"
        );
        assert!(
            main_rs.contains("\"block failed\""),
            "block error path must emit msg=\"block failed\", got: {main_rs}"
        );
        assert!(
            main_rs.contains("duration_us"),
            "every event must carry duration_us, got: {main_rs}"
        );
    }

    #[test]
    fn emit_rb_log_module_with_error_chain_and_backtrace() {
        let dir = TempDir::new().unwrap();
        let manifest = manifest_from(
            dir.path(),
            r#"{ "name": "rb_test", "version": "0.0.0", "description": "t" }"#,
        );
        let dist = dir.path().join("dist");
        emit(&manifest, dir.path(), &dist).unwrap();
        let main_rs = fs::read_to_string(dist.join("src/main.rs")).unwrap();
        let _: syn::File = syn::parse_str(&main_rs).expect("generated main.rs must parse");
        assert!(main_rs.contains("pub mod _rb_log"), "got: {main_rs}");
        assert!(main_rs.contains("fn error_chain"), "got: {main_rs}");
        assert!(main_rs.contains("fn error_backtrace"), "got: {main_rs}");
        assert!(main_rs.contains("fn rand_request_id"), "got: {main_rs}");
    }

    /// Snapshot tests for codegen.
    ///
    /// These tests freeze the exact Rust output (post-`prettyplease`) for a
    /// curated set of minimal projects. They are the safety net for
    /// cross-cutting regressions that the assertion-based tests above can
    /// miss — a tweak to one helper that silently rewrites every emitted
    /// handler shows up here as a snapshot diff. Update with
    /// `cargo insta review`. See `docs/testing.md`.
    mod snapshot_tests {
        use super::*;

        fn build_main_rs(write_project: impl FnOnce(&Path)) -> String {
            let dir = TempDir::new().unwrap();
            write_project(dir.path());
            let manifest_path = dir.path().join("main.json");
            if !manifest_path.exists() {
                fs::write(
                    &manifest_path,
                    r#"{ "name": "rb_test", "version": "0.0.0", "description": "snapshot test", "language": "en-US", "encoding": "utf-8", "logging": { "level": "info" } }"#,
                )
                .unwrap();
            }
            let manifest = Manifest::load(dir.path()).unwrap();
            let dist = dir.path().join("dist");
            emit(&manifest, dir.path(), &dist).unwrap();
            fs::read_to_string(dist.join("src/main.rs")).unwrap()
        }

        #[test]
        fn emit_minimal_manifest() {
            let main_rs = build_main_rs(|_| {});
            insta::assert_snapshot!(main_rs);
        }

        #[test]
        fn emit_cargo_toml_minimal() {
            let dir = TempDir::new().unwrap();
            let manifest = manifest_from(
                dir.path(),
                r#"{ "name": "rb_test", "version": "0.0.0", "description": "snapshot test", "language": "en-US", "encoding": "utf-8", "logging": { "level": "info" } }"#,
            );
            let toml = render_cargo_toml(&manifest, false);
            insta::assert_snapshot!(toml);
        }

        #[test]
        fn emit_cargo_toml_with_postgres_and_migrations() {
            let dir = TempDir::new().unwrap();
            let manifest = manifest_from(
                dir.path(),
                r#"{ "name": "rb_test", "version": "0.0.0", "description": "snapshot test", "language": "en-US", "encoding": "utf-8", "logging": { "level": "info" }, "services": { "postgres": { "url": "env:DATABASE_URL" } } }"#,
            );
            let toml = render_cargo_toml(&manifest, true);
            insta::assert_snapshot!(toml);
        }

        #[test]
        fn emit_model_struct() {
            let main_rs = build_main_rs(|root| {
                let models = root.join("models");
                fs::create_dir_all(&models).unwrap();
                fs::write(
                    models.join("post.json"),
                    r#"{
                        "name": "Post",
                        "table": "posts",
                        "fields": {
                            "id":         { "type": "uuid" },
                            "title":      { "type": "string" },
                            "body":       { "type": "string", "nullable": true },
                            "published":  { "type": "bool" },
                            "created_at": { "type": "timestamptz" }
                        }
                    }"#,
                )
                .unwrap();
                fs::write(
                    root.join("main.json"),
                    r#"{ "name": "rb_test", "version": "0.0.0", "description": "snapshot test", "language": "en-US", "encoding": "utf-8", "logging": { "level": "info" }, "services": { "postgres": { "url": "env:DATABASE_URL" } } }"#,
                )
                .unwrap();
            });
            insta::assert_snapshot!(main_rs);
        }

        #[test]
        fn emit_route_api_get() {
            let main_rs = build_main_rs(|root| {
                let routes = root.join("routes");
                fs::create_dir_all(&routes).unwrap();
                fs::write(
                    routes.join("ping.json"),
                    r#"{
                        "path": "/ping",
                        "method": "GET",
                        "kind": "api",
                        "output": { "status": "ok" }
                    }"#,
                )
                .unwrap();
            });
            insta::assert_snapshot!(main_rs);
        }

        #[test]
        fn emit_route_page_get() {
            let main_rs = build_main_rs(|root| {
                let routes = root.join("routes");
                fs::create_dir_all(&routes).unwrap();
                fs::write(
                    routes.join("home.json"),
                    r#"{
                        "path": "/",
                        "method": "GET",
                        "kind": "page",
                        "template": "home.html"
                    }"#,
                )
                .unwrap();
                let templates = root.join("templates");
                fs::create_dir_all(&templates).unwrap();
                fs::write(templates.join("home.html"), "<h1>home</h1>").unwrap();
            });
            insta::assert_snapshot!(main_rs);
        }

        #[test]
        fn emit_layout_inheritance() {
            let main_rs = build_main_rs(|root| {
                let layouts = root.join("layouts");
                fs::create_dir_all(&layouts).unwrap();
                fs::write(
                    layouts.join("base.json"),
                    r#"{
                        "name": "base",
                        "template": "layouts/base.html"
                    }"#,
                )
                .unwrap();
                let routes = root.join("routes");
                fs::create_dir_all(&routes).unwrap();
                fs::write(
                    routes.join("about.json"),
                    r#"{
                        "path": "/about",
                        "method": "GET",
                        "kind": "page",
                        "template": "about.html",
                        "layout": "base"
                    }"#,
                )
                .unwrap();
                let templates = root.join("templates");
                fs::create_dir_all(templates.join("layouts")).unwrap();
                fs::write(templates.join("about.html"), "<p>about</p>").unwrap();
                fs::write(
                    templates.join("layouts").join("base.html"),
                    "<!doctype html><body>{{ content|safe }}</body>",
                )
                .unwrap();
            });
            insta::assert_snapshot!(main_rs);
        }

        #[test]
        fn emit_block_db_find_many() {
            let main_rs = build_main_rs(|root| {
                let models = root.join("models");
                fs::create_dir_all(&models).unwrap();
                fs::write(
                    models.join("post.json"),
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
                let routes = root.join("routes");
                fs::create_dir_all(&routes).unwrap();
                fs::write(
                    routes.join("posts.json"),
                    r#"{
                        "path": "/posts",
                        "method": "GET",
                        "kind": "api",
                        "process": [
                            { "name": "posts", "block": "db.find_many", "table": "posts" }
                        ],
                        "output": { "posts": "$posts" }
                    }"#,
                )
                .unwrap();
                fs::write(
                    root.join("main.json"),
                    r#"{ "name": "rb_test", "version": "0.0.0", "description": "snapshot test", "language": "en-US", "encoding": "utf-8", "logging": { "level": "info" }, "services": { "postgres": { "url": "env:DATABASE_URL" } } }"#,
                )
                .unwrap();
            });
            insta::assert_snapshot!(main_rs);
        }

        #[test]
        fn emit_block_db_find_one() {
            let main_rs = build_main_rs(|root| {
                let models = root.join("models");
                fs::create_dir_all(&models).unwrap();
                fs::write(
                    models.join("post.json"),
                    r#"{
                        "name": "Post",
                        "table": "posts",
                        "fields": {
                            "id":   { "type": "uuid" },
                            "slug": { "type": "string" }
                        }
                    }"#,
                )
                .unwrap();
                let routes = root.join("routes");
                fs::create_dir_all(&routes).unwrap();
                fs::write(
                    routes.join("post.json"),
                    r#"{
                        "path": "/posts/{slug}",
                        "method": "GET",
                        "kind": "api",
                        "input": { "path": { "slug": { "type": "string", "required": true } } },
                        "process": [
                            { "name": "post", "block": "db.find_one", "table": "posts", "where": { "slug": "$input.path.slug" } }
                        ],
                        "output": { "post": "$post" }
                    }"#,
                )
                .unwrap();
                fs::write(
                    root.join("main.json"),
                    r#"{ "name": "rb_test", "version": "0.0.0", "description": "snapshot test", "language": "en-US", "encoding": "utf-8", "logging": { "level": "info" }, "services": { "postgres": { "url": "env:DATABASE_URL" } } }"#,
                )
                .unwrap();
            });
            insta::assert_snapshot!(main_rs);
        }

        #[test]
        fn emit_block_db_insert() {
            let main_rs = build_main_rs(|root| {
                let models = root.join("models");
                fs::create_dir_all(&models).unwrap();
                fs::write(
                    models.join("post.json"),
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
                let routes = root.join("routes");
                fs::create_dir_all(&routes).unwrap();
                fs::write(
                    routes.join("create.json"),
                    r#"{
                        "path": "/posts",
                        "method": "POST",
                        "kind": "api",
                        "input": { "body": { "title": { "type": "string", "required": true } } },
                        "process": [
                            {
                                "block": "db.insert",
                                "table": "posts",
                                "values": { "title": "$input.body.title" }
                            }
                        ],
                        "output": { "ok": true }
                    }"#,
                )
                .unwrap();
                fs::write(
                    root.join("main.json"),
                    r#"{ "name": "rb_test", "version": "0.0.0", "description": "snapshot test", "language": "en-US", "encoding": "utf-8", "logging": { "level": "info" }, "services": { "postgres": { "url": "env:DATABASE_URL" } } }"#,
                )
                .unwrap();
            });
            insta::assert_snapshot!(main_rs);
        }

        #[test]
        fn emit_block_error() {
            let main_rs = build_main_rs(|root| {
                let routes = root.join("routes");
                fs::create_dir_all(&routes).unwrap();
                fs::write(
                    routes.join("teapot.json"),
                    r#"{
                        "path": "/teapot",
                        "method": "GET",
                        "kind": "api",
                        "process": [
                            { "block": "error", "status": 418, "code": "i_am_a_teapot" }
                        ]
                    }"#,
                )
                .unwrap();
            });
            insta::assert_snapshot!(main_rs);
        }

        #[test]
        fn emit_block_guard() {
            let main_rs = build_main_rs(|root| {
                let routes = root.join("routes");
                fs::create_dir_all(&routes).unwrap();
                fs::write(
                    routes.join("admin.json"),
                    r#"{
                        "path": "/admin",
                        "method": "GET",
                        "kind": "api",
                        "input": { "query": { "token": { "type": "string", "required": true } } },
                        "process": [
                            { "block": "guard", "if": "token == \"open-sesame\"" }
                        ],
                        "output": { "ok": true }
                    }"#,
                )
                .unwrap();
            });
            insta::assert_snapshot!(main_rs);
        }

        #[test]
        fn emit_block_time_now() {
            let main_rs = build_main_rs(|root| {
                let routes = root.join("routes");
                fs::create_dir_all(&routes).unwrap();
                fs::write(
                    routes.join("now.json"),
                    r#"{
                        "path": "/now",
                        "method": "GET",
                        "kind": "api",
                        "process": [
                            { "name": "now", "block": "time.now", "format": "%Y-%m-%dT%H:%M:%SZ" }
                        ],
                        "output": { "now": "$now" }
                    }"#,
                )
                .unwrap();
            });
            insta::assert_snapshot!(main_rs);
        }
    }
}
