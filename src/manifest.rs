//! `main.json` parsing and validation.
//!
//! The manifest is the user-facing entry point of every rublocks project.
//! Schema and field semantics are documented in `docs/manifest.md`.

use indexmap::IndexMap;
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::language;
use crate::layouts::Layout;
use crate::models::Model;
use crate::routes::Route;
use crate::sftp::{SftpAuthMethod, SftpService};

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

/// Read a project file as UTF-8 text and enforce the
/// "UTF-8 everywhere, strict on input" half of the encoding contract.
///
/// Rejects UTF-16 / UTF-32 byte order marks at build time with a clear
/// error so the user knows which file to re-save. Tolerates (and strips) an
/// optional UTF-8 BOM: some editors on Windows write it by default and we
/// don't want that to look like a corrupt JSON file at the parse step.
///
/// This is the single entry point every manifest-adjacent reader goes
/// through (`main.json`, `routes/*.json`, `models/*.json`, `layouts/*.json`,
/// `migrations/.state.json`). The `encoding` declaration in `main.json`
/// applies to all of them — see `docs/encoding.md`.
pub fn read_text_utf8(path: &Path) -> Result<String, ManifestError> {
    let bytes = std::fs::read(path).map_err(|e| ManifestError::read(path, e))?;
    decode_utf8(&bytes, path)
}

/// Pure decode step, factored out so tests can exercise it without
/// touching the filesystem and so [`read_text_utf8`] stays a one-liner.
///
/// Order matters: UTF-32 BOMs share their first two bytes with UTF-16
/// (`FF FE` is the prefix of both UTF-16 LE and UTF-32 LE), so the four-byte
/// signatures must be checked first.
fn decode_utf8(bytes: &[u8], source: &Path) -> Result<String, ManifestError> {
    if bytes.starts_with(&[0x00, 0x00, 0xFE, 0xFF]) || bytes.starts_with(&[0xFF, 0xFE, 0x00, 0x00])
    {
        return Err(ManifestError::validation(
            source,
            "file is encoded as UTF-32 — re-save as UTF-8 (main.json declares `encoding: utf-8`)",
        ));
    }
    if bytes.starts_with(&[0xFE, 0xFF]) || bytes.starts_with(&[0xFF, 0xFE]) {
        return Err(ManifestError::validation(
            source,
            "file is encoded as UTF-16 — re-save as UTF-8 (main.json declares `encoding: utf-8`)",
        ));
    }
    let payload: &[u8] = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    std::str::from_utf8(payload)
        .map(str::to_owned)
        .map_err(|e| {
            ManifestError::validation(
                source,
                format!(
                    "file is not valid UTF-8 (byte offset {}): {e}",
                    e.valid_up_to()
                ),
            )
        })
}

/// Write a text file under the project's encoding contract: UTF-8 bytes, no
/// BOM, LF line endings — regardless of the host OS. This is the second half
/// of the "UTF-8 everywhere, strict on input, explicit on output" policy
/// stated in `docs/encoding.md`.
///
/// CRLF / CR sequences in the input are folded to LF so a Windows-style
/// snippet doesn't smuggle `\r\n` into a generated artifact. A leading UTF-8
/// BOM, if a caller happened to include one, is also stripped — generated
/// files must not advertise their encoding through a BOM (the explicit
/// `Content-Type: ...; charset=utf-8` headers are the canonical channel).
pub fn write_text_utf8(path: &Path, content: &str) -> std::io::Result<()> {
    let normalized = normalize_for_write(content);
    std::fs::write(path, normalized.as_bytes())
}

/// Pure transform extracted so codegen tests can verify the round-trip
/// without touching the filesystem.
pub(crate) fn normalize_for_write(content: &str) -> String {
    let stripped = content.strip_prefix('\u{FEFF}').unwrap_or(content);
    if stripped.contains('\r') {
        // Two-pass: first turn CRLF into LF, then any remaining lone CR
        // (old-Mac style) into LF. Order matters — doing CR first would
        // double-LF every CRLF.
        stripped.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        stripped.to_string()
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
/// `name` and `services` come from `main.json`. `routes`, `models`,
/// `layouts`, and `migrations` are populated by scanning their respective
/// subdirectories, so codegen sees a single fully resolved manifest.
/// See `docs/manifest.md` for the multi-file plan.
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
    /// Resolved SFTP services in declaration order. Empty when no
    /// `services.<name>` of `kind: "sftp"` is declared. Codegen wires one
    /// `Arc<SftpService>` field on `AppState` per entry.
    pub sftp_services: Vec<ResolvedSftpService>,
    /// Optional HTTP middleware config (compression, CORS, timeouts, ...).
    /// Resolved from `main.json.http`; when missing, no middleware layers
    /// are wired into the generated Axum router.
    pub http: Option<HttpConfig>,
    /// Structured logging configuration. Mandatory in `main.json` — a project
    /// without an explicit `logging` block is a build error (see issue #17).
    /// Drives the generated `tracing-subscriber` init and the per-block log
    /// emission contract.
    pub logging: Logging,
    /// Resolved `.env` loading policy — see [`LoadDotenv`]. Drives both the
    /// generated binary (a `dotenvy::from_path(...).ok()` call emitted before
    /// any `env:VAR` reference is resolved) and the dev supervisor (which
    /// merges the same file into the child's env before spawning).
    pub load_dotenv: LoadDotenv,
    pub routes: Vec<Route>,
    pub models: Vec<Model>,
    pub layouts: Vec<Layout>,
}

/// Resolved `.env` loading policy. Built from `main.json.load_dotenv` after
/// the `false | string` surface is split and any user-supplied path is
/// resolved against `main.json`'s directory.
///
/// - `Auto` — load a `.env` sitting next to `main.json` if present. This is
///   the implicit default (field omitted from the manifest); the "one
///   feature = one declarative form" rule forbids a redundant explicit
///   spelling of the default.
/// - `Path(p)` — load the file at the explicit absolute path `p`.
/// - `Disabled` — never load a dotenv file; the user manages env vars
///   exclusively through the shell or orchestrator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadDotenv {
    Auto,
    Path(PathBuf),
    Disabled,
}

/// Resolved logging configuration. Built from `main.json.logging` after the
/// `level` string has been parsed into a typed [`LogLevel`] and each `include`
/// value's `env:VAR` prefix has been split off into [`LogIncludeValue::Env`].
#[derive(Debug, Clone)]
pub struct Logging {
    pub level: LogLevel,
    /// Key → value pairs injected on every structured log event. Stable order
    /// from the source JSON so the generated `info!`/`error!` field list is
    /// deterministic.
    pub include: IndexMap<String, LogIncludeValue>,
}

/// Subscriber max-level driven by `logging.level`. The five canonical
/// `tracing` levels — anything else is rejected at manifest load with an
/// explicit error so the dev overlay can point at `main.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// One entry of `logging.include`. Either a literal string baked into every
/// log line (`{"service":"myblog"}`) or an `env:VAR` reference resolved at
/// startup so secrets / per-environment values stay out of the manifest.
#[derive(Debug, Clone)]
pub enum LogIncludeValue {
    Literal(String),
    Env(String),
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
    /// Structured logging configuration — see [`Logging`]. Mandatory in the
    /// manifest (issue #17): a missing block fails the load step so the dev
    /// overlay points the user at `main.json`.
    logging: LoggingRaw,
    /// Optional `.env` loading policy — see [`LoadDotenv`] for the resolved
    /// shape. Omitting the field means [`LoadDotenv::Auto`]; the only
    /// accepted explicit values are `false` (→ [`LoadDotenv::Disabled`]) and
    /// a string path (→ [`LoadDotenv::Path`]). Spelling out `true` is
    /// rejected at load time — the "one feature = one declarative form"
    /// rule forbids two ways to express the default.
    #[serde(default)]
    #[schemars(default)]
    load_dotenv: Option<RawLoadDotenv>,
}

/// On-disk shape of `main.json.load_dotenv`. The serde-side union mirrors
/// the user-facing `false | "<path>"` surface; resolution into
/// [`LoadDotenv`] happens in [`resolve_load_dotenv`] so the public type
/// only ever carries already-validated values.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
enum RawLoadDotenv {
    Bool(bool),
    Path(String),
}

/// On-disk shape of `main.json.logging`. Kept separate from [`Logging`] so the
/// public type carries already-validated values and the schema/derive
/// machinery stays scoped to this module.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "rublocks logging")]
struct LoggingRaw {
    /// Subscriber level: `trace` / `debug` / `info` / `warn` / `error`. No
    /// default — the project author must commit to a level explicitly.
    level: String,
    /// Optional key/value pairs injected on every log event. Values support
    /// the `env:VAR_NAME` prefix already used elsewhere in the manifest.
    #[serde(default)]
    #[schemars(default)]
    include: IndexMap<String, String>,
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
///
/// The typed slots (`db`, `postgres`, `redis`) keep the existing database +
/// cache wiring shape; any other key under `services.*` is captured by
/// [`Services::named`] as a generic kind-discriminated service (only
/// `kind: "sftp"` recognised today). The flatten pattern preserves
/// backwards compatibility while opening user-chosen service names.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct Services {
    /// Modern shorthand: `services.db` carries an explicit `kind`.
    pub db: Option<DatabaseService>,
    /// Legacy shorthand kept for backwards compatibility. Equivalent to
    /// `services.db` with `kind: postgres`. Setting both at once is a
    /// manifest error.
    pub postgres: Option<PostgresService>,
    pub redis: Option<RedisService>,
    /// Generic kind-discriminated services keyed by the user-chosen name.
    /// Flattened into the parent object so a manifest can mix
    /// `services.db` / `services.redis` and `services.<my-name>` freely.
    #[serde(flatten)]
    pub named: IndexMap<String, NamedService>,
}

/// One generic `services.<name>` entry. Discriminated by `kind`. Reserved
/// keys (`db`, `postgres`, `redis`) are captured by [`Services`]' typed
/// slots before flatten ever sees them, so this enum only carries the
/// kinds opened up to arbitrary naming.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum NamedService {
    /// SFTP target — see [`crate::sftp::SftpService`].
    Sftp(SftpService),
}

/// Resolved SFTP service ready for codegen. Pairs the user-chosen name
/// (`services.<name>`) with the parsed body and the auth method picked at
/// validation. Cheap to clone — the body is a few owned strings.
#[derive(Debug, Clone)]
pub struct ResolvedSftpService {
    pub name: String,
    pub config: SftpService,
    pub auth_method: SftpAuthMethod,
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
        let content = read_text_utf8(&path)?;
        let raw: RawManifest =
            serde_json::from_str(&content).map_err(|e| ManifestError::parse(&path, e))?;
        validate_name(&raw.name, &path)?;
        validate_version(&raw.version, &path)?;
        let description = validate_description(&raw.description, &path)?;
        validate_language(&raw.language, &path)?;
        let encoding = parse_encoding(&raw.encoding, &path)?;
        let logging = resolve_logging(&raw.logging, &path)?;
        let load_dotenv = resolve_load_dotenv(raw.load_dotenv.as_ref(), project_dir, &path)?;
        let database = resolve_database(&raw.services, &path)?;
        let sftp_services = resolve_sftp_services(&raw.services, &path)?;
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
            sftp_services,
            http: raw.http,
            logging,
            load_dotenv,
            routes,
            models,
            layouts,
        })
    }
}

/// Walk `services.named` and collect every `kind: "sftp"` entry as a
/// [`ResolvedSftpService`]. Auth validation runs here so a malformed manifest
/// fails at load time with a message pinned to `services.<name>.auth`.
fn resolve_sftp_services(
    services: &Services,
    source: &Path,
) -> Result<Vec<ResolvedSftpService>, ManifestError> {
    let mut out = Vec::new();
    for (name, decl) in &services.named {
        match decl {
            NamedService::Sftp(svc) => {
                let auth_method = svc.auth.validate(source, &format!("services.{name}"))?;
                out.push(ResolvedSftpService {
                    name: name.clone(),
                    config: svc.clone(),
                    auth_method,
                });
            }
        }
    }
    Ok(out)
}

/// Validate and lift `main.json.logging` into the typed [`Logging`] value.
///
/// The level is parsed off the canonical lowercase tracing names; anything
/// else fires a load-time error so the user picks one of the five accepted
/// values. Each `include` entry's `env:VAR` prefix is split off the same way
/// `ServiceUrl` handles connection strings, so the runtime can `std::env::var`
/// the value at startup.
fn resolve_logging(raw: &LoggingRaw, source: &Path) -> Result<Logging, ManifestError> {
    let level = match raw.level.as_str() {
        "trace" => LogLevel::Trace,
        "debug" => LogLevel::Debug,
        "info" => LogLevel::Info,
        "warn" => LogLevel::Warn,
        "error" => LogLevel::Error,
        other => {
            return Err(ManifestError::validation(
                source,
                format!(
                    "invalid `logging.level` value `{other}`: must be one of trace/debug/info/warn/error"
                ),
            ));
        }
    };
    let mut include: IndexMap<String, LogIncludeValue> = IndexMap::with_capacity(raw.include.len());
    for (k, v) in &raw.include {
        let value = match v.strip_prefix("env:") {
            Some(var) => LogIncludeValue::Env(var.to_string()),
            None => LogIncludeValue::Literal(v.clone()),
        };
        include.insert(k.clone(), value);
    }
    Ok(Logging { level, include })
}

/// Lift the optional `load_dotenv` manifest field into the resolved
/// [`LoadDotenv`] policy.
///
/// The omitted-field path is canonical for the default (`Auto`); spelling
/// out `true` is rejected as redundant per the "one feature = one
/// declarative form" rule. A string path is trimmed, resolved against
/// `project_dir` (so the file location is locked at parse time, never at
/// runtime), and rejected if it points at an existing directory — the
/// generated `dotenvy::from_path` call would otherwise produce a confusing
/// runtime error far from the manifest typo.
fn resolve_load_dotenv(
    raw: Option<&RawLoadDotenv>,
    project_dir: &Path,
    source: &Path,
) -> Result<LoadDotenv, ManifestError> {
    match raw {
        None => Ok(LoadDotenv::Auto),
        Some(RawLoadDotenv::Bool(false)) => Ok(LoadDotenv::Disabled),
        Some(RawLoadDotenv::Bool(true)) => Err(ManifestError::validation(
            source,
            "invalid `load_dotenv`: omit the field to load `.env` next to `main.json` — `true` is not a writable spelling (one feature = one form)",
        )),
        Some(RawLoadDotenv::Path(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Err(ManifestError::validation(
                    source,
                    "invalid `load_dotenv`: string value must not be empty or whitespace",
                ));
            }
            let candidate = PathBuf::from(trimmed);
            let absolute = if candidate.is_absolute() {
                candidate
            } else {
                project_dir.join(candidate)
            };
            if absolute.is_dir() {
                return Err(ManifestError::validation(
                    source,
                    format!(
                        "invalid `load_dotenv`: resolved path `{}` is a directory, expected a file",
                        absolute.display()
                    ),
                ));
            }
            Ok(LoadDotenv::Path(absolute))
        }
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
        // Backfill `logging` for the bulk of the tests that focus on other
        // fields — the dedicated logging tests below explicitly include the
        // block (and call `fs::write` directly when they want to test a
        // missing one).
        let body = if body.trim_start().starts_with('{') && !body.contains("\"logging\"") {
            let trimmed = body.trim_start();
            let mut out = String::from("{ \"logging\": { \"level\": \"info\" }, ");
            out.push_str(&trimmed[1..]);
            out
        } else {
            body.to_string()
        };
        fs::write(dir.join("main.json"), body).unwrap();
    }

    fn write_main_bytes(dir: &std::path::Path, bytes: &[u8]) {
        fs::write(dir.join("main.json"), bytes).unwrap();
    }

    #[test]
    fn load_strips_utf8_bom() {
        let dir = TempDir::new().unwrap();
        // EF BB BF + the usual JSON body. Some Windows editors prepend the
        // UTF-8 BOM by default; we tolerate it instead of failing the parse.
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(
            br#"{ "name": "myapp", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8", "logging": { "level": "info" } }"#,
        );
        write_main_bytes(dir.path(), &bytes);
        let m = Manifest::load(dir.path()).unwrap();
        assert_eq!(m.name, "myapp");
    }

    #[test]
    fn load_rejects_utf16_le_bom() {
        let dir = TempDir::new().unwrap();
        write_main_bytes(dir.path(), &[0xFF, 0xFE, b'{', 0x00]);
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(err.message.contains("UTF-16"), "got: {}", err.message);
    }

    #[test]
    fn load_rejects_utf16_be_bom() {
        let dir = TempDir::new().unwrap();
        write_main_bytes(dir.path(), &[0xFE, 0xFF, 0x00, b'{']);
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(err.message.contains("UTF-16"), "got: {}", err.message);
    }

    #[test]
    fn load_rejects_utf32_le_bom() {
        let dir = TempDir::new().unwrap();
        // UTF-32 LE BOM `FF FE 00 00` must be detected before UTF-16 LE
        // (`FF FE`) — the four-byte check has to run first.
        write_main_bytes(dir.path(), &[0xFF, 0xFE, 0x00, 0x00, b'{']);
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(err.message.contains("UTF-32"), "got: {}", err.message);
    }

    #[test]
    fn load_rejects_utf32_be_bom() {
        let dir = TempDir::new().unwrap();
        write_main_bytes(dir.path(), &[0x00, 0x00, 0xFE, 0xFF, b'{']);
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(err.message.contains("UTF-32"), "got: {}", err.message);
    }

    #[test]
    fn write_text_utf8_normalises_crlf_to_lf_and_strips_bom() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("hello.txt");
        // Caller hands over CRLF, lone CR, and a stray UTF-8 BOM: every one
        // of those must be erased so the on-disk bytes are the canonical
        // form rublocks promises (`docs/encoding.md`).
        write_text_utf8(&path, "\u{FEFF}line1\r\nline2\rline3\n").unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes, b"line1\nline2\nline3\n");
        assert!(
            !bytes.starts_with(&[0xEF, 0xBB, 0xBF]),
            "no UTF-8 BOM should remain in generated files"
        );
    }

    #[test]
    fn normalize_for_write_is_identity_when_already_clean() {
        let s = "a\nb\nc";
        assert_eq!(normalize_for_write(s), s);
    }

    #[test]
    fn load_rejects_invalid_utf8_bytes() {
        let dir = TempDir::new().unwrap();
        // 0xFF on its own is never valid UTF-8 (lead bytes 0xFE/0xFF are
        // reserved) — and there's no BOM signature, so the decode step
        // is what catches it.
        write_main_bytes(dir.path(), &[b'{', 0xFF, b'}']);
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(
            err.message.contains("not valid UTF-8"),
            "got: {}",
            err.message
        );
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
        assert!(err.message.contains("description"), "got: {}", err.message);
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
        assert!(
            err.message.contains("must not be empty"),
            "got: {}",
            err.message
        );
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
            &format!(
                r#"{{ "name": "a", "version": "0.1.0", "description": "{long}", "language": "en-US", "encoding": "utf-8" }}"#
            ),
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
    fn load_parses_named_sftp_service_with_password_auth() {
        // Issue #27 acceptance: `services.<name>.kind == "sftp"` resolves
        // into `manifest.sftp_services` with the auth method classified.
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
                    "files": {
                        "kind": "sftp",
                        "host": "env:SFTP_HOST",
                        "port": 2222,
                        "user": "env:SFTP_USER",
                        "auth": { "password": "env:SFTP_PASSWORD" },
                        "host_key_fingerprint": "SHA256:abc"
                    }
                }
            }"#,
        );
        let m = Manifest::load(dir.path()).unwrap();
        assert_eq!(m.sftp_services.len(), 1);
        let svc = &m.sftp_services[0];
        assert_eq!(svc.name, "files");
        assert_eq!(svc.config.port, 2222);
        assert_eq!(svc.auth_method, SftpAuthMethod::Password);
        match &svc.config.host {
            ServiceUrl::Env(v) => assert_eq!(v, "SFTP_HOST"),
            other => panic!("expected env, got {other:?}"),
        }
    }

    #[test]
    fn load_rejects_sftp_service_with_multiple_auth_methods() {
        // `auth` must hold exactly one of password/private_key/private_key_pem.
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
                    "files": {
                        "kind": "sftp",
                        "host": "h",
                        "user": "u",
                        "auth": { "password": "p", "private_key_pem": "pem" }
                    }
                }
            }"#,
        );
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(
            err.message.contains("services.files")
                && err.message.contains("password, private_key_pem"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn load_rejects_sftp_service_without_auth_method() {
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
                    "files": {
                        "kind": "sftp",
                        "host": "h",
                        "user": "u",
                        "auth": { "passphrase": "p" }
                    }
                }
            }"#,
        );
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(
            err.message.contains("services.files") && err.message.contains("exactly one of"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn load_rejects_sftp_service_with_unknown_field() {
        // `deny_unknown_fields` on `SftpService` catches typos at parse time.
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
                    "files": {
                        "kind": "sftp",
                        "host": "h",
                        "user": "u",
                        "auth": { "password": "p" },
                        "junk": "boom"
                    }
                }
            }"#,
        );
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(err.message.contains("junk"), "got: {}", err.message);
    }

    #[test]
    fn load_collects_multiple_sftp_services_in_declaration_order() {
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
                    "files": {
                        "kind": "sftp",
                        "host": "h1",
                        "user": "u1",
                        "auth": { "password": "p1" },
                        "host_key_fingerprint": "SHA256:1"
                    },
                    "backups": {
                        "kind": "sftp",
                        "host": "h2",
                        "user": "u2",
                        "auth": { "private_key": "/k" },
                        "host_key_fingerprint": "SHA256:2"
                    }
                }
            }"#,
        );
        let m = Manifest::load(dir.path()).unwrap();
        assert_eq!(m.sftp_services.len(), 2);
        assert_eq!(m.sftp_services[0].name, "files");
        assert_eq!(m.sftp_services[0].auth_method, SftpAuthMethod::Password);
        assert_eq!(m.sftp_services[1].name, "backups");
        assert_eq!(m.sftp_services[1].auth_method, SftpAuthMethod::PrivateKey);
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

    #[test]
    fn load_rejects_missing_logging() {
        // Issue #17: `logging` is mandatory. A manifest with every other
        // required field but no `logging` block must fail at load with an
        // error pointing at `main.json` so the dev overlay surfaces it.
        let dir = TempDir::new().unwrap();
        // Bypass the test helper that backfills `logging`.
        fs::write(
            dir.path().join("main.json"),
            r#"{ "name": "a", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8" }"#,
        )
        .unwrap();
        let err = Manifest::load(dir.path()).unwrap_err();
        assert_eq!(err.file, dir.path().join("main.json"));
        assert!(
            err.message.contains("logging"),
            "missing-field error must mention `logging`, got: {}",
            err.message
        );
    }

    #[test]
    fn load_rejects_logging_without_level() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("main.json"),
            r#"{ "name": "a", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8", "logging": {} }"#,
        )
        .unwrap();
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(
            err.message.contains("level"),
            "missing-field error must mention `level`, got: {}",
            err.message
        );
    }

    #[test]
    fn load_rejects_logging_unknown_field() {
        // `deny_unknown_fields` on LoggingRaw — only `level` and `include`
        // are accepted. A typo on a sub-field fails the load with the
        // offending key named.
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8", "logging": { "level": "info", "fancy": true } }"#,
        );
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(err.message.contains("fancy"), "got: {}", err.message);
    }

    #[test]
    fn load_rejects_logging_invalid_level() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8", "logging": { "level": "verbose" } }"#,
        );
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(
            err.message.contains("verbose"),
            "error must name the rejected value, got: {}",
            err.message
        );
        assert!(
            err.message.contains("trace/debug/info/warn/error"),
            "error must list the accepted values, got: {}",
            err.message
        );
    }

    #[test]
    fn load_dotenv_defaults_to_auto_when_field_omitted() {
        // Issue #52: omitting the field is the canonical default — there is
        // no writable spelling for `Auto`.
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8" }"#,
        );
        let m = Manifest::load(dir.path()).unwrap();
        assert_eq!(m.load_dotenv, LoadDotenv::Auto);
    }

    #[test]
    fn load_dotenv_false_resolves_to_disabled() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8", "load_dotenv": false }"#,
        );
        let m = Manifest::load(dir.path()).unwrap();
        assert_eq!(m.load_dotenv, LoadDotenv::Disabled);
    }

    #[test]
    fn load_dotenv_rejects_explicit_true_as_redundant() {
        // "one feature = one declarative form" — `true` would be a second
        // way to spell the default. Reject at load time with a hint.
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8", "load_dotenv": true }"#,
        );
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(
            err.message.contains("omit the field"),
            "error must point at the canonical form: {}",
            err.message
        );
    }

    #[test]
    fn load_dotenv_relative_string_resolves_against_project_dir() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8", "load_dotenv": ".env.shared" }"#,
        );
        let m = Manifest::load(dir.path()).unwrap();
        match m.load_dotenv {
            LoadDotenv::Path(p) => {
                assert!(p.is_absolute(), "relative path must be made absolute");
                assert_eq!(p, dir.path().join(".env.shared"));
            }
            other => panic!("expected Path, got {other:?}"),
        }
    }

    #[test]
    fn load_dotenv_absolute_string_is_kept_as_is() {
        let dir = TempDir::new().unwrap();
        let other = TempDir::new().unwrap();
        let abs = other.path().join(".env.deploy");
        let body = format!(
            "{{ \"name\": \"a\", \"version\": \"0.1.0\", \"description\": \"x\", \"language\": \"en-US\", \"encoding\": \"utf-8\", \"load_dotenv\": {} }}",
            serde_json::to_string(&abs.display().to_string()).unwrap()
        );
        write_main(dir.path(), &body);
        let m = Manifest::load(dir.path()).unwrap();
        match m.load_dotenv {
            LoadDotenv::Path(p) => assert_eq!(p, abs),
            other => panic!("expected Path, got {other:?}"),
        }
    }

    #[test]
    fn load_dotenv_rejects_directory_path() {
        // A typo that resolves to a directory would surface only at runtime
        // through dotenvy — too far from `main.json` to be useful. Catch it
        // at parse time so the dev overlay points at the manifest.
        let dir = TempDir::new().unwrap();
        let subdir = dir.path().join("envdir");
        fs::create_dir_all(&subdir).unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8", "load_dotenv": "envdir" }"#,
        );
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(
            err.message.contains("is a directory"),
            "error must mention the directory case: {}",
            err.message
        );
    }

    #[test]
    fn load_dotenv_rejects_empty_or_whitespace_string() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8", "load_dotenv": "   " }"#,
        );
        let err = Manifest::load(dir.path()).unwrap_err();
        assert!(
            err.message.contains("must not be empty"),
            "error must reject empty/whitespace strings: {}",
            err.message
        );
    }

    #[test]
    fn load_parses_logging_include_with_env_prefix() {
        let dir = TempDir::new().unwrap();
        write_main(
            dir.path(),
            r#"{ "name": "a", "version": "0.1.0", "description": "x", "language": "en-US", "encoding": "utf-8", "logging": { "level": "info", "include": { "service": "myblog", "env": "env:RUST_ENV" } } }"#,
        );
        let m = Manifest::load(dir.path()).unwrap();
        assert_eq!(m.logging.level, LogLevel::Info);
        match m.logging.include.get("service") {
            Some(LogIncludeValue::Literal(s)) => assert_eq!(s, "myblog"),
            other => panic!("expected literal, got {other:?}"),
        }
        match m.logging.include.get("env") {
            Some(LogIncludeValue::Env(v)) => assert_eq!(v, "RUST_ENV"),
            other => panic!("expected env, got {other:?}"),
        }
    }
}
