//! Aggregator for the JSON Schemas of the rublocks declarative surface.
//!
//! The three schemas (`main.json`, `models/*.json`, `routes/*.json`) are
//! derived from the parsing types via `schemars`. They are invariant for a
//! given rublocks binary version — there is no per-project copy. Consumers
//! embed them into the agent artifacts (Claude skill, AGENTS.md, Cursor
//! rules) written under each project at `build` time.

use schemars::schema::RootSchema;

use crate::{blocks, input, layouts, manifest, models, routes};

/// One schema entry: a stable identifier and the rendered JSON content.
pub struct Schema {
    /// Stable id (e.g. `manifest`, `block:db.find_many`). Currently used for
    /// test assertions; future tooling will reach for it as a filename key.
    #[allow(dead_code)]
    pub id: String,
    /// Short human-facing title shown alongside the schema in agent artifacts.
    pub title: String,
    /// The schemars-derived root schema.
    pub root: RootSchema,
}

impl Schema {
    /// Pretty-printed JSON of the schema, suitable for embedding into a
    /// markdown code block.
    pub fn pretty_json(&self) -> String {
        serde_json::to_string_pretty(&self.root).expect("RootSchema is always serializable")
    }
}

/// The full set of schemas exposed by this binary, in stable order.
///
/// Order: top-level shapes first (manifest → model → route → layout),
/// then one entry per registered block. The per-block entries make the
/// JSON contract of each `process[*]` discoverable to agents without them
/// having to read source code.
pub fn all() -> Vec<Schema> {
    let mut out = vec![
        Schema {
            id: "manifest".to_string(),
            title: "main.json".to_string(),
            root: manifest::json_schema(),
        },
        Schema {
            id: "model".to_string(),
            title: "models/*.json".to_string(),
            root: models::json_schema(),
        },
        Schema {
            id: "route".to_string(),
            title: "routes/*.json".to_string(),
            root: routes::json_schema(),
        },
        Schema {
            id: "layout".to_string(),
            title: "layouts/*.json".to_string(),
            root: layouts::json_schema(),
        },
        Schema {
            id: "input".to_string(),
            title: "route.input".to_string(),
            root: input::json_schema(),
        },
    ];
    for kind in blocks::registry().kinds() {
        out.push(Schema {
            id: format!("block:{}", kind.id()),
            title: format!("block: {}", kind.id()),
            root: kind.json_schema(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_starts_with_top_level_shapes_then_blocks() {
        let schemas = all();
        let ids: Vec<&str> = schemas.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.starts_with(&["manifest", "model", "route", "layout"]));
    }

    #[test]
    fn all_emits_one_schema_per_registered_block() {
        let schemas = all();
        for kind in crate::blocks::registry().kinds() {
            let expected = format!("block:{}", kind.id());
            assert!(
                schemas.iter().any(|s| s.id == expected),
                "missing schema entry for block `{}`",
                kind.id()
            );
        }
    }

    #[test]
    fn manifest_schema_lists_name_and_services() {
        let schemas = all();
        let manifest = &schemas[0];
        let json = manifest.pretty_json();
        assert!(
            json.contains("\"name\""),
            "schema must declare name: {json}"
        );
        assert!(
            json.contains("\"services\""),
            "schema must declare services: {json}"
        );
    }

    #[test]
    fn model_schema_lists_field_types_enum() {
        let schemas = all();
        let model = &schemas[1];
        let json = model.pretty_json();
        for ty in [
            "uuid",
            "string",
            "text",
            "int",
            "bigint",
            "bool",
            "timestamptz",
            "email",
        ] {
            assert!(
                json.contains(&format!("\"{ty}\"")),
                "model schema must list field type `{ty}`: {json}"
            );
        }
    }

    #[test]
    fn route_schema_lists_methods_and_kinds() {
        let schemas = all();
        let route = &schemas[2];
        let json = route.pretty_json();
        for m in ["GET", "POST", "PUT", "DELETE", "PATCH"] {
            assert!(
                json.contains(&format!("\"{m}\"")),
                "route schema must list method `{m}`: {json}"
            );
        }
        for k in ["page", "api"] {
            assert!(
                json.contains(&format!("\"{k}\"")),
                "route schema must list kind `{k}`: {json}"
            );
        }
    }

    #[test]
    fn service_url_is_modeled_as_string() {
        let schemas = all();
        let manifest = &schemas[0];
        let json = manifest.pretty_json();
        // ServiceUrl carries `#[schemars(with = "String")]` — the schema must
        // not leak the Rust enum representation.
        assert!(
            !json.contains("\"Literal\"") && !json.contains("\"Env\""),
            "ServiceUrl must be opaque-string in the schema, got: {json}"
        );
    }
}
