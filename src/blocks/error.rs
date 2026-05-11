//! `error` — terminate the handler with an HTTP error.
//!
//! Typically nested under another block's `on_missing` to short-circuit the
//! handler with a structured response. See `docs/blocks/error.md`.

use proc_macro2::TokenStream;
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;

use super::{BlockInstance, BlockKind, RawBlock};
use crate::manifest::ManifestError;
use crate::models::Model;

#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub enum Tag {
    #[serde(rename = "error")]
    Tag,
}

#[derive(Debug, Deserialize, JsonSchema, Clone)]
#[serde(deny_unknown_fields)]
#[schemars(title = "block: error")]
pub struct Spec {
    pub block: Tag,
    /// HTTP status code. Must be in the 4xx/5xx range — enforced at load time.
    pub status: u16,
    /// Machine-readable error identifier — surfaces in the JSON body for
    /// `kind: api` routes and in the page error context for `kind: page`.
    pub code: String,
    /// Human-readable description. Optional but strongly recommended.
    #[serde(default)]
    pub description: Option<String>,
}

pub struct Kind;

impl BlockKind for Kind {
    fn id(&self) -> &'static str {
        "error"
    }

    fn json_schema(&self) -> RootSchema {
        schema_for!(Spec)
    }

    fn parse(&self, raw: &RawBlock) -> Result<Box<dyn BlockInstance>, ManifestError> {
        let spec: Spec = serde_json::from_value(raw.as_full_object()).map_err(|e| raw.parse_error(e))?;
        if !(400..=599).contains(&spec.status) {
            return Err(raw.validation_error(format!(
                "error.status must be a 4xx/5xx HTTP status (got {})",
                spec.status
            )));
        }
        if spec.code.trim().is_empty() {
            return Err(raw.validation_error("error.code must not be empty"));
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
        "error"
    }

    fn name(&self) -> Option<&str> {
        None
    }

    /// Terminal block — short-circuits the handler with an HTTP response.
    /// No value bound, so `$<name>` references are not applicable.
    fn output_type(&self, _models: &[Model]) -> Option<TokenStream> {
        None
    }
}
