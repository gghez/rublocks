//! `xlsx.read` — parse one sheet of an XLSX workbook into a typed
//! `Vec<crate::models::T>` binding.
//!
//! Read-side conversion block. `source` references a prior
//! `bytes::Bytes` binding (e.g. `sftp.read`). `sheet` is the named tab
//! to parse; cells are coerced through the matched model's field types
//! so the bound value is the same shape a `db.find_many` would produce.
//! Errors surface in plain text so the dev overlay can fix the workbook
//! without leaving the browser.
//!
//! See `docs/blocks/xlsx.read.md`.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;
use serde_json::Value;

use super::runtime::BlockCodegenCtx;
use super::{BlockInstance, BlockKind, LogValue, RawBlock};
use crate::manifest::ManifestError;
use crate::models::{FieldType, Model};
use crate::value_ref::{BindingKind, ScopeBinding, ValueScope};

#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub enum Tag {
    #[serde(rename = "xlsx.read")]
    Tag,
}

/// On-disk shape of an `xlsx.read` block.
///
/// `block` is the serde discriminator — consumed during deserialization
/// only, hence the lint allow.
#[allow(dead_code)]
#[derive(Debug, Deserialize, JsonSchema, Clone)]
#[serde(deny_unknown_fields)]
#[schemars(title = "block: xlsx.read")]
pub struct Spec {
    pub block: Tag,
    /// Binding name. `$<name>` resolves to `Vec<crate::models::T>` (the
    /// matched model's struct) for downstream blocks / `view` / `output`.
    pub name: String,
    /// `$<block_name>` reference to a prior `bytes::Bytes` binding —
    /// typically `sftp.read`. The whole workbook body is consumed in
    /// memory; large workbooks are out of scope for v1.
    pub source: Value,
    /// Sheet name to parse. Missing sheets short-circuit with 422 and
    /// the list of available sheets so the dev overlay names the typo.
    pub sheet: String,
    /// Existing model whose field order + types drive the cell schema.
    pub model: String,
    /// Whether the first row is the header. Default `true`. When `false`,
    /// the parser treats every row (including row 0) as data.
    #[serde(default = "default_has_header")]
    pub has_header: bool,
}

fn default_has_header() -> bool {
    true
}

pub struct Kind;

impl BlockKind for Kind {
    fn id(&self) -> &'static str {
        "xlsx.read"
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
        if spec.sheet.trim().is_empty() {
            return Err(raw.validation_error("`sheet` must not be empty"));
        }
        if spec.model.trim().is_empty() {
            return Err(raw.validation_error("`model` must not be empty"));
        }
        let source_block = parse_source_ref(&spec.source, raw)?;
        Ok(Box::new(Instance { spec, source_block }))
    }
}

/// `source` must be a `$<block_name>` reference. Field access is rejected
/// because the binding has to resolve to `bytes::Bytes` (or `Vec<u8>`) at
/// the type layer — projecting a field on a scalar makes no sense here.
fn parse_source_ref(value: &Value, raw: &RawBlock) -> Result<String, ManifestError> {
    let Value::String(s) = value else {
        return Err(raw.validation_error(
            "`source`: must be a `$<block_name>` reference to a prior `bytes::Bytes` binding",
        ));
    };
    let Some(rest) = s.strip_prefix('$') else {
        return Err(raw.validation_error(format!("`source`: must start with `$` (got `{s}`)")));
    };
    if rest.contains('.') {
        return Err(raw.validation_error(format!(
            "`source`: must be `$<block_name>` (no field access), got `{s}`"
        )));
    }
    if rest.is_empty() {
        return Err(raw.validation_error("`source`: empty reference after `$`"));
    }
    Ok(rest.to_string())
}

#[derive(Debug)]
pub struct Instance {
    spec: Spec,
    source_block: String,
}

impl BlockInstance for Instance {
    fn kind_id(&self) -> &'static str {
        "xlsx.read"
    }

    fn name(&self) -> Option<&str> {
        Some(&self.spec.name)
    }

    fn output_type(&self, models: &[Model]) -> Option<TokenStream> {
        let m = models.iter().find(|m| m.name == self.spec.model)?;
        let ident = format_ident!("{}", m.name);
        Some(quote! { Vec<crate::models::#ident> })
    }

    fn log_fields(&self) -> Vec<(&'static str, LogValue)> {
        vec![
            ("sheet", LogValue::Str(self.spec.sheet.clone())),
            ("model", LogValue::Str(self.spec.model.clone())),
            ("has_header", LogValue::Int(self.spec.has_header as i64)),
        ]
    }

    fn emit_code(
        &self,
        ctx: &BlockCodegenCtx,
        scope: &mut ValueScope,
    ) -> Result<TokenStream, String> {
        let model = ctx
            .models
            .iter()
            .find(|m| m.name == self.spec.model)
            .ok_or_else(|| {
                format!(
                    "xlsx.read: no model named `{}` — declared models: {}",
                    self.spec.model,
                    ctx.models
                        .iter()
                        .map(|m| m.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
        let model_ident = format_ident!("{}", model.name);
        let name_ident = format_ident!("__block_{}", self.spec.name);

        let source_binding = scope.bindings.get(&self.source_block).ok_or_else(|| {
            format!(
                "xlsx.read: `source` references unbound block `${}`",
                self.source_block,
            )
        })?;
        if !matches!(source_binding.kind, BindingKind::Scalar { .. },) {
            return Err(format!(
                "xlsx.read: `source` block `${}` must produce a scalar `bytes::Bytes` binding",
                self.source_block,
            ));
        }
        let source_ident = &source_binding.ident;

        let sheet_name = &self.spec.sheet;
        let data_start_row: u32 = if self.spec.has_header { 1 } else { 0 };

        let field_inits: Vec<TokenStream> = model
            .fields
            .iter()
            .enumerate()
            .map(|(idx, (col, def))| emit_field_init(idx, col, def.ty, def.nullable, sheet_name))
            .collect::<Result<Vec<_>, String>>()?;

        let log_err = super::runtime::log_block_error_message(ctx.index, quote! { __msg });
        let column_count = model.fields.len();

        let tokens = quote! {
            let #name_ident: Vec<crate::models::#model_ident> = {
                use calamine::Reader as _;
                let __bytes: &bytes::Bytes = &#source_ident;
                let __cursor = ::std::io::Cursor::new(__bytes.clone().to_vec());
                let mut __wb = match calamine::open_workbook_auto_from_rs(__cursor) {
                    Ok(w) => w,
                    Err(e) => {
                        let __msg = format!("xlsx.read: workbook open failed: {e}");
                        #log_err
                        return axum::response::IntoResponse::into_response((
                            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                            format!("rublocks: {}", __msg),
                        ));
                    }
                };
                let __sheet_names: Vec<String> = __wb.sheet_names().to_owned();
                if !__sheet_names.iter().any(|n| n == #sheet_name) {
                    let __msg = format!(
                        "xlsx.read: sheet `{}` not found — available: {}",
                        #sheet_name,
                        __sheet_names.join(", "),
                    );
                    #log_err
                    return axum::response::IntoResponse::into_response((
                        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                        format!("rublocks: {}", __msg),
                    ));
                }
                let __range = match __wb.worksheet_range(#sheet_name) {
                    Ok(r) => r,
                    Err(e) => {
                        let __msg = format!("xlsx.read: sheet `{}` open failed: {e}", #sheet_name);
                        #log_err
                        return axum::response::IntoResponse::into_response((
                            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                            format!("rublocks: {}", __msg),
                        ));
                    }
                };
                let __rows: Vec<&[calamine::Data]> = __range.rows().collect();
                let mut __out: Vec<crate::models::#model_ident> = Vec::with_capacity(
                    __rows.len().saturating_sub(#data_start_row as usize),
                );
                for (__row_offset, __row) in __rows.iter().enumerate().skip(#data_start_row as usize) {
                    let __row_ref: &[calamine::Data] = __row;
                    if __row_ref.iter().all(|c| matches!(c, calamine::Data::Empty)) {
                        // Skip fully blank rows — common when a workbook
                        // tab has trailing empty rows from prior edits.
                        continue;
                    }
                    if __row_ref.len() < #column_count {
                        let __msg = format!(
                            "xlsx.read: sheet `{}` row {} has {} cells, expected at least {}",
                            #sheet_name,
                            __row_offset + 1,
                            __row_ref.len(),
                            #column_count,
                        );
                        #log_err
                        return axum::response::IntoResponse::into_response((
                            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                            format!("rublocks: {}", __msg),
                        ));
                    }
                    let __record: crate::models::#model_ident = {
                        let __sheet_name: &str = #sheet_name;
                        let __row_num: usize = __row_offset + 1;
                        let __res: ::std::result::Result<crate::models::#model_ident, String> = (|| {
                            Ok(crate::models::#model_ident {
                                #(#field_inits),*
                            })
                        })();
                        match __res {
                            Ok(r) => r,
                            Err(__msg) => {
                                #log_err
                                return axum::response::IntoResponse::into_response((
                                    axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                                    format!("rublocks: {}", __msg),
                                ));
                            }
                        }
                    };
                    __out.push(__record);
                }
                __out
            };
        };

        scope.bindings.insert(
            self.spec.name.clone(),
            ScopeBinding {
                ident: name_ident,
                kind: BindingKind::FindMany {
                    table: model.table.clone(),
                },
            },
        );

        Ok(tokens)
    }
}

/// Emit one model-field initializer reading the matching cell from `__row_ref`.
/// Coercion is type-aware: numbers + bools go through calamine's typed
/// accessors, strings through `as_string`, uuids through `Uuid::parse_str`,
/// timestamps through `DateTime::parse_from_rfc3339`. Failures bubble up via
/// `Err(String)` so the surrounding closure can short-circuit with a 422
/// carrying the cell coordinate (`Sheet!A5`-style).
fn emit_field_init(
    col_idx: usize,
    col_name: &str,
    ty: FieldType,
    nullable: bool,
    sheet_name: &str,
) -> Result<TokenStream, String> {
    let field = format_ident!("{col_name}");
    let idx = col_idx;
    let col_letter = column_to_letter(col_idx);
    let cell_ref = format!("{sheet_name}!{col_letter}");
    let parse = parse_cell_tokens(ty, &cell_ref)?;
    let body = if nullable {
        // Empty / missing cells map to None; otherwise coerce and wrap.
        quote! {
            {
                let __cell = &__row_ref[#idx];
                if matches!(__cell, calamine::Data::Empty) {
                    crate::_rb_util::NullDisplay(None)
                } else {
                    let __v = #parse;
                    crate::_rb_util::NullDisplay(Some(__v))
                }
            }
        }
    } else {
        quote! {
            {
                let __cell = &__row_ref[#idx];
                #parse
            }
        }
    };
    Ok(quote! {
        #field: #body
    })
}

/// Per-`FieldType` cell coercion. Operates on the local `__cell: &Data`
/// the surrounding scope sets up, with `__row_num` available for the
/// diagnostic. Returns an early `Err(String)` (caught one level up) when
/// the value can't be coerced.
fn parse_cell_tokens(ty: FieldType, cell_ref: &str) -> Result<TokenStream, String> {
    let typ_descr = match ty {
        FieldType::Uuid => "uuid",
        FieldType::String | FieldType::Text | FieldType::Email => "string",
        FieldType::Int => "int",
        FieldType::Bigint => "bigint",
        FieldType::Bool => "bool",
        FieldType::Timestamptz => "timestamptz",
    };
    let mismatch_msg =
        format!("xlsx.read: cell `{cell_ref}{{}}` cannot be parsed as {typ_descr}: {{:?}}");
    let parse = match ty {
        FieldType::String | FieldType::Text | FieldType::Email => quote! {
            match __cell {
                calamine::Data::String(s) => s.clone(),
                calamine::Data::Empty => String::new(),
                other => other.to_string(),
            }
        },
        FieldType::Int => quote! {
            match __cell {
                calamine::Data::Int(i) => (*i) as i32,
                calamine::Data::Float(f) => {
                    if f.fract() != 0.0 {
                        return Err(format!(#mismatch_msg, __row_num, __cell));
                    }
                    *f as i32
                }
                calamine::Data::String(s) => match s.parse::<i32>() {
                    Ok(v) => v,
                    Err(_) => return Err(format!(#mismatch_msg, __row_num, __cell)),
                },
                _ => return Err(format!(#mismatch_msg, __row_num, __cell)),
            }
        },
        FieldType::Bigint => quote! {
            match __cell {
                calamine::Data::Int(i) => *i,
                calamine::Data::Float(f) => {
                    if f.fract() != 0.0 {
                        return Err(format!(#mismatch_msg, __row_num, __cell));
                    }
                    *f as i64
                }
                calamine::Data::String(s) => match s.parse::<i64>() {
                    Ok(v) => v,
                    Err(_) => return Err(format!(#mismatch_msg, __row_num, __cell)),
                },
                _ => return Err(format!(#mismatch_msg, __row_num, __cell)),
            }
        },
        FieldType::Bool => quote! {
            match __cell {
                calamine::Data::Bool(b) => *b,
                calamine::Data::String(s) => match s.to_lowercase().as_str() {
                    "true" | "1" | "yes" => true,
                    "false" | "0" | "no" => false,
                    _ => return Err(format!(#mismatch_msg, __row_num, __cell)),
                },
                _ => return Err(format!(#mismatch_msg, __row_num, __cell)),
            }
        },
        FieldType::Uuid => quote! {
            {
                let __s = match __cell {
                    calamine::Data::String(s) => s.clone(),
                    _ => return Err(format!(#mismatch_msg, __row_num, __cell)),
                };
                match uuid::Uuid::parse_str(&__s) {
                    Ok(u) => u,
                    Err(_) => return Err(format!(#mismatch_msg, __row_num, __cell)),
                }
            }
        },
        FieldType::Timestamptz => quote! {
            {
                let __s = match __cell {
                    calamine::Data::String(s) => s.clone(),
                    _ => return Err(format!(#mismatch_msg, __row_num, __cell)),
                };
                match chrono::DateTime::parse_from_rfc3339(&__s) {
                    Ok(dt) => dt.with_timezone(&chrono::Utc),
                    Err(_) => return Err(format!(#mismatch_msg, __row_num, __cell)),
                }
            }
        },
    };
    Ok(parse)
}

/// Convert a 0-based column index to its A1-style letters.
/// Used only for diagnostic strings (`Sheet!B5`), so the cap matches
/// Excel's column count (`XFD` = 16383).
fn column_to_letter(mut idx: usize) -> String {
    let mut out = String::new();
    idx += 1;
    while idx > 0 {
        let rem = (idx - 1) % 26;
        out.insert(0, (b'A' + rem as u8) as char);
        idx = (idx - 1) / 26;
    }
    out
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
    fn parses_minimal_spec() {
        let r = raw(r#"{
            "block": "xlsx.read",
            "name": "imported",
            "source": "$workbook",
            "sheet": "Posts",
            "model": "Post"
        }"#);
        let parsed = Kind.parse(&r).unwrap();
        assert_eq!(parsed.kind_id(), "xlsx.read");
        assert_eq!(parsed.name(), Some("imported"));
    }

    #[test]
    fn parses_has_header_false() {
        let r = raw(r#"{
            "block": "xlsx.read",
            "name": "imported",
            "source": "$workbook",
            "sheet": "Posts",
            "model": "Post",
            "has_header": false
        }"#);
        Kind.parse(&r).unwrap();
    }

    #[test]
    fn rejects_source_without_dollar() {
        let r = raw(r#"{
            "block": "xlsx.read",
            "name": "imported",
            "source": "workbook",
            "sheet": "Posts",
            "model": "Post"
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(
            err.message.contains("must start with `$`"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_source_with_field_access() {
        let r = raw(r#"{
            "block": "xlsx.read",
            "name": "imported",
            "source": "$workbook.body",
            "sheet": "Posts",
            "model": "Post"
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(
            err.message.contains("no field access"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_unknown_field() {
        let r = raw(r#"{
            "block": "xlsx.read",
            "name": "imported",
            "source": "$workbook",
            "sheet": "Posts",
            "model": "Post",
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
    fn rejects_empty_sheet() {
        let r = raw(r#"{
            "block": "xlsx.read",
            "name": "imported",
            "source": "$workbook",
            "sheet": "",
            "model": "Post"
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("`sheet`"), "got: {}", err.message);
    }

    #[test]
    fn rejects_empty_model() {
        let r = raw(r#"{
            "block": "xlsx.read",
            "name": "imported",
            "source": "$workbook",
            "sheet": "Posts",
            "model": ""
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("`model`"), "got: {}", err.message);
    }

    #[test]
    fn column_letters_cover_first_two_dozens() {
        assert_eq!(column_to_letter(0), "A");
        assert_eq!(column_to_letter(1), "B");
        assert_eq!(column_to_letter(25), "Z");
        assert_eq!(column_to_letter(26), "AA");
        assert_eq!(column_to_letter(27), "AB");
        assert_eq!(column_to_letter(701), "ZZ");
        assert_eq!(column_to_letter(702), "AAA");
    }
}
