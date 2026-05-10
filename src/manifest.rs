//! `main.json` parsing and validation.
//!
//! The manifest is the user-facing entry point of every rublocks project.
//! Schema and field semantics are documented in `docs/manifest.md`.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

use crate::routes::Route;

/// Top-level shape of `main.json` plus everything discovered alongside it.
///
/// `name` and `services` come from `main.json`. `routes` is populated by
/// scanning `routes/*.json` at load time, so codegen sees a single fully
/// resolved manifest. The multi-file plan (models, jobs) is documented in
/// `docs/manifest.md` and lands in subsequent slices.
#[derive(Debug)]
pub struct Manifest {
    /// Application name. Becomes the cargo crate name in the generated project.
    pub name: String,
    pub services: Services,
    pub routes: Vec<Route>,
}

#[derive(Debug, Deserialize)]
struct RawManifest {
    name: String,
    #[serde(default)]
    services: Services,
}

/// Optional service declarations. Each present service triggers conditional
/// dependency wiring in the generated `Cargo.toml` and `AppState`.
#[derive(Debug, Default, Deserialize)]
pub struct Services {
    pub postgres: Option<PostgresService>,
    pub redis: Option<RedisService>,
}

/// Postgres service configuration. Currently only the connection URL.
#[derive(Debug, Deserialize)]
pub struct PostgresService {
    pub url: ServiceUrl,
}

/// Redis service configuration. Currently only the connection URL.
#[derive(Debug, Deserialize)]
pub struct RedisService {
    pub url: ServiceUrl,
}

/// A connection URL accepted by service declarations.
///
/// - `Literal("postgres://...")` is embedded directly in the generated source.
/// - `Env("DATABASE_URL")` becomes `std::env::var("DATABASE_URL")?` at startup.
///
/// The `env:` prefix is the recommended form for any secret-like value
/// (see `docs/manifest.md`).
#[derive(Debug)]
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
    /// Errors carry the file path so codegen failures don't read like opaque
    /// JSON errors floating in space.
    pub fn load(project_dir: &Path) -> Result<Self> {
        let path = project_dir.join("main.json");
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let raw: RawManifest = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        validate_name(&raw.name)?;
        let routes = Route::load_all(project_dir)?;
        Ok(Manifest {
            name: raw.name,
            services: raw.services,
            routes,
        })
    }
}

/// Enforce that `name` is a valid cargo crate name.
///
/// We catch this at manifest load instead of letting `cargo build` reject it
/// later — saves the user a pointless rebuild loop.
fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    anyhow::ensure!(
        ok,
        "invalid app name `{}`: must be lowercase ascii letters, digits, `_` or `-`",
        name
    );
    Ok(())
}
