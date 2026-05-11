//! Route discovery and parsing for `routes/*.json`.
//!
//! Each file under `<project>/routes/` declares one HTTP endpoint. The
//! per-block behaviour lives under `src/blocks/`; this module owns the
//! route-level fields (`path`, `method`, `kind`, `template`, `layout`,
//! `process`, `view`) and converts the raw `process` array into typed
//! [`BlockInstance`]s by dispatching against the registry. Authorization
//! is itself a block (`guard`) — there is no `route.guard` field.
//!
//! Unknown fields are rejected (`deny_unknown_fields`) so typos in route
//! files fail fast with a pointer to the offending file. Fields that are
//! recognised but not yet fully typed (`input`, `output`, `on_missing`,
//! `redirect`, `summary`, `description`, `tags`) are accepted as opaque
//! JSON until their dedicated slices land.

use indexmap::IndexMap;
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::blocks::{self, BlockInstance};
use crate::input::InputSpec;
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
    /// Declared process blocks, each parsed against the registry under
    /// `src/blocks/`. Unknown block ids and unknown per-block fields are
    /// already rejected — codegen can iterate freely.
    pub process: Vec<Box<dyn BlockInstance>>,
    /// Declared view bindings. Each value is the raw JSON expression, kept as
    /// a string for now (literals, `$ref`, `$ref.field`). See `docs/routes.md`.
    pub view: IndexMap<String, String>,
    /// Typed input spec. Per-field constraints are validated at load time
    /// (regex compiled, CEL syntax-checked, defaults type-matched against
    /// their declared kind); the auto-generated validator at codegen time
    /// derives from this exact structure — no `validate.input` block needed.
    pub input: Option<InputSpec>,
    /// Output binding map for `kind: api`. Raw JSON for now; the typed
    /// projection ships with the API response codegen.
    #[allow(dead_code)]
    pub output: Option<Value>,
    /// Redirect spec. Raw JSON for now.
    #[allow(dead_code)]
    pub redirect: Option<Value>,
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
#[serde(deny_unknown_fields)]
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
    /// Process blocks. Each entry is dispatched against the registry under
    /// `src/blocks/` — its full schema lives at `docs/blocks/<id>.md`.
    #[serde(default)]
    #[schemars(default)]
    process: Vec<Value>,
    #[serde(default)]
    #[schemars(default)]
    view: IndexMap<String, String>,
    /// Input spec. Per-field schema documented in `docs/input.md`.
    #[serde(default)]
    #[schemars(default)]
    input: Option<Value>,
    /// Output binding map. See `docs/routes.md`.
    #[serde(default)]
    #[schemars(default)]
    output: Option<Value>,
    /// Redirect spec. See `docs/routes.md`.
    #[serde(default)]
    #[schemars(default)]
    redirect: Option<Value>,
    /// OpenAPI summary. Consumed by the `kind: api` spec emitter.
    #[serde(default)]
    #[schemars(default)]
    #[allow(dead_code)]
    summary: Option<String>,
    /// OpenAPI description.
    #[serde(default)]
    #[schemars(default)]
    #[allow(dead_code)]
    description: Option<String>,
    /// OpenAPI tags.
    #[serde(default)]
    #[schemars(default)]
    #[allow(dead_code)]
    tags: Vec<String>,
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
            let process = parse_process(&raw.process, file)?;
            let input = match raw.input.as_ref() {
                Some(v) => Some(InputSpec::parse(v, file)?),
                None => None,
            };
            let route = Route {
                source: file.clone(),
                name,
                path: raw.path,
                method: raw.method,
                kind: raw.kind,
                template: raw.template,
                layout: raw.layout,
                process,
                view: raw.view,
                input,
                output: raw.output,
                redirect: raw.redirect,
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

/// Walk the raw `process` array and dispatch each entry against the block
/// registry. The `process[<idx>]` label lands in error messages so the user
/// can locate the offending element inside their JSON file.
pub(crate) fn parse_process(
    raw: &[Value],
    file: &Path,
) -> Result<Vec<Box<dyn BlockInstance>>, ManifestError> {
    let mut out = Vec::with_capacity(raw.len());
    for (idx, v) in raw.iter().enumerate() {
        let raw_block = blocks::RawBlock::from_value(v, file, &format!("process[{idx}]"))?;
        out.push(blocks::parse(&raw_block)?);
    }
    Ok(out)
}

/// JSON Schema describing the on-disk shape of `routes/*.json`.
///
/// Derived from `RawRoute` so the schema is always in sync with what the parser
/// actually accepts. The per-block surface inside `process[*]` is documented
/// separately under `docs/blocks/` and exposed via `blocks::registry`.
pub fn json_schema() -> RootSchema {
    schema_for!(RawRoute)
}

/// Parse a string against the route shape. Used by the doc examples test
/// to guarantee every `<!-- rb:route -->` block in `docs/*.md` still maps
/// onto the parser the binary actually runs — including the per-block
/// dispatch inside `process`.
#[cfg(test)]
pub(crate) fn validate_doc_example(s: &str) -> Result<(), String> {
    let raw: RawRoute = serde_json::from_str(s).map_err(|e| e.to_string())?;
    let fake = std::path::PathBuf::from("<doc example>");
    parse_process(&raw.process, &fake).map_err(|e| e.message)?;
    if let Some(v) = raw.input.as_ref() {
        InputSpec::parse(v, &fake).map_err(|e| e.message)?;
    }
    Ok(())
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
    fn load_all_rejects_guard_block_with_invalid_cel() {
        // The `guard` block carries the CEL predicate in `if`. Syntactic
        // errors surface with the offending block label.
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
                "process": [
                    { "block": "guard", "if": "user.is_admin &&" }
                ]
            }"#,
        )
        .unwrap();
        let err = Route::load_all(dir.path()).unwrap_err();
        assert!(
            err.message.contains("invalid CEL expression"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.contains("process[0].if"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn load_all_accepts_well_formed_guard_block() {
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
                    { "block": "guard", "if": "user.is_admin" }
                ]
            }"#,
        )
        .unwrap();
        let routes = Route::load_all(dir.path()).unwrap();
        assert_eq!(routes[0].process.len(), 1);
        assert_eq!(routes[0].process[0].kind_id(), "guard");
    }

    #[test]
    fn load_all_rejects_route_level_guard_field() {
        // Authorization is a `process` block — there is no `route.guard`
        // field. The schema rejects it via `deny_unknown_fields`, locking
        // the "one feature = one declarative form" rule from CLAUDE.md.
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
        let err = Route::load_all(dir.path()).unwrap_err();
        assert!(
            err.message.contains("guard"),
            "deny_unknown_fields must surface the legacy field: {}",
            err.message
        );
    }

    #[test]
    fn load_all_validates_process_where_cel_syntax() {
        // String-form `where` is a CEL predicate — surfaces an error with
        // the offending block label.
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
    fn load_all_rejects_unknown_block_with_catalogue() {
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
                "process": [
                    { "name": "posts", "block": "db.find_meny", "table": "posts" }
                ]
            }"#,
        )
        .unwrap();
        let err = Route::load_all(dir.path()).unwrap_err();
        assert!(
            err.message.contains("unknown block `db.find_meny`"),
            "expected catalogue error, got: {}",
            err.message
        );
        assert!(err.message.contains("db.find_many"));
    }

    #[test]
    fn load_all_rejects_unknown_field_on_block() {
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
                "process": [
                    { "name": "posts", "block": "db.find_many", "table": "posts", "ordr": "title" }
                ]
            }"#,
        )
        .unwrap();
        let err = Route::load_all(dir.path()).unwrap_err();
        assert!(
            err.message.contains("process[0]") && err.message.contains("ordr"),
            "expected per-block unknown-field error, got: {}",
            err.message
        );
    }

    #[test]
    fn load_all_rejects_unknown_route_field() {
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
                "blocs": []
            }"#,
        )
        .unwrap();
        let err = Route::load_all(dir.path()).unwrap_err();
        assert!(
            err.message.contains("blocs"),
            "deny_unknown_fields must surface the typo: {}",
            err.message
        );
    }

    #[test]
    fn load_all_parses_typed_blocks() {
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
        assert_eq!(routes[0].process[0].name(), Some("posts"));
        assert_eq!(routes[0].process[0].kind_id(), "db.find_many");
        assert_eq!(
            routes[0].view.get("page_title").map(|s| s.as_str()),
            Some("x")
        );
    }

    #[test]
    fn load_all_parses_nested_on_missing_block() {
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("posts.json"),
            r#"{
                "path": "/posts/:slug",
                "method": "GET",
                "kind": "page",
                "template": "posts/show.html",
                "process": [
                    {
                        "name": "post",
                        "block": "db.find_one",
                        "table": "posts",
                        "on_missing": {
                            "block": "error",
                            "status": 404,
                            "code": "post_not_found"
                        }
                    }
                ]
            }"#,
        )
        .unwrap();
        let routes = Route::load_all(dir.path()).unwrap();
        assert_eq!(routes[0].process.len(), 1);
        assert_eq!(routes[0].process[0].kind_id(), "db.find_one");
    }

    #[test]
    fn load_all_parses_typed_input_spec() {
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("home.json"),
            r#"{
                "path": "/",
                "method": "GET",
                "kind": "api",
                "input": {
                    "query": { "limit": { "type": "int", "default": 20, "max": 100 } }
                }
            }"#,
        )
        .unwrap();
        let routes = Route::load_all(dir.path()).unwrap();
        let input = routes[0].input.as_ref().expect("input parsed");
        let limit = input.query.get("limit").unwrap();
        assert_eq!(limit.max, Some(100));
    }

    #[test]
    fn load_all_rejects_input_with_invalid_regex() {
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("home.json"),
            r#"{
                "path": "/posts/:slug",
                "method": "GET",
                "kind": "page",
                "template": "posts/show.html",
                "input": {
                    "path": { "slug": { "type": "string", "pattern": "[a-z" } }
                }
            }"#,
        )
        .unwrap();
        let err = Route::load_all(dir.path()).unwrap_err();
        assert!(
            err.message.contains("input.path.slug.pattern")
                && err.message.contains("invalid regex"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn load_all_rejects_invalid_nested_block() {
        let dir = TempDir::new().unwrap();
        let routes_dir = dir.path().join("routes");
        fs::create_dir_all(&routes_dir).unwrap();
        fs::write(
            routes_dir.join("posts.json"),
            r#"{
                "path": "/posts/:slug",
                "method": "GET",
                "kind": "page",
                "template": "posts/show.html",
                "process": [
                    {
                        "name": "post",
                        "block": "db.find_one",
                        "table": "posts",
                        "on_missing": { "block": "error", "status": 999, "code": "x" }
                    }
                ]
            }"#,
        )
        .unwrap();
        let err = Route::load_all(dir.path()).unwrap_err();
        assert!(
            err.message.contains("on_missing") && err.message.contains("4xx/5xx"),
            "got: {}",
            err.message
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
