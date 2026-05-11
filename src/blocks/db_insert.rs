//! `db.insert` — insert a single row into a table.
//!
//! Write-side block. Does not bind a value: `$<name>` references against an
//! insert block are rejected at load time. See `docs/blocks/db.insert.md`.

use indexmap::IndexMap;
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;
use serde_json::Value;

use super::runtime::{self, BlockCodegenCtx};
use super::{BlockInstance, BlockKind, RawBlock};
use crate::manifest::ManifestError;
use crate::models::Model;
use crate::value_ref::{ValueRef, ValueScope};

#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub enum Tag {
    #[serde(rename = "db.insert")]
    Tag,
}

// `block` is the serde discriminator — read by deserialization only.
#[allow(dead_code)]
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
        let spec: Spec =
            serde_json::from_value(raw.as_full_object()).map_err(|e| raw.parse_error(e))?;
        if spec.values.is_empty() {
            return Err(raw.validation_error("db.insert requires at least one entry in `values`"));
        }
        let mut parsed: IndexMap<String, ValueRef> = IndexMap::with_capacity(spec.values.len());
        for (col, v) in &spec.values {
            let r = ValueRef::parse(v)
                .map_err(|e| raw.validation_error(format!("values.{col}: {e}")))?;
            parsed.insert(col.clone(), r);
        }
        Ok(Box::new(Instance {
            spec,
            values: parsed,
        }))
    }
}

#[derive(Debug)]
pub struct Instance {
    pub spec: Spec,
    pub values: IndexMap<String, ValueRef>,
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

    fn target_table(&self) -> Option<&str> {
        Some(&self.spec.table)
    }

    fn insert_values(&self) -> Option<&indexmap::IndexMap<String, ValueRef>> {
        Some(&self.values)
    }

    fn emit_code(
        &self,
        ctx: &BlockCodegenCtx,
        scope: &mut ValueScope,
    ) -> Result<TokenStream, String> {
        let table = &self.spec.table;
        let db_ty =
            runtime::sqlx_database_ty(ctx.db_kind.ok_or("db.insert requires a database service")?);
        let model = ctx
            .models
            .iter()
            .find(|m| m.table == *table)
            .ok_or_else(|| format!("no model declares table `{table}`"))?;
        for col in self.values.keys() {
            if !model.fields.contains_key(col) {
                return Err(format!(
                    "db.insert.values references unknown column `{col}` on table `{table}` — known: {}",
                    model.fields.keys().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
        }
        let builder = format_ident!("__qb");
        let cols_csv = self
            .values
            .keys()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let head = format!("INSERT INTO \"{table}\" ({cols_csv}) VALUES (");

        // Field-level CEL validators on the columns we're writing run
        // before the INSERT. The full set of bindings to expose to the
        // CEL context is the column being checked, bound under its own
        // name.
        let mut field_checks: Vec<TokenStream> = Vec::new();
        let mut value_pushes: Vec<TokenStream> = Vec::new();
        let mut first = true;
        for (col, vref) in &self.values {
            let resolved = vref.emit_expr(scope)?;
            let raw_expr = &resolved.expr;
            let sep = if first {
                quote! {}
            } else {
                quote! { #builder.push(", "); }
            };
            first = false;
            // Bind the column value.
            value_pushes.push(quote! {
                #sep
                #builder.push_bind((#raw_expr).clone());
            });

            // CEL validator on this column?
            let field = &model.fields[col];
            if let Some(cel_src) = field.validate.as_deref() {
                let prog_ident = format_ident!(
                    "__RB_INSERT_VALIDATE_{}_{}",
                    table.to_uppercase().replace('.', "_"),
                    col.to_uppercase()
                );
                let label = format!("db.insert.{col}.validate");
                let binding = cel_binding_for_field(col, field.ty);
                field_checks.push(quote! {
                    {
                        static #prog_ident: std::sync::OnceLock<cel::Program> =
                            std::sync::OnceLock::new();
                        let __prog = #prog_ident.get_or_init(|| {
                            cel::Program::compile(#cel_src)
                                .expect("CEL was syntax-checked at build time")
                        });
                        let mut __ctx = cel::Context::default();
                        let __v = &(#raw_expr);
                        #binding
                        let __pass = matches!(
                            __prog.execute(&__ctx),
                            Ok(cel::Value::Bool(true)),
                        );
                        if !__pass {
                            let _ = #label;
                            return crate::_rb_runtime::field_validation_error(
                                #col.to_string(),
                                #cel_src.to_string(),
                            );
                        }
                    }
                });
            }
        }

        let tokens = quote! {
            #(#field_checks)*
            {
                let mut #builder: sqlx::QueryBuilder<#db_ty> =
                    sqlx::QueryBuilder::new(#head);
                #(#value_pushes)*
                #builder.push(")");
                if let Err(e) = #builder.build().execute(&__state.pg).await {
                    return crate::_rb_runtime::db_error(e);
                }
            }
        };
        let _ = scope; // db.insert binds nothing for now.
        Ok(tokens)
    }
}

/// Bind one column value into a CEL `Context` so a field-level validator
/// can reference it by name (matching the input-validator convention).
fn cel_binding_for_field(name: &str, ty: crate::models::FieldType) -> TokenStream {
    use crate::models::FieldType;
    match ty {
        FieldType::Int => quote! { __ctx.add_variable_from_value(#name, *__v as i64); },
        FieldType::Bigint => quote! { __ctx.add_variable_from_value(#name, *__v); },
        FieldType::Bool => quote! { __ctx.add_variable_from_value(#name, *__v); },
        FieldType::String | FieldType::Text | FieldType::Email => {
            quote! { __ctx.add_variable_from_value(#name, __v.to_string()); }
        }
        FieldType::Uuid => quote! { __ctx.add_variable_from_value(#name, __v.to_string()); },
        FieldType::Timestamptz => {
            quote! { __ctx.add_variable_from_value(#name, __v.to_rfc3339()); }
        }
    }
}
