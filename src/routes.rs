//! Route discovery and parsing for `routes/*.json`.
//!
//! Each file under `<project>/routes/` declares one HTTP endpoint. The full
//! schema (input, process, view, output, redirect) is documented in
//! `docs/routes.md`; only fields the compiler currently consumes are surfaced
//! here. Unknown fields are accepted silently so partial implementations of
//! later v1 slices don't break manifest loading.

use anyhow::{Context, Result};
use schemars::{schema::RootSchema, schema_for, JsonSchema};
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

#[derive(Debug, Deserialize, JsonSchema, PartialEq, Eq, Hash, Clone, Copy)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
    Patch,
}

#[derive(Debug, Deserialize, JsonSchema, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum RouteKind {
    Page,
    Api,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(title = "rublocks route")]
struct RawRoute {
    /// HTTP path. Must start with `/`. Use `:name` for path parameters
    /// (rewritten to `{name}` for Axum at codegen time).
    path: String,
    method: HttpMethod,
    kind: RouteKind,
    /// Template file (under `templates/`). Required for `kind: page` GET routes.
    #[serde(default)]
    #[schemars(default)]
    template: Option<String>,
    /// Layout name (without extension). Resolved against `layouts/` at codegen time.
    #[serde(default)]
    #[schemars(default)]
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

/// JSON Schema describing the on-disk shape of `routes/*.json`.
///
/// Derived from `RawRoute` so the schema is always in sync with what the parser
/// actually accepts. Consumed by the agent installers in `src/agents.rs`.
pub fn json_schema() -> RootSchema {
    schema_for!(RawRoute)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn axum_path_passes_static_paths_through() {
        assert_eq!(axum_path("/"), "/");
        assert_eq!(axum_path("/posts"), "/posts");
        assert_eq!(axum_path("/api/posts"), "/api/posts");
    }

    #[test]
    fn axum_path_converts_colon_params() {
        assert_eq!(axum_path("/posts/:slug"), "/posts/{slug}");
        assert_eq!(axum_path("/a/:b/c/:d"), "/a/{b}/c/{d}");
        assert_eq!(
            axum_path("/posts/:slug/comments"),
            "/posts/{slug}/comments"
        );
    }

    #[test]
    fn load_all_returns_empty_when_no_routes_dir() {
        let dir = TempDir::new().unwrap();
        let routes = Route::load_all(dir.path()).unwrap();
        assert!(routes.is_empty());
    }

    #[test]
    fn load_all_derives_names_from_file_paths() {
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(routes_dir.join("posts")).unwrap();
        write_route(&routes_dir, "home.json", "/", "GET", "page", Some("home.html"));
        write_route(
            &routes_dir,
            "api-posts-list.json",
            "/api/posts",
            "GET",
            "api",
            None,
        );
        write_route(
            &routes_dir,
            "posts/show.json",
            "/posts/:slug",
            "GET",
            "page",
            Some("posts/show.html"),
        );

        let mut names: Vec<String> = Route::load_all(dir.path())
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "api_posts_list".to_string(),
                "home".to_string(),
                "posts_show".to_string(),
            ]
        );
    }

    #[test]
    fn load_all_rejects_path_without_leading_slash() {
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        write_route(&routes_dir, "bad.json", "no-slash", "GET", "page", Some("x.html"));
        let err = Route::load_all(dir.path()).unwrap_err().to_string();
        assert!(err.contains("`path` must start with `/`"), "got: {err}");
    }

    #[test]
    fn load_all_requires_template_for_get_page_routes() {
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        write_route(&routes_dir, "home.json", "/", "GET", "page", None);
        let err = Route::load_all(dir.path()).unwrap_err().to_string();
        assert!(err.contains("must declare a `template`"), "got: {err}");
    }

    #[test]
    fn load_all_rejects_duplicate_method_path_pairs() {
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        write_route(&routes_dir, "a.json", "/", "GET", "page", Some("x.html"));
        write_route(&routes_dir, "b.json", "/", "GET", "page", Some("y.html"));
        let err = Route::load_all(dir.path()).unwrap_err().to_string();
        assert!(err.contains("duplicate route"), "got: {err}");
    }

    #[test]
    fn load_all_accepts_unknown_fields() {
        // Slice 2+ fields (input/process/view/...) must not break parsing.
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
                "input": { "query": { "limit": { "type": "int" } } },
                "process": [{ "block": "db.find_many", "table": "posts" }],
                "view": { "page_title": "x" }
            }"#,
        )
        .unwrap();
        let routes = Route::load_all(dir.path()).unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].path, "/");
    }

    fn write_route(
        routes_dir: &std::path::Path,
        rel: &str,
        path: &str,
        method: &str,
        kind: &str,
        template: Option<&str>,
    ) {
        let body = match template {
            Some(t) => format!(
                r#"{{"path":"{path}","method":"{method}","kind":"{kind}","template":"{t}"}}"#
            ),
            None => format!(r#"{{"path":"{path}","method":"{method}","kind":"{kind}"}}"#),
        };
        let dest = routes_dir.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(dest, body).unwrap();
    }
}
