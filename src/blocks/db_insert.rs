//! `db.insert` — insert a single row into a table.
//!
//! Write-side block. Does not bind a value: `$<name>` references against an
//! insert block are rejected at load time. See `docs/blocks/db.insert.md`.

use indexmap::IndexMap;
use proc_macro2::TokenStream;
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;
use serde_json::Value;

use super::{BlockInstance, BlockKind, RawBlock};
use crate::manifest::ManifestError;
use crate::models::Model;

#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub enum Tag {
    #[serde(rename = "db.insert")]
    Tag,
}

#[derive(Debug, Deserialize, JsonSchema, Clone)]
#[serde(deny_unknown_fields)]
#[schemars(title = "block: db.insert")]
pub struct Spec {
    pub block: Tag,
    /// Target table. Must match an existing model's `table`.
    pub table: String,
    /// Column → value map. Each value is either a literal or a `$input.X.X`
    /// / `$<prior_block>.<field>` reference.
    pub values: IndexMap<String, Value>,
    /// Optional binding name for a future return-affected-row mode. Not yet
    /// referenceable from `view` / `output`.
    #[serde(default)]
    pub name: Option<String>,
}

pub struct Kind;

impl BlockKind for Kind {
    fn id(&self) -> &'static str {
        "db.insert"
    }

    fn json_schema(&self) -> RootSchema {
        schema_for!(Spec)
    }

    fn parse(&self, raw: &RawBlock) -> Result<Box<dyn BlockInstance>, ManifestError> {
        let spec: Spec = serde_json::from_value(raw.as_full_object()).map_err(|e| raw.parse_error(e))?;
        if spec.values.is_empty() {
            return Err(raw.validation_error("db.insert requires at least one entry in `values`"));
        }
        Ok(Box::new(Instance { spec }))
    }
}

#[derive(Debug)]
pub struct Instance {
    pub spec: Spec,
}

impl BlockInstance for Instance {
    fn kind_id(&self) -> &'static str {
        "db.insert"
    }

    fn name(&self) -> Option<&str> {
        self.spec.name.as_deref()
    }

    /// Write-side: nothing bound to `$<name>`. Returning `None` makes the
    /// view-type inference fall back to `String`, but the load-time check
    /// in `routes.rs` already forbids `$<name>` references against an
    /// insert block.
    fn output_type(&self, _models: &[Model]) -> Option<TokenStream> {
        None
    }
}
