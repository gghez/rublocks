//! Aggregator for the JSON Schemas of the rublocks declarative surface.
//!
//! The three schemas (`main.json`, `models/*.json`, `routes/*.json`) are
//! derived from the parsing types via `schemars`. They are invariant for a
//! given rublocks binary version — there is no per-project copy. Consumers
//! embed them into the agent artifacts (Claude skill, AGENTS.md, Cursor
//! rules) written under each project at `build` time.

use schemars::schema::RootSchema;

use crate::{manifest, models, routes};

/// One schema entry: a stable identifier and the rendered JSON content.
pub struct Schema {
    /// Stable id (e.g. `manifest`, `model`, `route`). Currently used for test
    /// assertions; future tooling will reach for it as a filename key.
    #[allow(dead_code)]
    pub id: &'static str,
    /// Short human-facing title shown alongside the schema in agent artifacts.
    pub title: &'static str,
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
pub fn all() -> Vec<Schema> {
    vec![
        Schema {
            id: "manifest",
            title: "main.json",
            root: manifest::json_schema(),
        },
        Schema {
            id: "model",
            title: "models/*.json",
            root: models::json_schema(),
        },
        Schema {
            id: "route",
            title: "routes/*.json",
            root: routes::json_schema(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_returns_three_schemas_in_stable_order() {
        let schemas = all();
        let ids: Vec<&str> = schemas.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["manifest", "model", "route"]);
    }

    #[test]
    fn manifest_schema_lists_name_and_services() {
        let schemas = all();
        let manifest = &schemas[0];
        let json = manifest.pretty_json();
        assert!(json.contains("\"name\""), "schema must declare name: {json}");
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
        for ty in ["uuid", "string", "text", "int", "bigint", "bool", "timestamptz", "email"] {
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
