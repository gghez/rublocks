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

use super::runtime::{self, BlockCodegenCtx};
use super::{BlockInstance, BlockKind, LogValue, RawBlock, model_for_table};
use crate::manifest::ManifestError;
use crate::models::Model;
use crate::value_ref::{ScopeBinding, ValueScope};
use crate::where_clause::WhereSpec;

#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub enum Tag {
    #[serde(rename = "db.find_one")]
    Tag,
}

// `block` is the serde discriminator — read by deserialization only.
#[allow(dead_code)]
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
        let spec: Spec =
            serde_json::from_value(raw.as_full_object()).map_err(|e| raw.parse_error(e))?;
        let where_spec = match spec.r#where.as_ref() {
            None => None,
            Some(v) => {
                let parsed = WhereSpec::parse(v, &format!("{}.where", raw.label))
                    .map_err(|e| raw.validation_error(e))?;
                if let WhereSpec::Cel(expr) = &parsed {
                    crate::expressions::validate(
                        expr,
                        &raw.source,
                        &format!("{}.where", raw.label),
                    )?;
                }
                Some(parsed)
            }
        };
        let on_missing = match spec.on_missing.as_ref() {
            Some(v) => {
                let nested =
                    RawBlock::from_value(v, &raw.source, &format!("{}.on_missing", raw.label))?;
                Some(super::parse(&nested)?)
            }
            None => None,
        };
        Ok(Box::new(Instance {
            spec,
            where_spec,
            on_missing,
        }))
    }
}

#[derive(Debug)]
pub struct Instance {
    pub spec: Spec,
    pub where_spec: Option<WhereSpec>,
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

    fn embeds_runtime_cel(&self) -> bool {
        matches!(self.where_spec.as_ref(), Some(WhereSpec::Cel(_)))
    }

    fn where_spec(&self) -> Option<&WhereSpec> {
        self.where_spec.as_ref()
    }

    fn target_table(&self) -> Option<&str> {
        Some(&self.spec.table)
    }

    fn log_fields(&self) -> Vec<(&'static str, LogValue)> {
        vec![("table", LogValue::Str(self.spec.table.clone()))]
    }

    fn emit_code(
        &self,
        ctx: &BlockCodegenCtx,
        scope: &mut ValueScope,
    ) -> Result<TokenStream, String> {
        let table = &self.spec.table;
        let name_ident = format_ident!("__block_{}", self.spec.name);
        let model = ctx
            .models
            .iter()
            .find(|m| m.table == *table)
            .ok_or_else(|| format!("no model declares table `{table}`"))?;
        let model_ident = format_ident!("{}", model.name);
        let db_ty = runtime::sqlx_database_ty(
            ctx.db_kind
                .ok_or("db.find_one requires a database service")?,
        );
        let select_head = runtime::select_head(table, ctx.models)?;
        let builder = format_ident!("__qb");

        let where_tokens = if let Some(ws) = self.where_spec.as_ref() {
            let body = runtime::emit_where(ws, table, scope, &builder, ctx.models)?;
            body.map(|b| {
                quote! {
                    #builder.push(" WHERE ");
                    #b
                }
            })
        } else {
            None
        };

        // on_missing: emit the sub-block's tokens, wrapped with its own
        // logging prelude + success so the nested block carries its own
        // span (block=error / table=… / etc.) rather than inheriting this
        // block's. The sub-block runs in a snapshot scope — `error`
        // short-circuits the handler, so nothing propagates back here.
        let on_missing_tokens = if let Some(sub) = self.on_missing.as_ref() {
            let mut sub_scope = ValueScope {
                input: scope.input,
                bindings: scope.bindings.clone(),
                models: scope.models,
            };
            runtime::emit_block_with_logging(sub.as_ref(), ctx, &mut sub_scope)?
        } else {
            // No on_missing — return a default 404 with a generic body.
            super::error::default_not_found(ctx.index, ctx.route_kind)
        };

        let log_err = runtime::log_block_error(ctx.index, quote! { e });
        let select_head_lit = select_head;
        let tokens = quote! {
            let #name_ident: crate::models::#model_ident = {
                let mut #builder: sqlx::QueryBuilder<#db_ty> =
                    sqlx::QueryBuilder::new(#select_head_lit);
                #where_tokens
                let row_result = #builder
                    .build_query_as::<crate::models::#model_ident>()
                    .fetch_optional(&__state.pg)
                    .await;
                match row_result {
                    Ok(Some(r)) => r,
                    Ok(None) => {
                        #on_missing_tokens
                    }
                    Err(e) => {
                        #log_err
                        return crate::_rb_runtime::db_error(e);
                    }
                }
            };
        };

        scope.bindings.insert(
            self.spec.name.clone(),
            ScopeBinding {
                ident: name_ident,
                kind: runtime::find_one_binding(table),
            },
        );

        Ok(tokens)
    }
}
