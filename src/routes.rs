//! Route discovery and parsing for `routes/*.json`.
//!
//! Each file under `<project>/routes/` declares one HTTP endpoint. The full
//! schema (input, process, view, output, redirect) is documented in
//! `docs/routes.md`; only fields the compiler currently consumes are surfaced
//! here. Unknown fields are accepted silently so partial implementations of
//! later v1 slices don't break manifest loading.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// One declared HTTP route, after parsing + validation.
///
/// The `name` is derived from the file path (stem, with `/` replaced by `_`)
/// and used to mint a unique handler identifier in the generated source.
#[derive(Debug)]
pub struct Route {
    pub name: String,
    pub path: String,
    pub method: HttpMethod,
    pub kind: RouteKind,
    pub template: Option<String>,
    /// Layout name (without extension). Unused in slice 1; consumed by the
    /// template-rendering slice that wires Askama inheritance.
    #[allow(dead_code)]
    pub layout: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Hash, Clone, Copy)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
    Patch,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum RouteKind {
    Page,
    Api,
}

#[derive(Debug, Deserialize)]
struct RawRoute {
    path: String,
    method: HttpMethod,
    kind: RouteKind,
    #[serde(default)]
    template: Option<String>,
    #[serde(default)]
    layout: Option<String>,
}

impl Route {
    /// Discover and parse every `routes/**/*.json` under `project_dir`.
    ///
    /// Returns an empty vec if the `routes/` directory is missing — projects
    /// without any routes are valid (only `/health` will be served).
    pub fn load_all(project_dir: &Path) -> Result<Vec<Route>> {
        let routes_dir = project_dir.join("routes");
        if !routes_dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut files: Vec<PathBuf> = Vec::new();
        collect_json(&routes_dir, &mut files);
        files.sort();

        let mut routes = Vec::with_capacity(files.len());
        for file in &files {
            let content = std::fs::read_to_string(file)
                .with_context(|| format!("failed to read {}", file.display()))?;
            let raw: RawRoute = serde_json::from_str(&content)
                .with_context(|| format!("failed to parse {}", file.display()))?;
            let name = derive_name(&routes_dir, file);
            let route = Route {
                name,
                path: raw.path,
                method: raw.method,
                kind: raw.kind,
                template: raw.template,
                layout: raw.layout,
            };
            route.validate(file)?;
            routes.push(route);
        }
        ensure_no_duplicates(&routes)?;
        Ok(routes)
    }

    /// Catch shape errors at load time so codegen never has to defend against them.
    fn validate(&self, source: &Path) -> Result<()> {
        anyhow::ensure!(
            self.path.starts_with('/'),
            "{}: `path` must start with `/`",
            source.display()
        );
        if self.kind == RouteKind::Page && self.method == HttpMethod::Get {
            anyhow::ensure!(
                self.template.is_some(),
                "{}: `kind: page` GET routes must declare a `template`",
                source.display()
            );
        }
        Ok(())
    }
}

fn derive_name(routes_dir: &Path, file: &Path) -> String {
    let rel = file.strip_prefix(routes_dir).unwrap_or(file);
    let stem = rel.with_extension("");
    stem.to_string_lossy()
        .chars()
        .map(|c| match c {
            '/' | '\\' | '-' | '.' => '_',
            other => other,
        })
        .collect()
}

fn ensure_no_duplicates(routes: &[Route]) -> Result<()> {
    let mut seen = std::collections::HashMap::new();
    for r in routes {
        let key = (r.method, r.path.as_str());
        if let Some(prev) = seen.insert(key, &r.name) {
            anyhow::bail!(
                "duplicate route: `{:?} {}` declared by both `{}` and `{}`",
                r.method,
                r.path,
                prev,
                r.name
            );
        }
    }
    Ok(())
}

fn collect_json(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_json(&path, out);
        } else if path.extension().and_then(|x| x.to_str()) == Some("json") {
            out.push(path);
        }
    }
}

/// Convert a rublocks path (`/posts/:slug`) to the matchit form Axum 0.8
/// expects (`/posts/{slug}`). Authors keep the familiar `:` syntax in JSON.
pub fn axum_path(path: &str) -> String {
    path.split('/')
        .map(|seg| {
            if let Some(name) = seg.strip_prefix(':') {
                format!("{{{name}}}")
            } else {
                seg.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}
