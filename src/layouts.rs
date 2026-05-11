//! Layout discovery and parsing for `layouts/*.json`.
//!
//! A layout is a named wrapper template that pages opt into via
//! `route.layout: "<name>"`. Slice 3 consumes only the fields needed to
//! wire Askama's `{% extends %}` and project the layout's required
//! variables onto the page context — `process` and `view` are accepted but
//! their execution is deferred to slice 5. See `docs/layouts.md`.

use indexmap::IndexMap;
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::manifest::ManifestError;
use crate::routes::ProcessBlock;

/// One declared layout, after parsing + validation.
#[derive(Debug)]
pub struct Layout {
    pub name: String,
    /// Template file the layout points to. Slice 3 doesn't read it directly
    /// (Askama resolves `{% extends "..." %}` inside the user's templates),
    /// but later slices use it for cross-validation.
    #[allow(dead_code)]
    pub template: String,
    /// Variables the layout expects the calling page to provide. They become
    /// fields on the generated page context struct so Askama inheritance
    /// finds them at render time.
    pub requires: IndexMap<String, LayoutRequire>,
    /// View bindings exposed by the layout itself. Each becomes a field on
    /// the page context — slice 3 always fills them with the type's default.
    pub view: IndexMap<String, String>,
    #[allow(dead_code)]
    pub process: Vec<ProcessBlock>,
}

#[derive(Debug)]
pub struct LayoutRequire {
    pub ty: RequireType,
}

/// Subset of model field types currently allowed in `layout.requires`.
/// Kept small on purpose; the schema can grow when real use cases appear.
#[derive(Debug, Deserialize, JsonSchema, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum RequireType {
    String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(title = "rublocks layout")]
struct RawLayout {
    /// Layout name. Routes reference this via `layout: "<name>"`.
    name: String,
    /// HTML template under `templates/`. Same lookup rules as page templates.
    template: String,
    #[serde(default)]
    #[schemars(default)]
    requires: IndexMap<String, RawRequire>,
    #[serde(default)]
    #[schemars(default)]
    process: Vec<ProcessBlock>,
    #[serde(default)]
    #[schemars(default)]
    view: IndexMap<String, String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RawRequire {
    #[serde(rename = "type")]
    ty: RequireType,
}

impl Layout {
    /// Discover and parse every `layouts/*.json` under `project_dir`.
    pub fn load_all(project_dir: &Path) -> Result<Vec<Layout>, ManifestError> {
        let dir = project_dir.join("layouts");
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut files: Vec<PathBuf> = Vec::new();
        collect_json(&dir, &mut files);
        files.sort();

        let mut layouts = Vec::with_capacity(files.len());
        let mut seen: HashSet<String> = HashSet::new();
        for file in &files {
            let content =
                std::fs::read_to_string(file).map_err(|e| ManifestError::read(file, e))?;
            let raw: RawLayout =
                serde_json::from_str(&content).map_err(|e| ManifestError::parse(file, e))?;
            if raw.name.is_empty() {
                return Err(ManifestError::validation(
                    file,
                    "layout `name` must not be empty",
                ));
            }
            if raw.template.is_empty() {
                return Err(ManifestError::validation(
                    file,
                    "layout `template` must not be empty",
                ));
            }
            if !seen.insert(raw.name.clone()) {
                return Err(ManifestError::validation(
                    file,
                    format!("duplicate layout name `{}`", raw.name),
                ));
            }
            let requires = raw
                .requires
                .into_iter()
                .map(|(k, r)| (k, LayoutRequire { ty: r.ty }))
                .collect();
            layouts.push(Layout {
                name: raw.name,
                template: raw.template,
                requires,
                view: raw.view,
                process: raw.process,
            });
        }
        Ok(layouts)
    }

    /// Look up a layout by name. Used by codegen to resolve `route.layout`
    /// against the loaded layout set; returns `None` so the caller can emit
    /// a friendly manifest error pointing at the offending route file.
    pub fn find<'a>(layouts: &'a [Layout], name: &str) -> Option<&'a Layout> {
        layouts.iter().find(|l| l.name == name)
    }
}

/// JSON Schema describing the on-disk shape of `layouts/*.json`. Consumed by
/// the agent installers in `src/agents.rs`.
pub fn json_schema() -> RootSchema {
    schema_for!(RawLayout)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn load_all_returns_empty_when_no_layouts_dir() {
        let dir = TempDir::new().unwrap();
        let layouts = Layout::load_all(dir.path()).unwrap();
        assert!(layouts.is_empty());
    }

    #[test]
    fn load_all_parses_minimal_layout() {
        let dir = TempDir::new().unwrap();
        let layouts_dir = dir.path().join("layouts");
        fs::create_dir_all(&layouts_dir).unwrap();
        fs::write(
            layouts_dir.join("main.json"),
            r#"{
                "name": "main",
                "template": "layout.html",
                "requires": { "page_title": { "type": "string" } },
                "view": { "current_year": "$year" }
            }"#,
        )
        .unwrap();
        let layouts = Layout::load_all(dir.path()).unwrap();
        assert_eq!(layouts.len(), 1);
        assert_eq!(layouts[0].name, "main");
        assert_eq!(layouts[0].template, "layout.html");
        assert!(layouts[0].requires.contains_key("page_title"));
        assert_eq!(
            layouts[0].view.get("current_year").map(|s| s.as_str()),
            Some("$year")
        );
    }

    #[test]
    fn load_all_rejects_missing_name() {
        let dir = TempDir::new().unwrap();
        let layouts_dir = dir.path().join("layouts");
        fs::create_dir_all(&layouts_dir).unwrap();
        fs::write(
            layouts_dir.join("a.json"),
            r#"{ "name": "", "template": "x.html" }"#,
        )
        .unwrap();
        let err = Layout::load_all(dir.path()).unwrap_err();
        assert_eq!(err.file, layouts_dir.join("a.json"));
        assert!(err.message.contains("name"), "got: {}", err.message);
    }

    #[test]
    fn load_all_rejects_missing_template() {
        let dir = TempDir::new().unwrap();
        let layouts_dir = dir.path().join("layouts");
        fs::create_dir_all(&layouts_dir).unwrap();
        fs::write(
            layouts_dir.join("a.json"),
            r#"{ "name": "main", "template": "" }"#,
        )
        .unwrap();
        let err = Layout::load_all(dir.path()).unwrap_err();
        assert_eq!(err.file, layouts_dir.join("a.json"));
        assert!(err.message.contains("template"), "got: {}", err.message);
    }

    #[test]
    fn load_all_rejects_duplicate_names() {
        let dir = TempDir::new().unwrap();
        let layouts_dir = dir.path().join("layouts");
        fs::create_dir_all(&layouts_dir).unwrap();
        fs::write(
            layouts_dir.join("a.json"),
            r#"{ "name": "main", "template": "a.html" }"#,
        )
        .unwrap();
        fs::write(
            layouts_dir.join("b.json"),
            r#"{ "name": "main", "template": "b.html" }"#,
        )
        .unwrap();
        let err = Layout::load_all(dir.path()).unwrap_err();
        assert!(err.message.contains("duplicate"), "got: {}", err.message);
    }

    #[test]
    fn find_returns_layout_by_name() {
        let layouts = vec![Layout {
            name: "main".to_string(),
            template: "layout.html".to_string(),
            requires: IndexMap::new(),
            view: IndexMap::new(),
            process: Vec::new(),
        }];
        assert!(Layout::find(&layouts, "main").is_some());
        assert!(Layout::find(&layouts, "absent").is_none());
    }
}
