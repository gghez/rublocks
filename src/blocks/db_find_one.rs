//! `db.find_one` — fetch a single row from a table.
//!
//! Read-side block. `$<name>` resolves to `crate::models::T`.
//! `on_missing` is itself a sub-block: when the lookup returns no row, the
//! handler executes the nested block (typically `error`) and short-circuits.
//! See `docs/blocks/db.find_one.md`.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;
use serde_json::Value;

use super::{BlockInstance, BlockKind, RawBlock, model_for_table};
use crate::expressions;
use crate::manifest::ManifestError;
use crate::models::Model;

#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub enum Tag {
    #[serde(rename = "db.find_one")]
    Tag,
}

#[derive(Debug, Deserialize, JsonSchema, Clone)]
#[serde(deny_unknown_fields)]
#[schemars(title = "block: db.find_one")]
pub struct Spec {
    pub block: Tag,
    /// Binding name. `$<name>` resolves to `crate::models::T`.
    pub name: String,
    pub table: String,
    /// Filter expression. CEL string or structured object.
    #[serde(default, rename = "where")]
    pub r#where: Option<Value>,
    /// Block executed when the lookup returns no row. Parsed recursively
    /// against the registry — typically points at `error`.
    #[serde(default)]
    pub on_missing: Option<Value>,
}

pub struct Kind;

impl BlockKind for Kind {
    fn id(&self) -> &'static str {
        "db.find_one"
    }

    fn json_schema(&self) -> RootSchema {
        schema_for!(Spec)
    }

    fn parse(&self, raw: &RawBlock) -> Result<Box<dyn BlockInstance>, ManifestError> {
        let spec: Spec = serde_json::from_value(raw.as_full_object()).map_err(|e| raw.parse_error(e))?;
        if let Some(Value::String(expr)) = spec.r#where.as_ref() {
            expressions::validate(
                expr,
                &raw.source,
                &format!("{}.where", raw.label),
            )?;
        }
        let on_missing = match spec.on_missing.as_ref() {
            Some(v) => {
                let nested = RawBlock::from_value(
                    v,
                    &raw.source,
                    &format!("{}.on_missing", raw.label),
                )?;
                Some(super::parse(&nested)?)
            }
            None => None,
        };
        Ok(Box::new(Instance { spec, on_missing }))
    }
}

#[derive(Debug)]
pub struct Instance {
    pub spec: Spec,
    /// Parsed sub-block. Kept owned so codegen can call it without re-parsing
    /// the original `Value` — and so deeper nesting (an error block with its
    /// own sub-blocks one day) Just Works.
    pub on_missing: Option<Box<dyn BlockInstance>>,
}

impl BlockInstance for Instance {
    fn kind_id(&self) -> &'static str {
        "db.find_one"
    }

    fn name(&self) -> Option<&str> {
        Some(&self.spec.name)
    }

    fn output_type(&self, models: &[Model]) -> Option<TokenStream> {
        let model = model_for_table(models, &self.spec.table)?;
        let ident = format_ident!("{}", model.name);
        Some(quote! { crate::models::#ident })
    }
}
