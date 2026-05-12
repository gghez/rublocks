//! `xlsx.write` — assemble an XLSX workbook from N named row collections.
//!
//! Write-side conversion block. Each entry in `sheets` references a
//! prior `Vec<crate::models::T>` binding (typically a `db.find_many`)
//! and lands as one named worksheet in the output workbook. The whole
//! workbook is buffered into `bytes::Bytes` and bound to `$<name>` so a
//! downstream block (or the API response) can ship it as
//! `application/vnd.openxmlformats-officedocument.spreadsheetml.sheet`.
//!
//! See `docs/blocks/xlsx.write.md`.

use indexmap::IndexMap;
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;

use super::runtime::BlockCodegenCtx;
use super::{BlockInstance, BlockKind, LogValue, RawBlock, model_for_table};
use crate::manifest::ManifestError;
use crate::models::{FieldType, Model};
use crate::value_ref::{BindingKind, ScopeBinding, ValueScope};

/// XLSX file format constraint — sheet names are capped at 31 characters
/// by the OOXML spec. Validated at load time so the dev overlay catches
/// the typo before the generated binary ever fires.
const SHEET_NAME_MAX_LEN: usize = 31;

/// Characters Excel rejects in a sheet name. The spec lists exactly six;
/// many tools also reject a leading or trailing apostrophe, but the
/// hard-coded validator here mirrors the OOXML rules verbatim.
const SHEET_NAME_FORBIDDEN: &[char] = &[':', '\\', '/', '?', '*', '[', ']'];

#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub enum Tag {
    #[serde(rename = "xlsx.write")]
    Tag,
}

/// On-disk shape of an `xlsx.write` block.
///
/// `block` is the serde discriminator — consumed during deserialization
/// only, hence the lint allow.
#[allow(dead_code)]
#[derive(Debug, Deserialize, JsonSchema, Clone)]
#[serde(deny_unknown_fields)]
#[schemars(title = "block: xlsx.write")]
pub struct Spec {
    pub block: Tag,
    /// Binding name. `$<name>` resolves to `bytes::Bytes` (the encoded
    /// workbook body, MIME
    /// `application/vnd.openxmlformats-officedocument.spreadsheetml.sheet`).
    pub name: String,
    /// Ordered map `sheet_name -> SheetSpec`. Source order is preserved so
    /// the workbook's sheet tabs come out in the order the author wrote them.
    pub sheets: IndexMap<String, SheetSpec>,
}

/// One sheet inside an `xlsx.write` workbook.
#[derive(Debug, Deserialize, JsonSchema, Clone)]
#[serde(deny_unknown_fields)]
pub struct SheetSpec {
    /// `$<name>` reference to a prior `Vec<T>` binding (typically
    /// `db.find_many`). The model's columns drive the cell types.
    pub rows: Value,
    /// Optional explicit header row. When omitted the model's field
    /// order doubles as the header — matches `csv.write` semantics so
    /// switching between the two formats is mechanical.
    #[serde(default)]
    pub headers: Option<Vec<String>>,
}

pub struct Kind;

impl BlockKind for Kind {
    fn id(&self) -> &'static str {
        "xlsx.write"
    }

    fn json_schema(&self) -> RootSchema {
        schema_for!(Spec)
    }

    fn parse(&self, raw: &RawBlock) -> Result<Box<dyn BlockInstance>, ManifestError> {
        let spec: Spec =
            serde_json::from_value(raw.as_full_object()).map_err(|e| raw.parse_error(e))?;
        if spec.name.trim().is_empty() {
            return Err(raw.validation_error("`name` must not be empty"));
        }
        if spec.sheets.is_empty() {
            return Err(raw.validation_error("`sheets` must declare at least one entry"));
        }
        let mut seen: HashSet<String> = HashSet::new();
        let mut parsed_sheets: Vec<ParsedSheet> = Vec::with_capacity(spec.sheets.len());
        for (sheet_name, sheet) in &spec.sheets {
            validate_sheet_name(sheet_name, raw)?;
            let lower = sheet_name.to_lowercase();
            if !seen.insert(lower) {
                return Err(raw.validation_error(format!(
                    "`sheets`: duplicate sheet name `{sheet_name}` (Excel sheet names are case-insensitive)"
                )));
            }
            let rows_ref = parse_rows_ref(&sheet.rows, sheet_name, raw)?;
            parsed_sheets.push(ParsedSheet {
                name: sheet_name.clone(),
                rows_block: rows_ref,
                headers: sheet.headers.clone(),
            });
        }
        Ok(Box::new(Instance {
            spec_name: spec.name,
            sheets: parsed_sheets,
        }))
    }
}

fn validate_sheet_name(name: &str, raw: &RawBlock) -> Result<(), ManifestError> {
    if name.is_empty() {
        return Err(raw.validation_error("`sheets`: sheet name must not be empty"));
    }
    if name.chars().count() > SHEET_NAME_MAX_LEN {
        return Err(raw.validation_error(format!(
            "`sheets`: sheet name `{name}` exceeds {SHEET_NAME_MAX_LEN} chars (XLSX limit)"
        )));
    }
    if let Some(c) = name.chars().find(|c| SHEET_NAME_FORBIDDEN.contains(c)) {
        return Err(raw.validation_error(format!(
            "`sheets`: sheet name `{name}` contains forbidden character `{c}` — Excel rejects any of `: \\ / ? * [ ]`"
        )));
    }
    Ok(())
}

/// Sheet `rows` accepts the `$<block_name>` form only — pointing at a
/// `Vec<T>` binding from a prior block. Anything else is rejected at
/// load time so the user sees the bad reference instead of a runtime
/// type error.
fn parse_rows_ref(value: &Value, sheet: &str, raw: &RawBlock) -> Result<String, ManifestError> {
    let Value::String(s) = value else {
        return Err(raw.validation_error(format!(
            "`sheets.{sheet}.rows`: must be a `$<block_name>` reference to a prior list binding"
        )));
    };
    let Some(rest) = s.strip_prefix('$') else {
        return Err(raw.validation_error(format!(
            "`sheets.{sheet}.rows`: must start with `$` (got `{s}`)"
        )));
    };
    if rest.contains('.') {
        return Err(raw.validation_error(format!(
            "`sheets.{sheet}.rows`: must be `$<block_name>` (no field access), got `{s}`"
        )));
    }
    if rest.is_empty() {
        return Err(
            raw.validation_error(format!("`sheets.{sheet}.rows`: empty reference after `$`"))
        );
    }
    Ok(rest.to_string())
}

#[derive(Debug, Clone)]
struct ParsedSheet {
    name: String,
    rows_block: String,
    headers: Option<Vec<String>>,
}

#[derive(Debug)]
pub struct Instance {
    spec_name: String,
    sheets: Vec<ParsedSheet>,
}

impl BlockInstance for Instance {
    fn kind_id(&self) -> &'static str {
        "xlsx.write"
    }

    fn name(&self) -> Option<&str> {
        Some(&self.spec_name)
    }

    fn output_type(&self, _models: &[Model]) -> Option<TokenStream> {
        Some(quote! { bytes::Bytes })
    }

    fn log_fields(&self) -> Vec<(&'static str, LogValue)> {
        let sheet_names = self
            .sheets
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(",");
        vec![
            ("sheets", LogValue::Str(sheet_names)),
            ("sheet_count", LogValue::Int(self.sheets.len() as i64)),
        ]
    }

    fn emit_code(
        &self,
        ctx: &BlockCodegenCtx,
        scope: &mut ValueScope,
    ) -> Result<TokenStream, String> {
        let name_ident = format_ident!("__block_{}", self.spec_name);
        let mut sheet_pieces: Vec<TokenStream> = Vec::with_capacity(self.sheets.len());
        for sheet in &self.sheets {
            sheet_pieces.push(emit_sheet(sheet, ctx, scope)?);
        }
        let log_err = super::runtime::log_block_error(ctx.index, quote! { e });

        let tokens = quote! {
            let #name_ident: bytes::Bytes = {
                let mut __wb: rust_xlsxwriter::Workbook = rust_xlsxwriter::Workbook::new();
                #(#sheet_pieces)*
                match __wb.save_to_buffer() {
                    Ok(buf) => bytes::Bytes::from(buf),
                    Err(e) => {
                        #log_err
                        return axum::response::IntoResponse::into_response((
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            format!("rublocks: xlsx.write failed: {e}"),
                        ));
                    }
                }
            };
        };

        scope.bindings.insert(
            self.spec_name.clone(),
            ScopeBinding {
                ident: name_ident,
                kind: BindingKind::Scalar {
                    ty: quote! { bytes::Bytes },
                },
            },
        );

        Ok(tokens)
    }
}

/// Emit the per-sheet tokens: name the sheet, write the header row, then
/// iterate the bound `Vec<T>` and write one row per record.
fn emit_sheet(
    sheet: &ParsedSheet,
    ctx: &BlockCodegenCtx,
    scope: &ValueScope,
) -> Result<TokenStream, String> {
    let binding = scope.bindings.get(&sheet.rows_block).ok_or_else(|| {
        format!(
            "xlsx.write: sheet `{}` references unbound block `${}`",
            sheet.name, sheet.rows_block,
        )
    })?;
    let table = match &binding.kind {
        BindingKind::FindMany { table } => table.clone(),
        _ => {
            return Err(format!(
                "xlsx.write: sheet `{}` rows reference `${}` must be a list binding (db.find_many)",
                sheet.name, sheet.rows_block,
            ));
        }
    };
    let model = model_for_table(ctx.models, &table)
        .ok_or_else(|| format!("xlsx.write: no model declares table `{table}`"))?;
    let columns: Vec<String> = model.fields.keys().cloned().collect();
    let headers: Vec<String> = match sheet.headers.as_ref() {
        Some(h) => {
            if h.len() != columns.len() {
                return Err(format!(
                    "xlsx.write: sheet `{}` has {} headers but the model `{}` has {} fields",
                    sheet.name,
                    h.len(),
                    model.name,
                    columns.len(),
                ));
            }
            h.clone()
        }
        None => columns.clone(),
    };
    let sheet_name = &sheet.name;
    let rows_ident = &binding.ident;
    let mut col_writes: Vec<TokenStream> = Vec::with_capacity(columns.len());
    for (idx, col_name) in columns.iter().enumerate() {
        let field = model
            .fields
            .get(col_name)
            .expect("column came from model.fields.keys");
        let col_idx = idx as u16;
        col_writes.push(emit_cell_write(
            col_name,
            field.ty,
            field.nullable,
            sheet_name,
            col_idx,
        ));
    }
    let header_writes: Vec<TokenStream> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let i = i as u16;
            quote! {
                __sheet.write(0u32, #i, #h)
                    .map_err(|e| format!("xlsx.write sheet `{}` header col {}: {e}", #sheet_name, #i))?;
            }
        })
        .collect();
    let log_err_inner_block = super::runtime::log_block_error(ctx.index, quote! { e });
    let log_err_msg = super::runtime::log_block_error_message(ctx.index, quote! { __msg });

    Ok(quote! {
        {
            let __sheet = __wb.add_worksheet();
            if let Err(e) = __sheet.set_name(#sheet_name) {
                #log_err_inner_block
                return axum::response::IntoResponse::into_response((
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("rublocks: xlsx.write sheet `{}` name rejected: {e}", #sheet_name),
                ));
            }
            let __res: ::std::result::Result<(), String> = (|| {
                #(#header_writes)*
                for (__i, __row) in #rows_ident.iter().enumerate() {
                    let __row_idx: u32 = (__i as u32) + 1;
                    #(#col_writes)*
                }
                Ok(())
            })();
            if let Err(__msg) = __res {
                #log_err_msg
                return axum::response::IntoResponse::into_response((
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("rublocks: {}", __msg),
                ));
            }
        }
    })
}

/// Emit the per-cell `__sheet.write(row, col, value)?` statement for one
/// model column. Type fidelity is preserved where Excel has a native shape
/// (numbers / booleans); types without one (`uuid::Uuid`,
/// `chrono::DateTime`) ride in as their string projection so the workbook
/// is visually correct and lossless on round-trip via `as_string`.
fn emit_cell_write(
    col: &str,
    ty: FieldType,
    nullable: bool,
    sheet_name: &str,
    col_idx: u16,
) -> TokenStream {
    let field = format_ident!("{col}");
    let map_err = quote! {
        .map_err(|e| format!(
            "xlsx.write sheet `{}` row {} col {}: {e}",
            #sheet_name, __row_idx, #col_idx,
        ))?
    };
    if nullable {
        // `NullDisplay<T>` wraps `Option<T>`. `None` leaves the cell empty;
        // `Some(v)` writes the typed value so numeric columns stay numeric
        // in the workbook (Excel sees them as numbers, not text).
        let some_arm = match ty {
            FieldType::Int => quote! { __sheet.write(__row_idx, #col_idx, *__v as i32) #map_err; },
            FieldType::Bigint => {
                quote! { __sheet.write(__row_idx, #col_idx, *__v as i64) #map_err; }
            }
            FieldType::Bool => quote! { __sheet.write(__row_idx, #col_idx, *__v) #map_err; },
            FieldType::String | FieldType::Text | FieldType::Email => {
                quote! { __sheet.write(__row_idx, #col_idx, __v.as_str()) #map_err; }
            }
            FieldType::Uuid | FieldType::Timestamptz => {
                quote! { __sheet.write(__row_idx, #col_idx, __v.to_string()) #map_err; }
            }
        };
        quote! {
            if let Some(__v) = __row.#field.0.as_ref() {
                #some_arm
            }
        }
    } else {
        let write = match ty {
            FieldType::Int => quote! { __sheet.write(__row_idx, #col_idx, __row.#field as i32) },
            FieldType::Bigint => {
                quote! { __sheet.write(__row_idx, #col_idx, __row.#field as i64) }
            }
            FieldType::Bool => quote! { __sheet.write(__row_idx, #col_idx, __row.#field) },
            FieldType::String | FieldType::Text | FieldType::Email => {
                quote! { __sheet.write(__row_idx, #col_idx, __row.#field.as_str()) }
            }
            FieldType::Uuid | FieldType::Timestamptz => {
                quote! { __sheet.write(__row_idx, #col_idx, __row.#field.to_string()) }
            }
        };
        quote! { #write #map_err; }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fake_path() -> PathBuf {
        PathBuf::from("/fake/route.json")
    }

    fn raw(body: &str) -> RawBlock {
        let v: Value = serde_json::from_str(body).unwrap();
        RawBlock::from_value(&v, &fake_path(), "process[0]").unwrap()
    }

    #[test]
    fn parses_minimal_workbook() {
        let r = raw(r#"{
            "block": "xlsx.write",
            "name": "report",
            "sheets": {
                "Posts": { "rows": "$posts" }
            }
        }"#);
        let parsed = Kind.parse(&r).unwrap();
        assert_eq!(parsed.kind_id(), "xlsx.write");
        assert_eq!(parsed.name(), Some("report"));
    }

    #[test]
    fn parses_multiple_sheets() {
        let r = raw(r#"{
            "block": "xlsx.write",
            "name": "report",
            "sheets": {
                "Posts":    { "rows": "$posts" },
                "Comments": { "rows": "$comments", "headers": ["id", "post_id", "body"] }
            }
        }"#);
        Kind.parse(&r).unwrap();
    }

    #[test]
    fn rejects_empty_sheets() {
        let r = raw(r#"{
            "block": "xlsx.write",
            "name": "report",
            "sheets": {}
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("at least one"), "got: {}", err.message);
    }

    #[test]
    fn rejects_sheet_name_too_long() {
        let long = "X".repeat(SHEET_NAME_MAX_LEN + 1);
        let r = raw(&format!(
            r#"{{
                "block": "xlsx.write",
                "name": "report",
                "sheets": {{ "{long}": {{ "rows": "$posts" }} }}
            }}"#
        ));
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("31"), "got: {}", err.message);
    }

    #[test]
    fn rejects_sheet_name_with_forbidden_char() {
        let r = raw(r#"{
            "block": "xlsx.write",
            "name": "report",
            "sheets": { "Sheet/Posts": { "rows": "$posts" } }
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(
            err.message.contains("forbidden character"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_rows_without_dollar_prefix() {
        let r = raw(r#"{
            "block": "xlsx.write",
            "name": "report",
            "sheets": { "Posts": { "rows": "posts" } }
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(
            err.message.contains("must start with `$`"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_rows_field_access() {
        let r = raw(r#"{
            "block": "xlsx.write",
            "name": "report",
            "sheets": { "Posts": { "rows": "$posts.id" } }
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(
            err.message.contains("no field access"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_duplicate_sheet_names_case_insensitive() {
        // OOXML treats sheet names case-insensitively; reject duplicates
        // at load time so the workbook open never errors out at runtime.
        let r = raw(r#"{
            "block": "xlsx.write",
            "name": "report",
            "sheets": {
                "Posts": { "rows": "$posts" },
                "posts": { "rows": "$posts" }
            }
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("duplicate"), "got: {}", err.message);
    }

    #[test]
    fn rejects_unknown_field_on_spec() {
        let r = raw(r#"{
            "block": "xlsx.write",
            "name": "report",
            "sheets": { "Posts": { "rows": "$posts" } },
            "junk": true
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(
            err.message.contains("unknown field"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_unknown_field_on_sheet() {
        let r = raw(r#"{
            "block": "xlsx.write",
            "name": "report",
            "sheets": { "Posts": { "rows": "$posts", "extra": 1 } }
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(
            err.message.contains("unknown field"),
            "got: {}",
            err.message
        );
    }
}
