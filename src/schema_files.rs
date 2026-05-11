//! Project-side JSON Schema emission for editor autocomplete.
//!
//! Companion to [`crate::schema`] (which feeds the agent integration files).
//! The schemas live under `<project>/.rublocks/schemas/` so editors can pick
//! them up via the standard `$schema` URL mechanism wired up in
//! `<project>/.vscode/settings.json`. The directory is committed to git in
//! the playground so users see autocomplete working out of the box.
//!
//! Why not a `dist/`-relative path: `dist/` is regenerated from scratch on
//! every build (and gitignored), so it cannot host paths editors expect to
//! be stable across IDE sessions. The `.rublocks/` namespace next to
//! `main.json` is the only place that has both properties.

use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::fs;
use std::path::Path;

use crate::manifest::write_text_utf8;
use crate::schema::{self, Schema};

/// Subdirectory holding the rendered schemas inside each rublocks project.
const SCHEMAS_DIR: &str = ".rublocks/schemas";

/// Write every schema to disk under `<project>/<SCHEMAS_DIR>/` and refresh
/// `<project>/.vscode/settings.json` with the matching `json.schemas`
/// mapping. Idempotent across runs — the directory is wiped clean each pass
/// so a removed block doesn't leave a stale `<id>.schema.json` behind.
pub fn write_all(project_dir: &Path) -> Result<()> {
    let schemas_root = project_dir.join(SCHEMAS_DIR);
    if schemas_root.exists() {
        fs::remove_dir_all(&schemas_root)
            .with_context(|| format!("failed to wipe {}", schemas_root.display()))?;
    }
    fs::create_dir_all(&schemas_root)
        .with_context(|| format!("failed to create {}", schemas_root.display()))?;

    for s in schema::all() {
        let rel = relative_filename(&s);
        let abs = schemas_root.join(&rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        write_text_utf8(&abs, &s.pretty_json())
            .with_context(|| format!("failed to write {}", abs.display()))?;
    }

    write_vscode_settings(project_dir)?;
    Ok(())
}

/// Map a [`Schema`] id to its on-disk filename, relative to the schemas
/// root. Top-level shapes get the canonical `<surface>.schema.json` name;
/// each block lands under `blocks/<kind>.schema.json` so editors can match
/// per-block surfaces in the future (today the route schema already
/// inlines block discriminators).
pub(crate) fn relative_filename(s: &Schema) -> String {
    match s.id.as_str() {
        "manifest" => "main.schema.json".to_string(),
        "model" => "model.schema.json".to_string(),
        "route" => "route.schema.json".to_string(),
        "layout" => "layout.schema.json".to_string(),
        "input" => "input.schema.json".to_string(),
        other if other.starts_with("block:") => {
            let kind = &other["block:".len()..];
            format!("blocks/{kind}.schema.json")
        }
        other => format!("{other}.schema.json"),
    }
}

/// File-match patterns paired with their schema file. Order is significant
/// only for the JSON we emit (stable across runs); the editor evaluates
/// every entry. Routes use `**/*.json` because the routes/ subtree allows
/// arbitrary nesting (see [`crate::routes::Route::load_all`]).
fn vscode_mappings() -> Vec<(Vec<String>, String)> {
    vec![
        (
            vec!["main.json".to_string()],
            "./.rublocks/schemas/main.schema.json".to_string(),
        ),
        (
            vec!["models/*.json".to_string()],
            "./.rublocks/schemas/model.schema.json".to_string(),
        ),
        (
            vec!["routes/**/*.json".to_string()],
            "./.rublocks/schemas/route.schema.json".to_string(),
        ),
        (
            vec!["layouts/*.json".to_string()],
            "./.rublocks/schemas/layout.schema.json".to_string(),
        ),
    ]
}

/// Refresh `<project>/.vscode/settings.json` so the user's editor wires
/// each rublocks JSON file to its schema. Preserves every unrelated key
/// in an existing settings file — the agent installers and codegen are
/// otherwise the only writers of files under `<project>/`, and `.vscode/`
/// is canonically user-owned.
fn write_vscode_settings(project_dir: &Path) -> Result<()> {
    let dir = project_dir.join(".vscode");
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let path = dir.join("settings.json");
    let existing_raw = match fs::read_to_string(&path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).context(format!("failed to read {}", path.display())),
    };
    let merged = merge_vscode_settings(existing_raw.as_deref())
        .with_context(|| format!("failed to merge existing {}", path.display()))?;
    write_text_utf8(&path, &merged)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Pure transform: take an existing `settings.json` body (or `None`) and
/// return the new content with `json.schemas` updated. Surrounding keys
/// stay in their original order — `serde_json` is built with
/// `preserve_order` (see `Cargo.toml`).
pub(crate) fn merge_vscode_settings(existing: Option<&str>) -> Result<String> {
    let mut root: Map<String, Value> = match existing {
        Some(raw) if !raw.trim().is_empty() => {
            serde_json::from_str(raw).context("existing .vscode/settings.json is not valid JSON")?
        }
        _ => Map::new(),
    };
    let mut entries: Vec<Value> = Vec::new();
    for (file_match, url) in vscode_mappings() {
        let mut entry = Map::new();
        entry.insert(
            "fileMatch".to_string(),
            Value::Array(file_match.into_iter().map(Value::String).collect()),
        );
        entry.insert("url".to_string(), Value::String(url));
        entries.push(Value::Object(entry));
    }
    root.insert("json.schemas".to_string(), Value::Array(entries));
    let mut out = serde_json::to_string_pretty(&Value::Object(root))
        .expect("serde_json::Map always serializes");
    out.push('\n');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn relative_filename_maps_top_level_shapes() {
        let cases = [
            ("manifest", "main.schema.json"),
            ("model", "model.schema.json"),
            ("route", "route.schema.json"),
            ("layout", "layout.schema.json"),
            ("input", "input.schema.json"),
        ];
        for (id, expected) in cases {
            let s = Schema {
                id: id.to_string(),
                title: id.to_string(),
                root: Default::default(),
            };
            assert_eq!(relative_filename(&s), expected);
        }
    }

    #[test]
    fn relative_filename_puts_blocks_under_blocks_subdir() {
        let s = Schema {
            id: "block:db.find_many".to_string(),
            title: "db.find_many".to_string(),
            root: Default::default(),
        };
        assert_eq!(relative_filename(&s), "blocks/db.find_many.schema.json");
    }

    #[test]
    fn write_all_emits_one_file_per_schema_under_rublocks_schemas() {
        let dir = TempDir::new().unwrap();
        write_all(dir.path()).unwrap();
        for s in schema::all() {
            let p = dir.path().join(SCHEMAS_DIR).join(relative_filename(&s));
            assert!(
                p.is_file(),
                "missing schema file for `{}` at {}",
                s.id,
                p.display()
            );
            let body = fs::read_to_string(&p).unwrap();
            // Each emitted file must round-trip through serde_json so the
            // editor side does not have to tolerate malformed schemas.
            let _: Value = serde_json::from_str(&body).expect("schema is valid JSON");
        }
    }

    #[test]
    fn write_all_wipes_stale_schema_files_from_previous_runs() {
        // A renamed or removed block must not leave a phantom schema file
        // on disk — the directory is fully owned by `write_all`.
        let dir = TempDir::new().unwrap();
        let stale = dir
            .path()
            .join(SCHEMAS_DIR)
            .join("blocks")
            .join("ghost.schema.json");
        fs::create_dir_all(stale.parent().unwrap()).unwrap();
        fs::write(&stale, "{}").unwrap();
        write_all(dir.path()).unwrap();
        assert!(
            !stale.exists(),
            "stale schema file not wiped: {}",
            stale.display()
        );
    }

    #[test]
    fn write_all_creates_vscode_settings_with_json_schemas_array() {
        let dir = TempDir::new().unwrap();
        write_all(dir.path()).unwrap();
        let raw = fs::read_to_string(dir.path().join(".vscode/settings.json")).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        let entries = v
            .get("json.schemas")
            .and_then(|x| x.as_array())
            .expect("json.schemas array");
        let urls: Vec<&str> = entries
            .iter()
            .filter_map(|e| e.get("url").and_then(|u| u.as_str()))
            .collect();
        assert!(urls.contains(&"./.rublocks/schemas/main.schema.json"));
        assert!(urls.contains(&"./.rublocks/schemas/model.schema.json"));
        assert!(urls.contains(&"./.rublocks/schemas/route.schema.json"));
        assert!(urls.contains(&"./.rublocks/schemas/layout.schema.json"));
    }

    #[test]
    fn merge_vscode_settings_preserves_unrelated_keys() {
        let existing = r#"{
          "editor.tabSize": 2,
          "rust-analyzer.checkOnSave": true
        }"#;
        let out = merge_vscode_settings(Some(existing)).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v.get("editor.tabSize").and_then(|x| x.as_i64()), Some(2));
        assert_eq!(
            v.get("rust-analyzer.checkOnSave").and_then(|x| x.as_bool()),
            Some(true)
        );
        assert!(v.get("json.schemas").is_some());
    }

    #[test]
    fn merge_vscode_settings_replaces_existing_json_schemas_block() {
        // Older versions of rublocks may have written a different mapping.
        // The merge must overwrite the array wholesale so renamed schema
        // files don't leave dangling references.
        let existing = r#"{
          "json.schemas": [
            { "fileMatch": ["old.json"], "url": "./old.schema.json" }
          ]
        }"#;
        let out = merge_vscode_settings(Some(existing)).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let entries = v.get("json.schemas").and_then(|x| x.as_array()).unwrap();
        let urls: Vec<&str> = entries
            .iter()
            .filter_map(|e| e.get("url").and_then(|u| u.as_str()))
            .collect();
        assert!(!urls.contains(&"./old.schema.json"));
        assert!(urls.contains(&"./.rublocks/schemas/main.schema.json"));
    }

    #[test]
    fn merge_vscode_settings_creates_object_when_no_file_exists() {
        let out = merge_vscode_settings(None).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("json.schemas").is_some());
    }

    #[test]
    fn merge_vscode_settings_rejects_invalid_existing_json() {
        let err = merge_vscode_settings(Some("not json")).unwrap_err();
        assert!(err.to_string().contains("valid JSON"));
    }
}
