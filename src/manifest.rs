//! `main.json` parsing and validation.
//!
//! The manifest is the user-facing entry point of every rublocks project.
//! Schema and field semantics are documented in `docs/manifest.md`.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

/// Top-level shape of `main.json`.
///
/// Currently only `name` and optional `services` are accepted; the multi-file
/// plan (routes, models, jobs) is documented in `docs/manifest.md` and not yet
/// implemented.
#[derive(Debug, Deserialize)]
pub struct Manifest {
    /// Application name. Becomes the cargo crate name in the generated project.
    pub name: String,
    #[serde(default)]
    pub services: Services,
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
    /// Read and validate `<project_dir>/main.json`.
    ///
    /// Errors carry the file path so codegen failures don't read like opaque
    /// JSON errors floating in space.
    pub fn load(project_dir: &Path) -> Result<Self> {
        let path = project_dir.join("main.json");
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let manifest: Manifest = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Enforce that `name` is a valid cargo crate name.
    ///
    /// We catch this at manifest load instead of letting `cargo build` reject it
    /// later — saves the user a pointless rebuild loop.
    fn validate(&self) -> Result<()> {
        let name_ok = !self.name.is_empty()
            && self
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
        anyhow::ensure!(
            name_ok,
            "invalid app name `{}`: must be lowercase ascii letters, digits, `_` or `-`",
            self.name
        );
        Ok(())
    }
}
