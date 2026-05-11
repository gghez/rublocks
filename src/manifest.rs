//! `main.json` parsing and validation.
//!
//! The manifest is the user-facing entry point of every rublocks project.
//! Schema and field semantics are documented in `docs/manifest.md`.

use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::layouts::Layout;
use crate::models::Model;
use crate::routes::Route;

/// Every manifest-level failure carries the offending file path so the dev
/// overlay can always point the user at the right place to edit.
///
/// `line`/`column` come from `serde_json::Error` on parse failures; they are
/// `None` for shape/validation errors that fire after the JSON parsed cleanly.
/// See issue #2 and `docs/dev-mode.md`.
#[derive(Debug, Clone)]
pub struct ManifestError {
    pub file: PathBuf,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub message: String,
}

impl ManifestError {
    pub fn validation(file: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            line: None,
            column: None,
            message: message.into(),
        }
    }

    pub fn parse(file: impl Into<PathBuf>, err: serde_json::Error) -> Self {
        Self {
            file: file.into(),
            line: Some(err.line()),
            column: Some(err.column()),
            message: err.to_string(),
        }
    }

    pub fn read(file: impl Into<PathBuf>, err: std::io::Error) -> Self {
        Self {
            file: file.into(),
            line: None,
            column: None,
            message: format!("failed to read: {err}"),
        }
    }
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.line, self.column) {
            (Some(l), Some(c)) => write!(f, "{}:{l}:{c}: {}", self.file.display(), self.message),
            (Some(l), None) => write!(f, "{}:{l}: {}", self.file.display(), self.message),
            _ => write!(f, "{}: {}", self.file.display(), self.message),
        }
    }
}

impl std::error::Error for ManifestError {}

/// Top-level shape of `main.json` plus everything discovered alongside it.
///
/// `name` and `services` come from `main.json`. `routes` and `models` are
/// populated by scanning their respective subdirectories, so codegen sees a
/// single fully resolved manifest. The multi-file plan (migrations, layouts,
/// jobs) is documented in `docs/manifest.md` and lands in subsequent slices.
#[derive(Debug)]
pub struct Manifest {
    /// Application name. Becomes the cargo crate name in the generated project.
    pub name: String,
    pub services: Services,
    /// Resolved database service. Folds `services.db` (preferred) and the
    /// legacy `services.postgres` shorthand into one struct so codegen does
    /// not have to track two shapes. `None` means "no database wired".
    pub database: Option<Database>,
    /// Optional HTTP middleware config (compression, CORS, timeouts, ...).
    /// Resolved from `main.json.http`; when missing, no middleware layers
    /// are wired into the generated Axum router.
    pub http: Option<HttpConfig>,
    pub routes: Vec<Route>,
    pub models: Vec<Model>,
    pub layouts: Vec<Layout>,
}

/// Optional HTTP middleware configuration. Maps onto `tower-http` layers in
/// the generated `main.rs`. Anything not set falls back to "layer not
/// installed"; the dist binary keeps the layer surface minimal so projects
/// that ship pure JSON APIs don't pay for HTML-only knobs.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct HttpConfig {
    /// Wrap the router in `tower_http::compression::CompressionLayer` so
    /// the server transparently gzip/brotli/zstd-encodes responses based on
    /// the client's `Accept-Encoding`.
    #[serde(default)]
    #[schemars(default)]
    pub compression: bool,
    /// CORS — when set, allow the listed origins (and the standard
    /// methods/headers) via `tower_http::cors::CorsLayer`.
    #[serde(default)]
    #[schemars(default)]
    pub cors: Option<CorsConfig>,
    /// Per-request timeout in milliseconds. Maps to
    /// `tower_http::timeout::TimeoutLayer`.
    #[serde(default)]
    #[schemars(default)]
    pub timeout_ms: Option<u64>,
    /// Inject opinionated security response headers
    /// (`X-Content-Type-Options`, `X-Frame-Options`,
    /// `Referrer-Policy`, `Strict-Transport-Security`).
    #[serde(default)]
    #[schemars(default)]
    pub security_headers: bool,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct CorsConfig {
    /// Allowed origins. `"*"` is accepted to allow any origin; mixing
    /// `"*"` with credentialed requests is the user's responsibility.
    pub origins: Vec<String>,
}

/// Database service after normalization. `kind` drives the sqlx feature
/// set, the Rust pool type, and the migration dialect.
#[derive(Debug, Clone)]
pub struct Database {
    pub kind: DbKind,
    pub url: ServiceUrl,
}

/// SQL backend selected by the manifest. Defaults to `Postgres` when the
/// legacy `services.postgres` shorthand is used. `Mariadb` shares the
/// `mysql` sqlx feature with MySQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum DbKind {
    #[default]
    Postgres,
    Mysql,
    Mariadb,
    Mssql,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(title = "rublocks main.json", deny_unknown_fields)]
struct RawManifest {
    /// Application name. Must be a valid cargo crate name
    /// (lowercase ASCII letters, digits, `_` or `-`).
    name: String,
    #[serde(default)]
    #[schemars(default)]
    services: Services,
    /// Optional HTTP middleware config — see [`HttpConfig`].
    #[serde(default)]
    #[schemars(default)]
    http: Option<HttpConfig>,
}

/// Optional service declarations. Each present service triggers conditional
/// dependency wiring in the generated `Cargo.toml` and `AppState`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct Services {
    /// Modern shorthand: `services.db` carries an explicit `kind`.
    pub db: Option<DatabaseService>,
    /// Legacy shorthand kept for backwards compatibility. Equivalent to
    /// `services.db` with `kind: postgres`. Setting both at once is a
    /// manifest error.
    pub postgres: Option<PostgresService>,
    pub redis: Option<RedisService>,
}

/// Generic database service declaration. `kind` defaults to `postgres` so
/// older manifests that only set the URL keep working.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DatabaseService {
    #[serde(default)]
    #[schemars(default)]
    pub kind: DbKind,
    /// Either a literal URL or `env:VAR_NAME` to read it from the
    /// environment at startup.
    #[schemars(with = "String")]
    pub url: ServiceUrl,
}

/// Postgres service configuration. Currently only the connection URL.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PostgresService {
    /// Either a literal URL (`postgres://...`) or `env:VAR_NAME` to read it from
    /// the environment at startup.
    #[schemars(with = "String")]
    pub url: ServiceUrl,
}

/// Redis service configuration. Currently only the connection URL.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RedisService {
    /// Either a literal URL (`redis://...`) or `env:VAR_NAME` to read it from
    /// the environment at startup.
    #[schemars(with = "String")]
    pub url: ServiceUrl,
}

/// A connection URL accepted by service declarations.
///
/// - `Literal("postgres://...")` is embedded directly in the generated source.
/// - `Env("DATABASE_URL")` becomes `std::env::var("DATABASE_URL")?` at startup.
///
/// The `env:` prefix is the recommended form for any secret-like value
/// (see `docs/manifest.md`). The schema-side representation is a plain string —
/// the prefix split happens during deserialization.
#[derive(Debug, Clone)]
pub enum ServiceUrl {
    Literal(String),
    Env(String),
}

impl<'de> Deserialize<'de> for ServiceUrl {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(match raw.strip_prefix("env:") {
            Some(var) => ServiceUrl::Env(var.to_string()),
            None => ServiceUrl::Literal(raw),
        })
    }
}

impl Manifest {
    /// Read `main.json` and discover sibling declarative files (routes, ...).
    ///
    /// Every error variant carries `file: PathBuf` so the dev overlay always
    /// knows which file the user must edit — see issue #2.
    pub fn load(project_dir: &Path) -> Result<Self, ManifestError> {
        let path = project_dir.join("main.json");
        let content = std::fs::read_to_string(&path).map_err(|e| ManifestError::read(&path, e))?;
        let raw: RawManifest =
            serde_json::from_str(&content).map_err(|e| ManifestError::parse(&path, e))?;
        validate_name(&raw.name, &path)?;
        let database = resolve_database(&raw.services, &path)?;
        let routes = Route::load_all(project_dir)?;
        let models = Model::load_all(project_dir)?;
        let layouts = Layout::load_all(project_dir)?;
        validate_route_layouts(&routes, &layouts)?;
        Ok(Manifest {
            name: raw.name,
            services: raw.services,
            database,
            http: raw.http,
            routes,
            models,
            layouts,
        })
    }
}

/// Fold `services.db` and the legacy `services.postgres` into one normalized
/// `Database` value. Returns `None` when neither is set; returns an error
/// when both are set (the user must pick one).
fn resolve_database(services: &Services, source: &Path) -> Result<Option<Database>, ManifestError> {
    match (&services.db, &services.postgres) {
        (Some(_), Some(_)) => Err(ManifestError::validation(
            source,
            "only one of `services.db` or `services.postgres` may be declared",
        )),
        (Some(db), None) => Ok(Some(Database {
            kind: db.kind,
            url: db.url.clone(),
        })),
        (None, Some(pg)) => Ok(Some(Database {
            kind: DbKind::Postgres,
            url: pg.url.clone(),
        })),
        (None, None) => Ok(None),
    }
}

/// JSON Schema describing the on-disk shape of `main.json`.
///
/// Derived from `RawManifest` so the schema is always in sync with what the
/// parser actually accepts. Consumed by the agent installers in `src/agents.rs`
/// — there is one schema per binary version, no per-project copy.
pub fn json_schema() -> RootSchema {
    schema_for!(RawManifest)
}

/// Parse a string against the manifest shape. Used by the doc examples test
/// to guarantee every `<!-- rb:manifest -->` block in `docs/*.md` still maps
/// onto the parser the binary actually runs.
#[cfg(test)]
pub(crate) fn validate_doc_example(s: &str) -> serde_json::Result<()> {
    serde_json::from_str::<RawManifest>(s).map(|_| ())
}

/// Catch unknown layout references at load time so codegen can assume every
/// `route.layout` resolves. The error points at the offending route file —
/// the user-actionable place to edit.
fn validate_route_layouts(routes: &[Route], layouts: &[Layout]) -> Result<(), ManifestError> {
    for r in routes {
        if let Some(layout_name) = &r.layout
            && !layouts.iter().any(|l| &l.name == layout_name)
        {
            return Err(ManifestError::validation(
                &r.source,
                format!(
                    "route declares layout `{layout_name}` but no such layout exists in layouts/"
                ),
            ));
        }
    }
    Ok(())
}

/// Enforce that `name` is a valid cargo crate name.
///
/// We catch this at manifest load instead of letting `cargo build` reject it
/// later — saves the user a pointless rebuild loop.
fn validate_name(name: &str, source: &Path) -> Result<(), ManifestError> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !ok {
        return Err(ManifestError::validation(
            source,
            format!(
                "invalid app name `{name}`: must be lowercase ascii letters, digits, `_` or `-`"
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_main(dir: &std::path::Path, body: &str) {
        fs::write(dir.join("main.json"), body).unwrap();
    }

    #[test]
    fn load_accepts_minimal_manifest() {
        let dir = TempDir::new().unwrap();
        write_main(dir.path(), r#"{ "name": "myapp" }"#);
        let m = Manifest::load(dir.path()).unwrap();
        assert_eq!(m.name, "myapp");
        assert!(m.database.is_none());
        assert!(m.routes.is_empty());
        assert!(m.models.is_empty());
    }

    #[test]
    fn load_rejects_uppercase_name() {
        let dir = TempDir::new().unwrap();
        write_main(dir.path(), r#"{ "name": "MyApp" }"#);
        let err = Manifest::load(dir.path()).unwrap_err();
        assert_eq!(err.file, dir.path().join("main.json"));
        assert!(
            err.message.contains("invalid app name"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn load_reports_syntax_error_with_line_and_column() {
        let dir = TempDir::new().unwrap();
        // Two-line malformed JSON: closing brace missing.
        write_main(dir.path(), "{\n  \"name\": \"x\"\n");
        let err = Manifest::load(dir.path()).unwrap_err();
        assert_eq!(err.file, dir.path().join("main.json"));
        assert!(err.line.is_some(), "syntax error must carry a line");
        assert!(err.column.is_some(), "syntax error must carry a column");
    }

    #[test]
    fn load_rejects_empty_name() {
        let dir = TempDir::new().unwrap();
        write_main(dir.path(), r#"{ "name": "" }"#);
        assert!(Manifest::load(dir.path()).is_err());
    }

    #[test]
    fn load_parses_env_service_url() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "services": { "postgres": { "url": "env:DATABASE_URL" } } }"#,
        );
        let m = Manifest::load(dir.path()).unwrap();
        let db = m.database.expect("postgres alias resolves to database");
        assert_eq!(db.kind, DbKind::Postgres);
        match db.url {
            ServiceUrl::Env(v) => assert_eq!(v, "DATABASE_URL"),
            other => panic!("expected env, got {other:?}"),
        }
    }

    #[test]
    fn load_parses_services_db_with_explicit_mysql_kind() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "services": { "db": { "kind": "mysql", "url": "env:MYSQL_URL" } } }"#,
        );
        let m = Manifest::load(dir.path()).unwrap();
        let db = m.database.unwrap();
        assert_eq!(db.kind, DbKind::Mysql);
    }

    #[test]
    fn load_rejects_both_db_and_postgres_set() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{
                "name": "a",
                "services": {
                    "db": { "kind": "mysql", "url": "env:X" },
                    "postgres": { "url": "env:Y" }
                }
            }"#,
        );
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(err.message.contains("only one of"), "got: {}", err.message);
    }

    #[test]
    fn load_parses_literal_service_url() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "services": { "postgres": { "url": "postgres://x" } } }"#,
        );
        let m = Manifest::load(dir.path()).unwrap();
        match m.database.unwrap().url {
            ServiceUrl::Literal(s) => assert_eq!(s, "postgres://x"),
            other => panic!("expected literal, got {other:?}"),
        }
    }

    #[test]
    fn load_aggregates_routes_and_models() {
        let dir = TempDir::new().unwrap();
        write_main(dir.path(), r#"{ "name": "a" }"#);
        fs::create_dir_all(dir.path().join("routes")).unwrap();
        fs::write(
            dir.path().join("routes").join("home.json"),
            r#"{ "path": "/", "method": "GET", "kind": "page", "template": "home.html" }"#,
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("models")).unwrap();
        fs::write(
            dir.path().join("models").join("post.json"),
            r#"{ "name": "Post", "table": "posts", "fields": { "id": { "type": "uuid" } } }"#,
        )
        .unwrap();
        let m = Manifest::load(dir.path()).unwrap();
        assert_eq!(m.routes.len(), 1);
        assert_eq!(m.models.len(), 1);
        assert_eq!(m.routes[0].path, "/");
        assert_eq!(m.models[0].name, "Post");
    }
}
