use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub name: String,
    #[serde(default)]
    pub services: Services,
}

#[derive(Debug, Default, Deserialize)]
pub struct Services {
    pub postgres: Option<PostgresService>,
    pub redis: Option<RedisService>,
}

#[derive(Debug, Deserialize)]
pub struct PostgresService {
    pub url: ServiceUrl,
}

#[derive(Debug, Deserialize)]
pub struct RedisService {
    pub url: ServiceUrl,
}

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
    pub fn load(project_dir: &Path) -> Result<Self> {
        let path = project_dir.join("main.json");
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let manifest: Manifest = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        manifest.validate()?;
        Ok(manifest)
    }

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
