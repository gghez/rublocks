//! `time.now` — bind the current wall-clock time to `$<name>`, optionally
//! formatted via a `chrono` strftime pattern.
//!
//! Scalar block: `$<name>` resolves to `String`. The `format` field is the
//! same syntax accepted by `chrono::DateTime::format` (e.g. `"%Y"`).
//! See `docs/blocks/time.now.md`.

use proc_macro2::TokenStream;
use quote::quote;
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;

use super::{BlockInstance, BlockKind, RawBlock};
use crate::manifest::ManifestError;
use crate::models::Model;

#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub enum Tag {
    #[serde(rename = "time.now")]
    Tag,
}

#[derive(Debug, Deserialize, JsonSchema, Clone)]
#[serde(deny_unknown_fields)]
#[schemars(title = "block: time.now")]
pub struct Spec {
    pub block: Tag,
    /// Binding name. `$<name>` resolves to `String` (the formatted time).
    pub name: String,
    /// `chrono` strftime pattern. Defaults to RFC 3339 when omitted.
    #[serde(default)]
    pub format: Option<String>,
    /// Timezone selector. Currently only `"utc"` is supported.
    #[serde(default)]
    pub timezone: Option<String>,
}

pub struct Kind;

impl BlockKind for Kind {
    fn id(&self) -> &'static str {
        "time.now"
    }

    fn json_schema(&self) -> RootSchema {
        schema_for!(Spec)
    }

    fn parse(&self, raw: &RawBlock) -> Result<Box<dyn BlockInstance>, ManifestError> {
        let spec: Spec =
            serde_json::from_value(raw.as_full_object()).map_err(|e| raw.parse_error(e))?;
        if let Some(tz) = spec.timezone.as_deref()
            && tz != "utc"
        {
            return Err(
                raw.validation_error(format!("time.now.timezone: only `utc` is supported (got `{tz}`)"))
            );
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
        "time.now"
    }

    fn name(&self) -> Option<&str> {
        Some(&self.spec.name)
    }

    /// Scalar `String` — formatted timestamp. The view interpolation path
    /// is `Display`-based, so plain `String` is enough.
    fn output_type(&self, _models: &[Model]) -> Option<TokenStream> {
        Some(quote! { String })
    }
}
