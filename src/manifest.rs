//! `main.json` parsing and validation.
//!
//! The manifest is the user-facing entry point of every rublocks project.
//! Schema and field semantics are documented in `docs/manifest.md`.

use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::language;
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
    /// Project version. Mandatory SemVer 2.0.0 string. Single source of truth
    /// for every generated artifact that needs to identify the build:
    /// `Cargo.toml` `package.version`, OpenAPI `info.version`, the
    /// `X-App-Version` response header, and the dev-mode error page footer.
    /// See issue #15 — no fallback default, the project author must state it.
    pub version: String,
    /// One-line human-readable synopsis of the project. Threaded as the
    /// single source of truth to every artifact that needs to describe the
    /// project — `Cargo.toml` `package.description`, the dev-mode landing
    /// `<meta name="description">` and subtitle, the dev-mode error
    /// overlay subtitle, and (once it ships) the OpenAPI `info.description`.
    pub description: String,
    /// BCP 47 language tag (e.g. `"en-US"`, `"fr-FR"`, `"pt-BR"`). Mandatory:
    /// every project must declare its primary locale so HTML, dev-mode error
    /// strings, and future i18n hang off a single, explicit source of truth.
    /// See issue #14 and `docs/manifest.md`.
    pub language: String,
    /// Project-wide character encoding. Always `Encoding::Utf8` today — the
    /// field is mandatory in `main.json` so every project commits to a
    /// declared encoding, and a future value (e.g. another normalization form)
    /// can land without a silent default flip. See `docs/encoding.md`.
    pub encoding: Encoding,
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
    /// Project version, mandatory. SemVer 2.0.0 — e.g. `"0.1.0"`,
    /// `"1.4.2-rc.1"`. Threaded into every generated artifact that
    /// identifies the build. See `docs/manifest.md` and issue #15.
    version: String,
    /// One-line human-readable synopsis of what the project does.
    /// Mandatory; non-empty after trimming; max 280 characters; no newlines.
    description: String,
    /// Required BCP 47 language tag — see [`Manifest::language`].
    language: String,
    /// Project-wide character encoding. Required. Only `"utf-8"` is
    /// accepted today; any other value is rejected at load time. See
    /// `docs/encoding.md` for the rationale (UTF-8 everywhere, strict
    /// on input, explicit on output).
    encoding: String,
    #[serde(default)]
    #[schemars(default)]
    services: Services,
    /// Optional HTTP middleware config — see [`HttpConfig`].
    #[serde(default)]
    #[schemars(default)]
    http: Option<HttpConfig>,
}

/// Project-wide character encoding declared in `main.json`.
///
/// Always `Utf8` today — keeping it an enum (rather than a unit type)
/// preserves the seam for a future second value without breaking the
/// rest of the codebase. See `docs/encoding.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
}

impl Encoding {
    /// IANA charset label emitted in generated `Content-Type` headers and
    /// in `client_encoding=` connection parameters.
    pub fn charset_label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "utf-8",
        }
    }
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
        validate_version(&raw.version, &path)?;
        let description = validate_description(&raw.description, &path)?;
        validate_language(&raw.language, &path)?;
        let encoding = parse_encoding(&raw.encoding, &path)?;
        let database = resolve_database(&raw.services, &path)?;
        let routes = Route::load_all(project_dir)?;
        let models = Model::load_all(project_dir)?;
        let layouts = Layout::load_all(project_dir)?;
        validate_route_layouts(&routes, &layouts)?;
        crate::expressions::scope_check_routes(&routes, &models)?;
        Ok(Manifest {
            name: raw.name,
            version: raw.version,
            description,
            language: raw.language,
            encoding,
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

/// Enforce that `version` is a valid SemVer 2.0.0 string.
///
/// Caught at manifest load so a malformed version surfaces in the dev
/// overlay (file + reason) rather than as a cryptic cargo error later in
/// the build pipeline.
fn validate_version(version: &str, source: &Path) -> Result<(), ManifestError> {
    if let Err(e) = semver::Version::parse(version) {
        return Err(ManifestError::validation(
            source,
            format!("invalid version `{version}`: must be SemVer 2.0.0 ({e})"),
        ));
    }
    Ok(())
}

/// Enforce that `language` is a well-formed BCP 47 tag.
///
/// We reject the empty string and malformed tags at load time so the dev
/// overlay can point at `main.json` directly — `<html lang>` and the
/// `Content-Language` header would otherwise fail silently or produce
/// invalid HTTP downstream.
fn validate_language(language: &str, source: &Path) -> Result<(), ManifestError> {
    if !language::is_well_formed(language) {
        return Err(ManifestError::validation(
            source,
            format!(
                "invalid `language` value `{language}`: must be a BCP 47 tag like \"en-US\" or \"fr-FR\""
            ),
        ));
    }
    Ok(())
}

/// Enforce the formatting rules for the manifest `description` field.
///
/// The description is the single source of truth for every "what is this
/// project?" artifact (`Cargo.toml`, dev-mode overlay, future OpenAPI).
/// The rules force a real one-liner so downstream consumers can embed it
/// verbatim without truncation or re-flow:
///
/// - non-empty after trimming
/// - max 280 characters
/// - no newlines (synopsis, not prose)
///
/// Returns the trimmed value so callers don't carry around incidental
/// surrounding whitespace.
fn validate_description(raw: &str, source: &Path) -> Result<String, ManifestError> {
    if raw.contains('\n') || raw.contains('\r') {
        return Err(ManifestError::validation(
            source,
            "invalid `description`: must be a single line (no newlines)",
        ));
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ManifestError::validation(
            source,
            "invalid `description`: must not be empty",
        ));
    }
    if trimmed.chars().count() > 280 {
        return Err(ManifestError::validation(
            source,
            "invalid `description`: must be at most 280 characters",
        ));
    }
    Ok(trimmed.to_string())
}

/// Parse the declared `encoding` field. Required at the manifest level; only
/// `"utf-8"` is accepted today. The enum lives in [`Encoding`] so codegen
/// reads a typed value rather than a free-form string. See `docs/encoding.md`
/// for the policy and `docs/decisions.md` for the rationale.
fn parse_encoding(value: &str, source: &Path) -> Result<Encoding, ManifestError> {
    // Case-insensitive match: the IANA charset label is case-insensitive
    // (`UTF-8` and `utf-8` denote the same encoding). The dash variants
    // (`utf8` without the dash, common in MySQL configs) are intentionally
    // not accepted — we want one canonical spelling in `main.json`.
    if value.eq_ignore_ascii_case("utf-8") {
        return Ok(Encoding::Utf8);
    }
    Err(ManifestError::validation(
        source,
        format!("unsupported `encoding`: `{value}` — only `utf-8` is supported"),
    ))
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
        write_main(
            dir.path(),
            r#"{ "name": "myapp", "version": "0.1.0", "description": "demo app", "language": "en-US", "encoding": "utf-8" }"#,
        );
        let m = Manifest::load(dir.path()).unwrap();
        assert_eq!(m.name, "myapp");
        assert_eq!(m.version, "0.1.0");
        assert_eq!(m.description, "demo app");
        assert_eq!(m.language, "en-US");
        assert_eq!(m.encoding, Encoding::Utf8);
        assert!(m.database.is_none());
        assert!(m.routes.is_empty());
        assert!(m.models.is_empty());
    }

    #[test]
    fn load_rejects_missing_encoding() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "myapp", "version": "0.1.0", "description": "x", "language": "en-US" }"#,
        );
        let err = Manifest::load(dir.path()).unwrap_err();
        assert_eq!(err.file, dir.path().join("main.json"));
        assert!(
            err.message.contains("encoding"),
            "missing field error must mention `encoding`, got: {}",
            err.message
        );
    }

    #[test]
    fn load_rejects_unsupported_encoding() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "myapp", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-16" }"#,
        );
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(
            err.message.contains("utf-16"),
            "error must name the rejected value, got: {}",
            err.message
        );
        assert!(
            err.message.contains("only `utf-8`"),
            "error must point users at the accepted value, got: {}",
            err.message
        );
    }

    #[test]
    fn load_accepts_case_insensitive_utf8_spelling() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "myapp", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "UTF-8" }"#,
        );
        let m = Manifest::load(dir.path()).unwrap();
        assert_eq!(m.encoding, Encoding::Utf8);
    }

    #[test]
    fn load_rejects_uppercase_name() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "MyApp", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8" }"#,
        );
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
        write_main(
            dir.path(),
            r#"{ "name": "", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8" }"#,
        );
        assert!(Manifest::load(dir.path()).is_err());
    }

    #[test]
    fn load_rejects_missing_version() {
        // `version` is mandatory (issue #15) — no fallback default. The error
        // must surface as a manifest parse error so the dev overlay points at
        // `main.json`.
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "description": "x", "language": "en-US" }"#,
        );
        let err = Manifest::load(dir.path()).unwrap_err();
        assert_eq!(err.file, dir.path().join("main.json"));
        assert!(
            err.message.contains("version"),
            "missing-field error must name `version`: {}",
            err.message
        );
    }

    #[test]
    fn load_rejects_missing_description() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "0.1.0", "language": "en-US" }"#,
        );
        let err = Manifest::load(dir.path()).unwrap_err();
        assert_eq!(err.file, dir.path().join("main.json"));
        assert!(
            err.message.contains("description"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn load_rejects_missing_language() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "myapp", "version": "0.1.0", "description": "x" }"#,
        );
        let err = Manifest::load(dir.path()).unwrap_err();
        assert_eq!(err.file, dir.path().join("main.json"));
        assert!(
            err.message.contains("language"),
            "missing-field error should mention `language`, got: {}",
            err.message
        );
    }

    #[test]
    fn load_rejects_invalid_semver() {
        // SemVer 2.0.0 is enforced — a freeform string like "v1" or "1.0"
        // is rejected with a message that names the offending value.
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "1.0", "description": "x", "language": "en-US", "encoding": "utf-8" }"#,
        );
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(
            err.message.contains("invalid version") && err.message.contains("1.0"),
            "must name the offending version: {}",
            err.message
        );
    }

    #[test]
    fn load_accepts_pre_release_and_build_metadata() {
        // SemVer 2.0.0 admits `-rc.1`, `+gabc1234`, etc. The validator must
        // accept the full grammar (issue #15 scope example).
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "1.4.2-rc.1+build.7", "description": "x", "language": "en-US", "encoding": "utf-8" }"#,
        );
        let m = Manifest::load(dir.path()).unwrap();
        assert_eq!(m.version, "1.4.2-rc.1+build.7");
    }

    #[test]
    fn load_rejects_empty_description() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "0.1.0", "description": "   ", "language": "en-US", "encoding": "utf-8" }"#,
        );
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(err.message.contains("must not be empty"), "got: {}", err.message);
    }

    #[test]
    fn load_rejects_description_with_newline() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "0.1.0", "description": "first\nsecond", "language": "en-US", "encoding": "utf-8" }"#,
        );
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(err.message.contains("single line"), "got: {}", err.message);
    }

    #[test]
    fn load_rejects_description_over_280_chars() {
        let dir = TempDir::new().unwrap();
        let long = "a".repeat(281);
        write_main(
            dir.path(),
            &format!(r#"{{ "name": "a", "version": "0.1.0", "description": "{long}", "language": "en-US", "encoding": "utf-8" }}"#),
        );
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(err.message.contains("at most 280"), "got: {}", err.message);
    }

    #[test]
    fn load_trims_description() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "0.1.0", "description": "   hello   ", "language": "en-US", "encoding": "utf-8" }"#,
        );
        let m = Manifest::load(dir.path()).unwrap();
        assert_eq!(m.description, "hello");
    }

    #[test]
    fn load_rejects_invalid_language_tag() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "myapp", "version": "0.1.0", "description": "x", "language": "francais", "encoding": "utf-8" }"#,
        );
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(
            err.message.contains("invalid `language` value `francais`"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("BCP 47"), "got: {}", err.message);
    }

    #[test]
    fn load_parses_env_service_url() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8", "services": { "postgres": { "url": "env:DATABASE_URL" } } }"#,
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
            r#"{ "name": "a", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8", "services": { "db": { "kind": "mysql", "url": "env:MYSQL_URL" } } }"#,
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
                "version": "0.1.0",
                "description": "x",
                "language": "en-US",
                "encoding": "utf-8",
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
            r#"{ "name": "a", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8", "services": { "postgres": { "url": "postgres://x" } } }"#,
        );
        let m = Manifest::load(dir.path()).unwrap();
        match m.database.unwrap().url {
            ServiceUrl::Literal(s) => assert_eq!(s, "postgres://x"),
            other => panic!("expected literal, got {other:?}"),
        }
    }

    #[test]
    fn load_rejects_guard_referencing_unknown_identifier() {
        // The `guard` block at process[0] only sees the route input. A
        // reference to `user` (no auth wired) must fail at build with the
        // offender named.
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8" }"#,
        );
        fs::create_dir_all(dir.path().join("routes")).unwrap();
        fs::write(
            dir.path().join("routes").join("admin.json"),
            r#"{
                "path": "/admin",
                "method": "GET",
                "kind": "page",
                "template": "admin.html",
                "process": [
                    { "block": "guard", "if": "user.is_admin" }
                ]
            }"#,
        )
        .unwrap();
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(
            err.message.contains("unknown identifier"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.contains("user"),
            "must name the offender: {}",
            err.message
        );
    }

    #[test]
    fn load_accepts_guard_referencing_input_field() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8" }"#,
        );
        fs::create_dir_all(dir.path().join("routes")).unwrap();
        fs::write(
            dir.path().join("routes").join("admin.json"),
            r#"{
                "path": "/admin",
                "method": "GET",
                "kind": "page",
                "template": "admin.html",
                "input": { "query": { "token": { "type": "string", "required": true } } },
                "process": [
                    { "block": "guard", "if": "token == \"open-sesame\"" }
                ]
            }"#,
        )
        .unwrap();
        let m = Manifest::load(dir.path()).unwrap();
        assert_eq!(m.routes[0].process[0].kind_id(), "guard");
    }

    #[test]
    fn load_accepts_guard_referencing_prior_block_binding() {
        // After a `db.find_one` binds `$post`, a later guard can assert
        // ownership against the loaded row.
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8" }"#,
        );
        fs::create_dir_all(dir.path().join("routes")).unwrap();
        fs::create_dir_all(dir.path().join("models")).unwrap();
        fs::write(
            dir.path().join("models").join("post.json"),
            r#"{ "name": "Post", "table": "posts", "fields": { "id": { "type": "uuid" }, "author_id": { "type": "uuid" } } }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("routes").join("edit.json"),
            r#"{
                "path": "/posts/:slug/edit",
                "method": "GET",
                "kind": "page",
                "template": "posts/edit.html",
                "input": { "path": { "slug": { "type": "string", "required": true } } },
                "process": [
                    { "name": "post", "block": "db.find_one", "table": "posts" },
                    { "block": "guard", "if": "post.author_id == slug" }
                ]
            }"#,
        )
        .unwrap();
        let m = Manifest::load(dir.path()).unwrap();
        assert_eq!(m.routes[0].process.len(), 2);
    }

    #[test]
    fn load_rejects_where_string_referencing_unknown_column() {
        // The `db.find_*` string-form `where` is scope-checked against the
        // target table's columns. A typo on the column name fires here.
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8" }"#,
        );
        fs::create_dir_all(dir.path().join("routes")).unwrap();
        fs::create_dir_all(dir.path().join("models")).unwrap();
        fs::write(
            dir.path().join("models").join("post.json"),
            r#"{ "name": "Post", "table": "posts", "fields": { "id": { "type": "uuid" }, "title": { "type": "string" } } }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("routes").join("posts.json"),
            r#"{
                "path": "/posts",
                "method": "GET",
                "kind": "api",
                "process": [
                    { "name": "posts", "block": "db.find_many", "table": "posts", "where": "tilte == \"x\"" }
                ]
            }"#,
        )
        .unwrap();
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(
            err.message.contains("unknown identifier") && err.message.contains("tilte"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn load_rejects_input_field_collision_across_sections() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8" }"#,
        );
        fs::create_dir_all(dir.path().join("routes")).unwrap();
        fs::write(
            dir.path().join("routes").join("dup.json"),
            r#"{
                "path": "/x/:slug",
                "method": "POST",
                "kind": "page",
                "template": "x.html",
                "input": {
                    "path": { "slug": { "type": "string", "required": true } },
                    "body": { "slug": { "type": "string", "required": true } }
                },
                "process": [
                    { "block": "guard", "if": "slug != \"\"" }
                ]
            }"#,
        )
        .unwrap();
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(err.message.contains("collides"), "got: {}", err.message);
    }

    #[test]
    fn load_rejects_where_string_using_unsupported_operator() {
        // The build-time SQL translator covers ==/!=/</<=/>/>=/&&/||/in.
        // Anything else (arithmetic, function calls, field selection)
        // fails the build so the user does not learn the limitation only
        // at execution time.
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8" }"#,
        );
        fs::create_dir_all(dir.path().join("routes")).unwrap();
        fs::create_dir_all(dir.path().join("models")).unwrap();
        fs::write(
            dir.path().join("models").join("post.json"),
            r#"{ "name": "Post", "table": "posts", "fields": { "id": { "type": "uuid" }, "score": { "type": "int" } } }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("routes").join("posts.json"),
            r#"{
                "path": "/posts",
                "method": "GET",
                "kind": "api",
                "process": [
                    { "name": "posts", "block": "db.find_many", "table": "posts", "where": "score + 1 == 2" }
                ]
            }"#,
        )
        .unwrap();
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(
            err.message.contains("not supported"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn load_aggregates_routes_and_models() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8" }"#,
        );
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
