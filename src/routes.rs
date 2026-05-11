//! Route discovery and parsing for `routes/*.json`.
//!
//! Each file under `<project>/routes/` declares one HTTP endpoint. The full
//! schema (input, process, view, output, redirect) is documented in
//! `docs/routes.md`; only fields the compiler currently consumes are surfaced
//! here. Unknown fields are accepted silently so partial implementations of
//! later v1 slices don't break manifest loading.

use indexmap::IndexMap;
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::manifest::ManifestError;

/// One declared HTTP route, after parsing + validation.
///
/// The `name` is derived from the file path (stem, with `/` replaced by `_`)
/// and used to mint a unique handler identifier in the generated source.
#[derive(Debug)]
pub struct Route {
    /// Absolute path to the JSON file this route came from. Used to point
    /// manifest errors at the right place to edit (see issue #2).
    pub source: PathBuf,
    pub name: String,
    pub path: String,
    pub method: HttpMethod,
    pub kind: RouteKind,
    pub template: Option<String>,
    /// Layout name (without extension). Resolved against `layouts/` at codegen time.
    pub layout: Option<String>,
    /// Optional CEL guard. Validated at parse time; runtime evaluation
    /// (returning 403 on `false`) lands once process blocks execute.
    #[allow(dead_code)]
    pub guard: Option<String>,
    /// Declared process blocks. Slice 3 only reads `name`, `block`, `table`
    /// for type inference of the page context — the blocks are not executed.
    pub process: Vec<ProcessBlock>,
    /// Declared view bindings. Each value is the raw JSON expression, kept as
    /// a string for now (literals, `$ref`, `$ref.field`). See `docs/routes.md`.
    pub view: IndexMap<String, String>,
}

/// One declared process block, minimally parsed.
///
/// `block`/`table` are the only fields slice 3 consumes (for context type
/// inference); execution semantics land in slice 5. Unknown fields are
/// accepted silently via `extra` so partial schemas keep parsing.
#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub struct ProcessBlock {
    /// Variable name the block produces. Optional because write-side blocks
    /// (e.g. `db.insert` in a POST handler) don't bind a value that the view
    /// can reference.
    #[serde(default)]
    #[schemars(default)]
    pub name: Option<String>,
    pub block: String,
    #[serde(default)]
    #[schemars(default)]
    pub table: Option<String>,
    #[serde(flatten)]
    #[allow(dead_code)]
    pub extra: IndexMap<String, serde_json::Value>,
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
    /// Optional CEL expression evaluated before handler execution. When the
    /// expression returns `false`, the response is `403 Forbidden`.
    #[serde(default)]
    #[schemars(default)]
    guard: Option<String>,
    #[serde(default)]
    #[schemars(default)]
    process: Vec<ProcessBlock>,
    #[serde(default)]
    #[schemars(default)]
    view: IndexMap<String, String>,
}

impl Route {
    /// Discover and parse every `routes/**/*.json` under `project_dir`.
    ///
    /// Returns an empty vec if the `routes/` directory is missing — projects
    /// without any routes are valid (only `/health` will be served).
    pub fn load_all(project_dir: &Path) -> Result<Vec<Route>, ManifestError> {
        let routes_dir = project_dir.join("routes");
        if !routes_dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut files: Vec<PathBuf> = Vec::new();
        collect_json(&routes_dir, &mut files);
        files.sort();

        let mut routes = Vec::with_capacity(files.len());
        let mut seen: HashMap<(HttpMethod, String), PathBuf> = HashMap::new();
        for file in &files {
            let content =
                std::fs::read_to_string(file).map_err(|e| ManifestError::read(file, e))?;
            let raw: RawRoute =
                serde_json::from_str(&content).map_err(|e| ManifestError::parse(file, e))?;
            let name = derive_name(&routes_dir, file);
            // Validate every CEL snippet syntactically before we commit
            // the route into the manifest. `process.<block>.where` is
            // optional and only present when the user wired it.
            if let Some(g) = raw.guard.as_deref() {
                crate::expressions::validate(g, file, "route.guard")?;
            }
            for (idx, pb) in raw.process.iter().enumerate() {
                if let Some(serde_json::Value::String(expr)) = pb.extra.get("where") {
                    crate::expressions::validate(expr, file, &format!("process[{idx}].where"))?;
                }
            }
            let route = Route {
                source: file.clone(),
                name,
                path: raw.path,
                method: raw.method,
                kind: raw.kind,
                template: raw.template,
                layout: raw.layout,
                guard: raw.guard,
                process: raw.process,
                view: raw.view,
            };
            route.validate(file)?;
            let key = (route.method, route.path.clone());
            if let Some(prev_file) = seen.get(&key) {
                return Err(ManifestError::validation(
                    file,
                    format!(
                        "duplicate route `{:?} {}` — also declared by `{}`",
                        route.method,
                        route.path,
                        prev_file.display()
                    ),
                ));
            }
            seen.insert(key, file.clone());
            routes.push(route);
        }
        Ok(routes)
    }

    /// Catch shape errors at load time so codegen never has to defend against them.
    fn validate(&self, source: &Path) -> Result<(), ManifestError> {
        if !self.path.starts_with('/') {
            return Err(ManifestError::validation(
                source,
                "`path` must start with `/`",
            ));
        }
        if self.kind == RouteKind::Page && self.method == HttpMethod::Get && self.template.is_none()
        {
            return Err(ManifestError::validation(
                source,
                "`kind: page` GET routes must declare a `template`",
            ));
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
        assert_eq!(axum_path("/posts/:slug/comments"), "/posts/{slug}/comments");
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
        write_route(
            &routes_dir,
            "home.json",
            "/",
            "GET",
            "page",
            Some("home.html"),
        );
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
        write_route(
            &routes_dir,
            "bad.json",
            "no-slash",
            "GET",
            "page",
            Some("x.html"),
        );
        let err = Route::load_all(dir.path()).unwrap_err();
        assert_eq!(err.file, routes_dir.join("bad.json"));
        assert!(
            err.message.contains("`path` must start with `/`"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn load_all_requires_template_for_get_page_routes() {
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        write_route(&routes_dir, "home.json", "/", "GET", "page", None);
        let err = Route::load_all(dir.path()).unwrap_err();
        assert_eq!(err.file, routes_dir.join("home.json"));
        assert!(
            err.message.contains("must declare a `template`"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn load_all_rejects_duplicate_method_path_pairs() {
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        write_route(&routes_dir, "a.json", "/", "GET", "page", Some("x.html"));
        write_route(&routes_dir, "b.json", "/", "GET", "page", Some("y.html"));
        let err = Route::load_all(dir.path()).unwrap_err();
        // The error points at the second occurrence; the message names the first.
        assert_eq!(err.file, routes_dir.join("b.json"));
        assert!(
            err.message.contains("duplicate route"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.contains("a.json"),
            "duplicate error must mention the other file: {}",
            err.message
        );
    }

    #[test]
    fn load_all_rejects_route_with_invalid_cel_guard() {
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("home.json"),
            r#"{
                "path": "/admin",
                "method": "GET",
                "kind": "page",
                "template": "admin.html",
                "guard": "user.is_admin &&"
            }"#,
        )
        .unwrap();
        let err = Route::load_all(dir.path()).unwrap_err();
        assert!(
            err.message.contains("invalid CEL expression"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("route.guard"), "got: {}", err.message);
    }

    #[test]
    fn load_all_accepts_well_formed_cel_guard() {
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
                "guard": "user.is_admin"
            }"#,
        )
        .unwrap();
        let routes = Route::load_all(dir.path()).unwrap();
        assert_eq!(routes[0].guard.as_deref(), Some("user.is_admin"));
    }

    #[test]
    fn load_all_validates_process_where_cel_syntax() {
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("posts.json"),
            r#"{
                "path": "/posts",
                "method": "GET",
                "kind": "api",
                "process": [
                    { "name": "posts", "block": "db.find_many", "table": "posts", "where": "post.author_id ==" }
                ]
            }"#,
        )
        .unwrap();
        let err = Route::load_all(dir.path()).unwrap_err();
        assert!(
            err.message.contains("process[0].where"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn load_all_accepts_unknown_fields() {
        // Slice 2+ fields (input/output/...) must not break parsing; process
        // and view are typed but optional.
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
                "process": [{ "name": "posts", "block": "db.find_many", "table": "posts" }],
                "view": { "page_title": "x", "posts": "$posts" }
            }"#,
        )
        .unwrap();
        let routes = Route::load_all(dir.path()).unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].path, "/");
        assert_eq!(routes[0].process.len(), 1);
        assert_eq!(routes[0].process[0].name.as_deref(), Some("posts"));
        assert_eq!(
            routes[0].view.get("page_title").map(|s| s.as_str()),
            Some("x")
        );
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
