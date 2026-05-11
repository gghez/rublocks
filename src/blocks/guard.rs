//! `guard` — authorize the current request against a CEL predicate.
//!
//! Composes inside `process` like any other block: the scope of names the
//! `if` expression can reference is exactly what has been bound by prior
//! blocks plus the route input. Place a guard at the top of `process` for
//! an early check (`user.is_admin`), or after a `db.find_*` that loads the
//! row whose ownership you want to assert (`post.author_id == user.id`).
//!
//! Runtime semantics (slice 5): when `if` evaluates to `false`, the
//! handler short-circuits with `403 Forbidden`. The codegen surface today
//! emits a stub like every other block.
//!
//! See `docs/blocks/guard.md`.

use proc_macro2::TokenStream;
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;

use super::{BlockInstance, BlockKind, RawBlock};
use crate::expressions;
use crate::manifest::ManifestError;
use crate::models::Model;

/// Singleton discriminator. Anchors `block: "guard"` in the JSON schema
/// so agents see the exact string they must write.
#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub enum Tag {
    #[serde(rename = "guard")]
    Tag,
}

/// On-disk shape of the block.
///
/// `block` is the serde discriminator — read by deserialization, not by
/// Rust code, hence the lint allow.
#[allow(dead_code)]
#[derive(Debug, Deserialize, JsonSchema, Clone)]
#[serde(deny_unknown_fields)]
#[schemars(title = "block: guard")]
pub struct Spec {
    /// Discriminator. Always the literal `"guard"`.
    pub block: Tag,
    /// CEL predicate. Evaluated against the current scope; `false` ⇒ 403.
    #[serde(rename = "if")]
    pub r#if: String,
}

pub struct Kind;

impl BlockKind for Kind {
    fn id(&self) -> &'static str {
        "guard"
    }

    fn json_schema(&self) -> RootSchema {
        schema_for!(Spec)
    }

    fn parse(&self, raw: &RawBlock) -> Result<Box<dyn BlockInstance>, ManifestError> {
        let spec: Spec =
            serde_json::from_value(raw.as_full_object()).map_err(|e| raw.parse_error(e))?;
        expressions::validate(&spec.r#if, &raw.source, &format!("{}.if", raw.label))?;
        Ok(Box::new(Instance { spec }))
    }
}

#[derive(Debug)]
pub struct Instance {
    pub spec: Spec,
}

impl BlockInstance for Instance {
    fn kind_id(&self) -> &'static str {
        "guard"
    }

    /// Authorization block — binds nothing, so `$<name>` references against
    /// it are not applicable.
    fn name(&self) -> Option<&str> {
        None
    }

    fn output_type(&self, _models: &[Model]) -> Option<TokenStream> {
        None
    }

    fn embeds_runtime_cel(&self) -> bool {
        true
    }

    fn guard_if(&self) -> Option<&str> {
        Some(&self.spec.r#if)
    }
}
