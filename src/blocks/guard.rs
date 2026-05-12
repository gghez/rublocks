//! `guard` — authorize the current request against a CEL predicate.
//!
//! Composes inside `process` like any other block: the scope of names the
//! `if` expression can reference is exactly what has been bound by prior
//! blocks plus the route input. Place a guard at the top of `process` for
//! an early check (`user.is_admin`), or after a `db.find_*` that loads the
//! row whose ownership you want to assert (`post.author_id == user.id`).
//!
//! Runtime semantics: when `if` evaluates to `false`, the handler
//! short-circuits with `403 Forbidden`. The exact response shape (JSON
//! for `kind: api`, plain text for `kind: page`) lives in the
//! `_rb_runtime` module emitted into the dist crate.
//!
//! See `docs/blocks/guard.md`.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;

use super::runtime::{self, BlockCodegenCtx};
use super::{BlockInstance, BlockKind, LogValue, RawBlock};
use crate::expressions;
use crate::manifest::ManifestError;
use crate::models::Model;
use crate::routes::RouteKind;
use crate::value_ref::{BindingKind, ValueScope};

/// Singleton discriminator. Anchors `block: "guard"` in the JSON schema
/// so agents see the exact string they must write.
#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub enum Tag {
    #[serde(rename = "guard")]
    Tag,
}

// `block` is the serde discriminator — read by deserialization only.
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

    fn log_fields(&self) -> Vec<(&'static str, LogValue)> {
        vec![("predicate", LogValue::Str(self.spec.r#if.clone()))]
    }

    fn emit_code(
        &self,
        ctx: &BlockCodegenCtx,
        scope: &mut ValueScope,
    ) -> Result<TokenStream, String> {
        let prog_ident = format_ident!("__RB_GUARD_PROG_{}", ctx.index);
        let label = format!("process[{}].if", ctx.index);
        let expr = &self.spec.r#if;
        let context = build_guard_context(scope);
        let forbidden_call = match ctx.route_kind {
            RouteKind::Api => quote! { return crate::_rb_runtime::api_403(); },
            RouteKind::Page => quote! { return crate::_rb_runtime::page_403(); },
        };
        let log_denied = runtime::log_block_error_message(
            ctx.index,
            quote! { format!("guard denied: {}", #expr) },
        );
        Ok(quote! {
            {
                static #prog_ident: std::sync::OnceLock<cel::Program> =
                    std::sync::OnceLock::new();
                let __prog = #prog_ident.get_or_init(|| {
                    cel::Program::compile(#expr)
                        .expect("CEL was syntax-checked at build time")
                });
                let mut __ctx = cel::Context::default();
                #context
                let __pass = matches!(
                    __prog.execute(&__ctx),
                    Ok(cel::Value::Bool(true)),
                );
                if !__pass {
                    let _ = #label;
                    #log_denied
                    #forbidden_call
                }
            }
        })
    }
}

/// Build the CEL context for one guard expression: input fields bound by
/// their declared name (as in [`crate::codegen_input::render_input_cel_bindings_raw`])
/// plus every prior block binding under its `$<name>` ident.
///
/// Block bindings expose their *whole row* under the binding name. Field
/// access (`post.author_id`) goes through CEL's struct-style access on
/// the serialized form — UUID/timestamp scalars stringify on the way in.
///
/// The serialization path goes through `cel::to_value` (not
/// `serde_json::to_value`): `cel::Context::add_variable_from_value`
/// requires `Into<cel::Value>`, and `serde_json::Value` has no such
/// conversion. Going straight to `cel::Value` keeps every prior binding
/// — including layout-bound scalars like `time.now` — usable inside the
/// guard's CEL scope without requiring a `serde_json`→`cel` adapter.
fn build_guard_context(scope: &ValueScope) -> TokenStream {
    let input_bindings = scope
        .input
        .map(crate::codegen_input::render_input_cel_bindings_raw)
        .unwrap_or_default();
    let block_bindings = scope.bindings.iter().map(|(name, b)| {
        let ident = &b.ident;
        match &b.kind {
            BindingKind::FindOne { .. }
            | BindingKind::FindMany { .. }
            | BindingKind::Scalar { .. } => quote! {
                if let Ok(__rb_v) = ::cel::to_value(&#ident) {
                    __ctx.add_variable_from_value(#name, __rb_v);
                }
            },
        }
    });
    quote! {
        #input_bindings
        #(#block_bindings)*
    }
}
