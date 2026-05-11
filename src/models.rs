//! Model discovery and parsing for `models/*.json`.
//!
//! Each file under `<project>/models/` declares one entity. The parser
//! produces a fully resolved `Model` that codegen and the migration
//! generator share: fields keep source order, and field-level shorthand
//! (`unique` / `references`) is merged into the table-level `indexes`,
//! `foreign_keys` and `checks` collections at load time. Conflicts between
//! the shorthand and the explicit forms are rejected with a manifest error.

use indexmap::IndexMap;
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::manifest::ManifestError;

/// One declared entity, after parsing + validation.
///
/// `name` is the user-facing struct name as declared in the JSON. Fields are
/// kept in the order they appear in the source file so the generated struct
/// reads the same way the human-authored manifest does. `indexes`,
/// `foreign_keys` and `checks` are the fully resolved table-level
/// collections — field-level shorthand has already been merged in.
#[derive(Debug)]
pub struct Model {
    pub name: String,
    pub table: String,
    pub fields: IndexMap<String, FieldDef>,
    // `indexes`, `foreign_keys` and `checks` are populated and validated here
    // but only consumed by the migration generator (issue #6).
    #[allow(dead_code)]
    pub indexes: Vec<Index>,
    #[allow(dead_code)]
    pub foreign_keys: Vec<ForeignKey>,
    #[allow(dead_code)]
    pub checks: Vec<Check>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FieldDef {
    #[serde(rename = "type")]
    pub ty: FieldType,
    #[serde(default)]
    #[schemars(default)]
    pub nullable: bool,
    /// SQL primary-key flag. The migration generator emits `PRIMARY KEY` on
    /// every field that carries this. Multiple fields with `primary_key`
    /// yield a composite key in the order they appear.
    #[serde(default)]
    #[schemars(default)]
    #[allow(dead_code)]
    pub primary_key: bool,
    /// Field-level shorthand for a single-column unique index. Equivalent to
    /// adding `{ "fields": ["<col>"], "unique": true }` to `indexes`.
    #[serde(default)]
    #[schemars(default)]
    pub unique: bool,
    /// SQL default expression embedded verbatim into the migration. Example:
    /// `"gen_random_uuid()"`, `"now()"`. No escaping is performed.
    #[serde(default)]
    #[schemars(default)]
    #[allow(dead_code)]
    pub default: Option<String>,
    /// `VARCHAR(N)` length hint. Ignored for non-string columns.
    #[serde(default)]
    #[schemars(default)]
    #[allow(dead_code)]
    pub max_length: Option<u32>,
    /// Field-level shorthand for a foreign key. Equivalent to adding a
    /// `{ "field": "<col>", "references": "<Model>.<field>" }` entry to
    /// `foreign_keys`. Accepts the legacy object form too.
    #[serde(default)]
    #[schemars(default)]
    pub references: Option<FieldReference>,
}

/// Logical column types supported in `models/*.json`. Mapped to concrete Rust
/// types in `codegen::model_field_type`.
#[derive(Debug, Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq)]
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

/// Table-level index declaration.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[allow(dead_code)]
pub struct Index {
    /// Columns covered by this index, in order. Must be non-empty.
    pub fields: Vec<String>,
    #[serde(default)]
    #[schemars(default)]
    pub unique: bool,
    /// Optional explicit index name. When omitted, the migration generator
    /// derives one from the table + columns (e.g. `posts_slug_idx`).
    #[serde(default)]
    #[schemars(default)]
    pub name: Option<String>,
}

/// Table-level foreign-key declaration.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[allow(dead_code)]
pub struct ForeignKey {
    /// Local column. Must exist in `fields`.
    pub field: String,
    /// `<Model>.<field>` reference. Resolved against the loaded model set;
    /// unknown references fail at load time.
    pub references: String,
    /// SQL `ON DELETE` action. Defaults to `restrict` to surprise no one.
    #[serde(default)]
    #[schemars(default)]
    pub on_delete: Option<OnDelete>,
}

/// `ON DELETE` actions accepted in declared foreign keys.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OnDelete {
    Restrict,
    Cascade,
    SetNull,
    NoAction,
}

/// Table-level check constraint.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[allow(dead_code)]
pub struct Check {
    /// Optional constraint name. Helps the migration emit `CONSTRAINT <name>
    /// CHECK (...)`, which is easier to alter or drop later.
    #[serde(default)]
    #[schemars(default)]
    pub name: Option<String>,
    /// SQL expression embedded verbatim. The current parser does not validate
    /// it; the database rejects bad expressions at migration apply time.
    pub expr: String,
}

/// The two accepted shapes of field-level `references`:
/// - the structured form `{ "model": "Author", "field": "id", "on_delete": "..." }`
/// - the dotted shorthand `"Author.id"`
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum FieldReference {
    Object {
        model: String,
        field: String,
        #[serde(default)]
        #[schemars(default)]
        on_delete: Option<OnDelete>,
    },
    Dotted(String),
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(title = "rublocks model")]
struct RawModel {
    /// PascalCase ASCII. Becomes the generated Rust struct name.
    name: String,
    /// SQL table name. Consumed by the migration generator.
    table: String,
    /// Ordered map of `column_name` → field definition. Source order is preserved
    /// so the generated struct reads the same way as the JSON.
    fields: IndexMap<String, FieldDef>,
    #[serde(default)]
    #[schemars(default)]
    indexes: Vec<Index>,
    #[serde(default)]
    #[schemars(default)]
    foreign_keys: Vec<ForeignKey>,
    #[serde(default)]
    #[schemars(default)]
    checks: Vec<Check>,
}

impl Model {
    /// Discover and parse every `models/*.json` under `project_dir`.
    pub fn load_all(project_dir: &Path) -> Result<Vec<Model>, ManifestError> {
        let dir = project_dir.join("models");
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut files: Vec<PathBuf> = Vec::new();
        collect_json(&dir, &mut files);
        files.sort();

        let mut models = Vec::with_capacity(files.len());
        for file in &files {
            let content =
                std::fs::read_to_string(file).map_err(|e| ManifestError::read(file, e))?;
            let raw: RawModel =
                serde_json::from_str(&content).map_err(|e| ManifestError::parse(file, e))?;
            validate_struct_name(&raw.name, file)?;
            models.push(resolve_model(raw, file)?);
        }
        validate_cross_references(&models, &files)?;
        Ok(models)
    }
}

/// Merge field-level shorthand into the table-level collections.
///
/// Validation handled here:
/// - `indexes[*].fields` is non-empty and references known columns
/// - `foreign_keys[*].field` references a known column
/// - `foreign_keys[*].references` parses as `<Model>.<field>`
/// - field-level `references` does not conflict with an explicit FK on the same column
/// - field-level `unique: true` does not duplicate an explicit single-column unique index
fn resolve_model(raw: RawModel, source: &Path) -> Result<Model, ManifestError> {
    let mut indexes = raw.indexes;
    let mut foreign_keys = raw.foreign_keys;
    let checks = raw.checks;

    for idx in &indexes {
        if idx.fields.is_empty() {
            return Err(ManifestError::validation(
                source,
                "index `fields` must contain at least one column",
            ));
        }
        for col in &idx.fields {
            if !raw.fields.contains_key(col) {
                return Err(ManifestError::validation(
                    source,
                    format!("index references unknown column `{col}`"),
                ));
            }
        }
    }
    for fk in &foreign_keys {
        if !raw.fields.contains_key(&fk.field) {
            return Err(ManifestError::validation(
                source,
                format!("foreign_keys references unknown column `{}`", fk.field),
            ));
        }
        if !is_dotted_reference(&fk.references) {
            return Err(ManifestError::validation(
                source,
                format!(
                    "foreign_keys.references must be `<Model>.<field>` (got `{}`)",
                    fk.references
                ),
            ));
        }
    }

    let fk_columns: HashSet<String> = foreign_keys.iter().map(|f| f.field.clone()).collect();
    let unique_single_cols: HashSet<String> = indexes
        .iter()
        .filter(|i| i.unique && i.fields.len() == 1)
        .map(|i| i.fields[0].clone())
        .collect();

    for (col, def) in &raw.fields {
        if def.unique {
            if unique_single_cols.contains(col) {
                return Err(ManifestError::validation(
                    source,
                    format!(
                        "column `{col}` declares `unique: true` and is also covered by an explicit unique index — drop one",
                    ),
                ));
            }
            indexes.push(Index {
                fields: vec![col.clone()],
                unique: true,
                name: None,
            });
        }
        if let Some(reference) = &def.references {
            if fk_columns.contains(col) {
                return Err(ManifestError::validation(
                    source,
                    format!(
                        "column `{col}` declares `references` and is also covered by an explicit foreign_keys entry — drop one",
                    ),
                ));
            }
            let (target_model, target_field, on_delete) = match reference {
                FieldReference::Object {
                    model,
                    field,
                    on_delete,
                } => (model.clone(), field.clone(), *on_delete),
                FieldReference::Dotted(dotted) => {
                    let (m, f) = parse_dotted(dotted).ok_or_else(|| {
                        ManifestError::validation(
                            source,
                            format!(
                                "field `{col}` references must be `<Model>.<field>` (got `{dotted}`)",
                            ),
                        )
                    })?;
                    (m, f, None)
                }
            };
            foreign_keys.push(ForeignKey {
                field: col.clone(),
                references: format!("{target_model}.{target_field}"),
                on_delete,
            });
        }
    }

    Ok(Model {
        name: raw.name,
        table: raw.table,
        fields: raw.fields,
        indexes,
        foreign_keys,
        checks,
    })
}

/// Verify every foreign key resolves against the loaded model set.
///
/// Runs after every model in the project has been parsed so that order of
/// files does not matter. Errors carry the file that declared the offending
/// reference, which is what the dev overlay needs to point the user at.
fn validate_cross_references(models: &[Model], files: &[PathBuf]) -> Result<(), ManifestError> {
    for (model, file) in models.iter().zip(files.iter()) {
        for fk in &model.foreign_keys {
            let Some((target_model, target_field)) = parse_dotted(&fk.references) else {
                continue; // already rejected by resolve_model
            };
            let Some(referent) = models.iter().find(|m| m.name == target_model) else {
                return Err(ManifestError::validation(
                    file,
                    format!(
                        "foreign key on column `{}` references unknown model `{}`",
                        fk.field, target_model
                    ),
                ));
            };
            if !referent.fields.contains_key(&target_field) {
                return Err(ManifestError::validation(
                    file,
                    format!(
                        "foreign key on column `{}` references unknown field `{}.{}`",
                        fk.field, target_model, target_field
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn parse_dotted(s: &str) -> Option<(String, String)> {
    let (m, f) = s.split_once('.')?;
    if m.is_empty() || f.is_empty() {
        return None;
    }
    Some((m.to_string(), f.to_string()))
}

fn is_dotted_reference(s: &str) -> bool {
    parse_dotted(s).is_some()
}

/// JSON Schema describing the on-disk shape of `models/*.json`.
///
/// Derived from `RawModel` so the schema is always in sync with what the
/// parser actually accepts. Consumed by the agent installers in `src/agents.rs`.
pub fn json_schema() -> RootSchema {
    schema_for!(RawModel)
}

fn validate_struct_name(name: &str, source: &Path) -> Result<(), ManifestError> {
    let first_ok = name.chars().next().is_some_and(|c| c.is_ascii_uppercase());
    let rest_ok = name.chars().skip(1).all(|c| c.is_ascii_alphanumeric());
    if !(first_ok && rest_ok) {
        return Err(ManifestError::validation(
            source,
            format!("model `name` must be PascalCase ASCII (got `{name}`)"),
        ));
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
    fn shorthand_unique_merges_into_indexes() {
        let dir = TempDir::new().unwrap();
        let models_dir = dir.path().join("models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("a.json"),
            r#"{
                "name": "A",
                "table": "a",
                "fields": {
                    "id":   { "type": "uuid", "primary_key": true },
                    "slug": { "type": "string", "unique": true }
                }
            }"#,
        )
        .unwrap();
        let models = Model::load_all(dir.path()).unwrap();
        let idx = &models[0].indexes;
        assert_eq!(idx.len(), 1);
        assert_eq!(idx[0].fields, vec!["slug".to_string()]);
        assert!(idx[0].unique);
    }

    #[test]
    fn shorthand_unique_conflicting_with_explicit_index_is_rejected() {
        let dir = TempDir::new().unwrap();
        let models_dir = dir.path().join("models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("a.json"),
            r#"{
                "name": "A",
                "table": "a",
                "fields": {
                    "slug": { "type": "string", "unique": true }
                },
                "indexes": [{ "fields": ["slug"], "unique": true }]
            }"#,
        )
        .unwrap();
        let err = Model::load_all(dir.path()).unwrap_err();
        assert!(
            err.message.contains("drop one"),
            "expected conflict error, got: {}",
            err.message
        );
    }

    #[test]
    fn dotted_references_shorthand_resolves_across_files() {
        let dir = TempDir::new().unwrap();
        let models_dir = dir.path().join("models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("author.json"),
            r#"{
                "name": "Author",
                "table": "authors",
                "fields": { "id": { "type": "uuid" } }
            }"#,
        )
        .unwrap();
        fs::write(
            models_dir.join("post.json"),
            r#"{
                "name": "Post",
                "table": "posts",
                "fields": {
                    "id":        { "type": "uuid" },
                    "author_id": { "type": "uuid", "references": "Author.id" }
                }
            }"#,
        )
        .unwrap();
        let models = Model::load_all(dir.path()).unwrap();
        let post = models.iter().find(|m| m.name == "Post").unwrap();
        assert_eq!(post.foreign_keys.len(), 1);
        assert_eq!(post.foreign_keys[0].field, "author_id");
        assert_eq!(post.foreign_keys[0].references, "Author.id");
    }

    #[test]
    fn unknown_foreign_key_target_model_is_rejected() {
        let dir = TempDir::new().unwrap();
        let models_dir = dir.path().join("models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("post.json"),
            r#"{
                "name": "Post",
                "table": "posts",
                "fields": {
                    "author_id": { "type": "uuid", "references": "Author.id" }
                }
            }"#,
        )
        .unwrap();
        let err = Model::load_all(dir.path()).unwrap_err();
        assert!(
            err.message.contains("unknown model"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn unknown_foreign_key_target_field_is_rejected() {
        let dir = TempDir::new().unwrap();
        let models_dir = dir.path().join("models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("author.json"),
            r#"{
                "name": "Author",
                "table": "authors",
                "fields": { "id": { "type": "uuid" } }
            }"#,
        )
        .unwrap();
        fs::write(
            models_dir.join("post.json"),
            r#"{
                "name": "Post",
                "table": "posts",
                "fields": {
                    "author_id": { "type": "uuid", "references": "Author.email" }
                }
            }"#,
        )
        .unwrap();
        let err = Model::load_all(dir.path()).unwrap_err();
        assert!(
            err.message.contains("unknown field"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn object_references_form_is_accepted_with_on_delete() {
        let dir = TempDir::new().unwrap();
        let models_dir = dir.path().join("models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("author.json"),
            r#"{
                "name": "Author",
                "table": "authors",
                "fields": { "id": { "type": "uuid" } }
            }"#,
        )
        .unwrap();
        fs::write(
            models_dir.join("post.json"),
            r#"{
                "name": "Post",
                "table": "posts",
                "fields": {
                    "author_id": {
                        "type": "uuid",
                        "references": { "model": "Author", "field": "id", "on_delete": "cascade" }
                    }
                }
            }"#,
        )
        .unwrap();
        let models = Model::load_all(dir.path()).unwrap();
        let post = models.iter().find(|m| m.name == "Post").unwrap();
        assert_eq!(post.foreign_keys.len(), 1);
        assert_eq!(post.foreign_keys[0].on_delete, Some(OnDelete::Cascade));
    }

    #[test]
    fn checks_and_explicit_indexes_round_trip() {
        let dir = TempDir::new().unwrap();
        let models_dir = dir.path().join("models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("a.json"),
            r#"{
                "name": "A",
                "table": "a",
                "fields": {
                    "title": { "type": "string" },
                    "score": { "type": "int" }
                },
                "indexes": [
                    { "fields": ["title", "score"], "name": "a_title_score_idx" }
                ],
                "checks": [
                    { "name": "score_positive", "expr": "score >= 0" }
                ]
            }"#,
        )
        .unwrap();
        let models = Model::load_all(dir.path()).unwrap();
        let a = &models[0];
        assert_eq!(a.indexes.len(), 1);
        assert_eq!(a.indexes[0].fields, vec!["title", "score"]);
        assert_eq!(a.indexes[0].name.as_deref(), Some("a_title_score_idx"));
        assert_eq!(a.checks.len(), 1);
        assert_eq!(a.checks[0].name.as_deref(), Some("score_positive"));
        assert_eq!(a.checks[0].expr, "score >= 0");
    }

    #[test]
    fn rejects_index_on_unknown_column() {
        let dir = TempDir::new().unwrap();
        let models_dir = dir.path().join("models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(
            models_dir.join("a.json"),
            r#"{
                "name": "A",
                "table": "a",
                "fields": { "id": { "type": "uuid" } },
                "indexes": [{ "fields": ["missing"] }]
            }"#,
        )
        .unwrap();
        let err = Model::load_all(dir.path()).unwrap_err();
        assert!(
            err.message.contains("unknown column"),
            "got: {}",
            err.message
        );
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
        let err = Model::load_all(dir.path()).unwrap_err();
        assert_eq!(err.file, models_dir.join("a.json"));
        assert!(err.message.contains("PascalCase"), "got: {}", err.message);
    }
}
