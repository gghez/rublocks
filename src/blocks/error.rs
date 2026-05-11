//! `error` — terminate the handler with an HTTP error.
//!
//! Typically nested under another block's `on_missing` to short-circuit the
//! handler with a structured response. See `docs/blocks/error.md`.

use proc_macro2::TokenStream;
use quote::quote;
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;

use super::runtime::{self, BlockCodegenCtx};
use super::{BlockInstance, BlockKind, LogValue, RawBlock};
use crate::manifest::ManifestError;
use crate::models::Model;
use crate::routes::RouteKind;
use crate::value_ref::ValueScope;

#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub enum Tag {
    #[serde(rename = "error")]
    Tag,
}

// `block` is the serde discriminator — read by deserialization only.
#[allow(dead_code)]
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
        let spec: Spec =
            serde_json::from_value(raw.as_full_object()).map_err(|e| raw.parse_error(e))?;
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

    fn log_fields(&self) -> Vec<(&'static str, LogValue)> {
        vec![
            ("status", LogValue::Int(self.spec.status as i64)),
            ("code", LogValue::Str(self.spec.code.clone())),
        ]
    }

    fn has_success_path(&self) -> bool {
        // Terminal block — always returns. A trailing success info! would be
        // unreachable code; skip it.
        false
    }

    fn emit_code(
        &self,
        ctx: &BlockCodegenCtx,
        _scope: &mut ValueScope,
    ) -> Result<TokenStream, String> {
        Ok(render_logged_error_return(
            ctx.index,
            ctx.route_kind,
            self.spec.status,
            &self.spec.code,
            self.spec.description.as_deref(),
        ))
    }
}

/// `error`-block emission that pairs a structured log event with the HTTP
/// short-circuit. Codegen relies on the prelude having installed
/// `__rb_block_start_{index}` (issue #17) — every wrapped block exits
/// through a `tracing::error!` event before returning the response.
pub fn render_logged_error_return(
    index: usize,
    kind: RouteKind,
    status: u16,
    code: &str,
    description: Option<&str>,
) -> TokenStream {
    let log = runtime::log_block_error_message(index, quote! { #code });
    let ret = render_error_return(kind, status, code, description);
    quote! {
        #log
        #ret
    }
}

/// Emit a `return …` statement with the given status / code / description.
///
/// Page routes emit a plain-text body; API routes a JSON object under the
/// canonical `{ "error": { ... } }` shape so clients can parse uniformly.
pub fn render_error_return(
    kind: RouteKind,
    status: u16,
    code: &str,
    description: Option<&str>,
) -> TokenStream {
    let description = description.map(|s| s.to_string());
    match kind {
        RouteKind::Api => {
            let desc_token = match description {
                Some(d) => quote! { Some(#d.to_string()) },
                None => quote! { None },
            };
            quote! {
                return crate::_rb_runtime::api_error(#status, #code.to_string(), #desc_token);
            }
        }
        RouteKind::Page => {
            let body = match description {
                Some(d) => format!("{code}: {d}"),
                None => code.to_string(),
            };
            quote! {
                return crate::_rb_runtime::page_error(#status, #body.to_string());
            }
        }
    }
}

/// Default short-circuit when a `db.find_one` returns no row and the block
/// declares no `on_missing`. Falls back to a generic 404 so handlers always
/// terminate cleanly. `index` is the parent block's codegen index — the
/// emitted `tracing::error!` reads its `__rb_block_start_{index}` local for
/// the `duration_us` field.
pub fn default_not_found(index: usize, kind: RouteKind) -> TokenStream {
    render_logged_error_return(index, kind, 404, "not_found", Some("resource not found"))
}
