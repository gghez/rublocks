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

use super::runtime::{self, BlockCodegenCtx};
use super::{BlockInstance, BlockKind, LogValue, RawBlock, model_for_table};
use crate::manifest::ManifestError;
use crate::models::Model;
use crate::value_ref::{ScopeBinding, ValueRef, ValueScope};
use crate::where_clause::WhereSpec;

/// Singleton discriminator. Anchors the `block: "db.find_many"` value
/// in the JSON schema so agents see the exact string they must write.
#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub enum Tag {
    #[serde(rename = "db.find_many")]
    Tag,
}

/// On-disk shape of the block.
///
/// `block` is the serde discriminator — read by deserialization only,
/// not by Rust code, hence the lint allow.
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
        let order_by = parse_order_by(&spec.order_by, raw)?;
        let limit = parse_pagination(&spec.limit, "limit", raw)?;
        let offset = parse_pagination(&spec.offset, "offset", raw)?;
        Ok(Box::new(Instance {
            spec,
            where_spec,
            order_by,
            limit,
            offset,
        }))
    }
}

#[derive(Debug)]
pub struct Instance {
    pub spec: Spec,
    pub where_spec: Option<WhereSpec>,
    pub order_by: Vec<OrderSpec>,
    pub limit: Option<ValueRef>,
    pub offset: Option<ValueRef>,
}

/// One sort directive. `column` is the SQL identifier; `descending` is
/// the leading-dash form parsed once at load time.
#[derive(Debug, Clone)]
pub struct OrderSpec {
    pub column: String,
    pub descending: bool,
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
                .ok_or("db.find_many requires a database service")?,
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

        let order_lit = if self.order_by.is_empty() {
            String::new()
        } else {
            let parts: Vec<String> = self
                .order_by
                .iter()
                .map(|o| {
                    let dir = if o.descending { "DESC" } else { "ASC" };
                    format!("\"{}\" {dir}", o.column)
                })
                .collect();
            format!(" ORDER BY {}", parts.join(", "))
        };
        let order_tokens = (!order_lit.is_empty()).then(|| {
            quote! { #builder.push(#order_lit); }
        });

        let limit_tokens = self
            .limit
            .as_ref()
            .map(|v| pagination_push(v, "LIMIT", scope, &builder))
            .transpose()?;
        let offset_tokens = self
            .offset
            .as_ref()
            .map(|v| pagination_push(v, "OFFSET", scope, &builder))
            .transpose()?;

        let log_err = runtime::log_block_error(ctx.index, quote! { e });
        let select_head_lit = select_head;
        let tokens = quote! {
            let #name_ident: Vec<crate::models::#model_ident> = {
                let mut #builder: sqlx::QueryBuilder<#db_ty> =
                    sqlx::QueryBuilder::new(#select_head_lit);
                #where_tokens
                #order_tokens
                #limit_tokens
                #offset_tokens
                match #builder
                    .build_query_as::<crate::models::#model_ident>()
                    .fetch_all(&__state.pg)
                    .await
                {
                    Ok(rows) => rows,
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
                kind: runtime::find_many_binding(table),
            },
        );

        Ok(tokens)
    }
}

fn pagination_push(
    v: &ValueRef,
    keyword: &str,
    scope: &ValueScope,
    builder: &proc_macro2::Ident,
) -> Result<TokenStream, String> {
    let head = format!(" {keyword} ");
    let value = v.emit_expr(scope)?;
    // sqlx LIMIT/OFFSET expect i64 on postgres; coerce.
    let expr = &value.expr;
    Ok(quote! {
        #builder.push(#head);
        #builder.push_bind((#expr) as i64);
    })
}

fn parse_order_by(value: &Option<Value>, raw: &RawBlock) -> Result<Vec<OrderSpec>, ManifestError> {
    let Some(v) = value.as_ref() else {
        return Ok(Vec::new());
    };
    match v {
        Value::String(s) => Ok(vec![one_order(s)]),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                let Value::String(s) = item else {
                    return Err(raw.validation_error(format!(
                        "order_by[{i}]: must be a string like `\"col\"` or `\"-col\"`"
                    )));
                };
                out.push(one_order(s));
            }
            Ok(out)
        }
        _ => Err(raw.validation_error(
            "order_by: must be a string or array of strings (e.g. `\"-created_at\"`)",
        )),
    }
}

fn one_order(s: &str) -> OrderSpec {
    if let Some(rest) = s.strip_prefix('-') {
        OrderSpec {
            column: rest.to_string(),
            descending: true,
        }
    } else {
        OrderSpec {
            column: s.to_string(),
            descending: false,
        }
    }
}

fn parse_pagination(
    value: &Option<Value>,
    field: &str,
    raw: &RawBlock,
) -> Result<Option<ValueRef>, ManifestError> {
    let Some(v) = value.as_ref() else {
        return Ok(None);
    };
    let parsed = ValueRef::parse(v).map_err(|e| raw.validation_error(format!("{field}: {e}")))?;
    Ok(Some(parsed))
}
