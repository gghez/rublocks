//! Model discovery and parsing for `models/*.json`.
//!
//! Each file under `<project>/models/` declares one entity (table + fields +
//! optional indexes). Slice 2 consumes only the fields needed to emit a Rust
//! struct in the dist project; SQL-side concerns (`indexes`, `references`,
//! `default`, `unique`) are accepted and ignored until migration generation.

use anyhow::{Context, Result};
use indexmap::IndexMap;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// One declared entity, after parsing + validation.
///
/// `name` is the user-facing struct name as declared in the JSON. Fields are
/// kept in the order they appear in the source file so the generated struct
/// reads the same way the human-authored manifest does.
#[derive(Debug)]
pub struct Model {
    pub name: String,
    #[allow(dead_code)]
    pub table: String,
    pub fields: IndexMap<String, FieldDef>,
}

#[derive(Debug, Deserialize)]
pub struct FieldDef {
    #[serde(rename = "type")]
    pub ty: FieldType,
    #[serde(default)]
    pub nullable: bool,
    /// All other declarative attributes (`primary_key`, `default`, `unique`,
    /// `references`, `max_length`, ...) are accepted but unused in slice 2.
    /// They drive migration generation in a later slice.
    #[serde(flatten)]
    #[allow(dead_code)]
    pub extra: IndexMap<String, serde_json::Value>,
}

/// Logical column types supported in `models/*.json`. Mapped to concrete Rust
/// types in `codegen::model_field_type`.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    Uuid,
    String,
    Text,
    Int,
    Bigint,
    Bool,
    Timestamptz,
    Email,
}

#[derive(Debug, Deserialize)]
struct RawModel {
    name: String,
    table: String,
    fields: IndexMap<String, FieldDef>,
}

impl Model {
    /// Discover and parse every `models/*.json` under `project_dir`.
    pub fn load_all(project_dir: &Path) -> Result<Vec<Model>> {
        let dir = project_dir.join("models");
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut files: Vec<PathBuf> = Vec::new();
        collect_json(&dir, &mut files);
        files.sort();

        let mut models = Vec::with_capacity(files.len());
        for file in &files {
            let content = std::fs::read_to_string(file)
                .with_context(|| format!("failed to read {}", file.display()))?;
            let raw: RawModel = serde_json::from_str(&content)
                .with_context(|| format!("failed to parse {}", file.display()))?;
            validate_struct_name(&raw.name, file)?;
            models.push(Model {
                name: raw.name,
                table: raw.table,
                fields: raw.fields,
            });
        }
        Ok(models)
    }
}

fn validate_struct_name(name: &str, source: &Path) -> Result<()> {
    let first_ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase());
    let rest_ok = name.chars().skip(1).all(|c| c.is_ascii_alphanumeric());
    anyhow::ensure!(
        first_ok && rest_ok,
        "{}: model `name` must be PascalCase ASCII (got `{}`)",
        source.display(),
        name
    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn load_all_returns_empty_when_no_models_dir() {
        let dir = TempDir::new().unwrap();
        let models = Model::load_all(dir.path()).unwrap();
        assert!(models.is_empty());
    }

    #[test]
    fn load_all_preserves_field_order() {
        let dir = TempDir::new().unwrap();
        let models_dir = dir.path().join("models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("post.json"),
            r#"{
                "name": "Post",
                "table": "posts",
                "fields": {
                    "id":           { "type": "uuid" },
                    "slug":         { "type": "string" },
                    "title":        { "type": "string" },
                    "published_at": { "type": "timestamptz", "nullable": true }
                }
            }"#,
        )
        .unwrap();
        let models = Model::load_all(dir.path()).unwrap();
        assert_eq!(models.len(), 1);
        let fields: Vec<&String> = models[0].fields.keys().collect();
        assert_eq!(fields, vec!["id", "slug", "title", "published_at"]);
    }

    #[test]
    fn nullable_marks_field_as_optional() {
        let dir = TempDir::new().unwrap();
        let models_dir = dir.path().join("models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("a.json"),
            r#"{
                "name": "A",
                "table": "a",
                "fields": {
                    "x": { "type": "string", "nullable": true },
                    "y": { "type": "string" }
                }
            }"#,
        )
        .unwrap();
        let models = Model::load_all(dir.path()).unwrap();
        assert!(models[0].fields["x"].nullable);
        assert!(!models[0].fields["y"].nullable);
    }

    #[test]
    fn extra_attributes_are_accepted_silently() {
        let dir = TempDir::new().unwrap();
        let models_dir = dir.path().join("models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("a.json"),
            r#"{
                "name": "A",
                "table": "a",
                "fields": {
                    "id": {
                        "type": "uuid",
                        "primary_key": true,
                        "default": "gen_random_uuid()"
                    }
                },
                "indexes": [{ "fields": ["id"] }]
            }"#,
        )
        .unwrap();
        let models = Model::load_all(dir.path()).unwrap();
        assert_eq!(models[0].name, "A");
    }

    #[test]
    fn rejects_lowercase_name() {
        let dir = TempDir::new().unwrap();
        let models_dir = dir.path().join("models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("a.json"),
            r#"{ "name": "post", "table": "posts", "fields": {} }"#,
        )
        .unwrap();
        let err = Model::load_all(dir.path()).unwrap_err().to_string();
        assert!(err.contains("PascalCase"), "got: {err}");
    }
}
