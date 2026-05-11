//! `db.find_many` — fetch a list of rows from a table.
//!
//! Read-side block. The result is bound to `name` and exposed as
//! `Vec<crate::models::T>` (where `T` is the struct generated from the
//! model whose `table` matches). See `docs/blocks/db.find_many.md`.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;
use serde_json::Value;

use super::{BlockInstance, BlockKind, RawBlock, model_for_table};
use crate::expressions;
use crate::manifest::ManifestError;
use crate::models::Model;

/// Singleton discriminator. Anchors the `block: "db.find_many"` value
/// in the JSON schema so agents see the exact string they must write.
#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub enum Tag {
    #[serde(rename = "db.find_many")]
    Tag,
}

/// On-disk shape of the block.
///
/// `where` / `order_by` / `limit` / `offset` are kept as opaque JSON for now;
/// their full filter-expression grammar lands in the slice that wires real
/// query execution. Validation today is limited to: the `where` value being
/// a CEL expression when written as a string (matches the `guard` block's
/// `if` and `field.validate` syntactic checks).
// `block` is the serde discriminator; `order_by`/`limit`/`offset` are kept
// opaque until slice 5 wires query execution. Rust's dead-code lint can't
// see serde's field reads, so we allow it explicitly on the whole struct.
#[allow(dead_code)]
#[derive(Debug, Deserialize, JsonSchema, Clone)]
#[serde(deny_unknown_fields)]
#[schemars(title = "block: db.find_many")]
pub struct Spec {
    /// Discriminator. Always the literal `"db.find_many"`.
    pub block: Tag,
    /// Binding name. `$<name>` references in `view` / `output` resolve to a
    /// `Vec<crate::models::T>` for the matched model.
    pub name: String,
    /// Target table. Must match an existing model's `table`.
    pub table: String,
    /// Filter expression. Either a CEL string or a structured filter object
    /// (the structured grammar is documented in `docs/blocks/db.find_many.md`).
    #[serde(default, rename = "where")]
    pub r#where: Option<Value>,
    /// Sort directive. Either `"-col"` / `"col"` or an array of those.
    #[serde(default)]
    pub order_by: Option<Value>,
    /// Result cap. Either an integer literal or a `$input.X.X` reference.
    #[serde(default)]
    pub limit: Option<Value>,
    /// Pagination offset, same accepted shapes as `limit`.
    #[serde(default)]
    pub offset: Option<Value>,
}

pub struct Kind;

impl BlockKind for Kind {
    fn id(&self) -> &'static str {
        "db.find_many"
    }

    fn json_schema(&self) -> RootSchema {
        schema_for!(Spec)
    }

    fn parse(&self, raw: &RawBlock) -> Result<Box<dyn BlockInstance>, ManifestError> {
        let spec: Spec =
            serde_json::from_value(raw.as_full_object()).map_err(|e| raw.parse_error(e))?;
        if let Some(Value::String(expr)) = spec.r#where.as_ref() {
            // String-form `where` is a CEL predicate — syntactically
            // validate it now, like the `guard` block's `if` and
            // `field.validate`. The structured object form is accepted as-is.
            expressions::validate(expr, &raw.source, &format!("{}.where", raw.label))?;
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
        "db.find_many"
    }

    fn name(&self) -> Option<&str> {
        Some(&self.spec.name)
    }

    fn output_type(&self, models: &[Model]) -> Option<TokenStream> {
        let model = model_for_table(models, &self.spec.table)?;
        let ident = format_ident!("{}", model.name);
        Some(quote! { Vec<crate::models::#ident> })
    }

    fn embeds_runtime_cel(&self) -> bool {
        matches!(self.spec.r#where.as_ref(), Some(Value::String(_)))
    }

    fn where_predicate(&self) -> Option<&str> {
        match self.spec.r#where.as_ref() {
            Some(Value::String(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    fn target_table(&self) -> Option<&str> {
        Some(&self.spec.table)
    }
}
