//! `csv.read` — parse a CSV byte buffer into `Vec<crate::models::T>`.
//!
//! Read-side counterpart of [`super::csv_write`]. Takes a `$ref` pointing
//! at a `bytes::Bytes` binding (typically `sftp.read` or a multipart body
//! field once that lands) and binds the parsed rows to `$<name>` as
//! `Vec<crate::models::T>` for the declared model. Header validation
//! happens up-front (extra / missing columns → 400) and each per-row
//! parse failure short-circuits with `422` carrying the offending line
//! and column name — the dev-mode user fixes the source from the
//! browser.
//!
//! See `docs/blocks/csv.read.md` and `docs/encoding.md` for the
//! project-wide encoding contract this block inherits.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;
use serde_json::Value;

use super::runtime::{self, BlockCodegenCtx};
use super::{BlockInstance, BlockKind, LogValue, RawBlock};
use crate::manifest::ManifestError;
use crate::models::{FieldType, Model};
use crate::value_ref::{BindingKind, ScopeBinding, ValueScope};

#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub enum Tag {
    #[serde(rename = "csv.read")]
    Tag,
}

// `block` is a serde discriminator — consumed by deserialization only.
#[allow(dead_code)]
#[derive(Debug, Deserialize, JsonSchema, Clone)]
#[serde(deny_unknown_fields)]
#[schemars(title = "block: csv.read")]
pub struct Spec {
    pub block: Tag,
    /// Binding name. `$<name>` resolves to `Vec<crate::models::T>` for
    /// downstream blocks and `view` / `output`.
    pub name: String,
    /// `$<binding>` reference to a `bytes::Bytes` (or any `AsRef<[u8]>`)
    /// value produced earlier in the pipeline.
    pub source: Value,
    /// Existing model whose fields define the record schema. Every header
    /// column must match a field, and every field must be present in the
    /// header row.
    pub model: String,
    /// Single-byte CSV delimiter. Default `,`.
    #[serde(default)]
    pub delimiter: Option<String>,
    /// True when the first record is a header row to be validated against
    /// the model fields. False when columns are positional in declared
    /// field order. Default `true`.
    #[serde(default)]
    pub has_header: Option<bool>,
    /// Character encoding for the incoming bytes. When omitted, inherits
    /// the project-wide `main.json.encoding`. Today only `"utf-8"` is
    /// accepted; any other value fails at build time.
    #[serde(default)]
    pub encoding: Option<String>,
}

pub struct Kind;

impl BlockKind for Kind {
    fn id(&self) -> &'static str {
        "csv.read"
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
        if spec.model.trim().is_empty() {
            return Err(raw.validation_error("`model` must not be empty"));
        }
        let source_ref = parse_source_ref(&spec.source, raw)?;
        let delimiter = parse_delimiter(spec.delimiter.as_deref(), raw)?;
        let declared_encoding = match spec.encoding.as_deref() {
            Some(s) => Some(
                crate::manifest::parse_encoding(s, &raw.source)
                    .map_err(|e| raw.validation_error(format!("`encoding`: {}", e.message)))?,
            ),
            None => None,
        };
        Ok(Box::new(Instance {
            spec,
            source_ref,
            delimiter,
            declared_encoding,
        }))
    }
}

/// `source` accepts a single `$<binding>` form. Anything else is rejected
/// at load time.
fn parse_source_ref(value: &Value, raw: &RawBlock) -> Result<String, ManifestError> {
    match value {
        Value::String(s) => match s.strip_prefix('$') {
            Some(rest) if !rest.is_empty() && !rest.contains('.') => Ok(rest.to_string()),
            _ => Err(raw.validation_error(format!(
                "`source` must reference a prior block as `$<name>` (got `{s}`)"
            ))),
        },
        _ => Err(raw.validation_error("`source` must be a `$<name>` string")),
    }
}

/// Same shape as `csv.write` — single ASCII char, default `,`.
fn parse_delimiter(value: Option<&str>, raw: &RawBlock) -> Result<u8, ManifestError> {
    let Some(s) = value else { return Ok(b',') };
    let bytes = s.as_bytes();
    if bytes.len() != 1 || !s.is_ascii() {
        return Err(raw.validation_error(format!(
            "`delimiter` must be a single ASCII character (got `{s}`)"
        )));
    }
    Ok(bytes[0])
}

#[derive(Debug)]
pub struct Instance {
    pub spec: Spec,
    source_ref: String,
    delimiter: u8,
    declared_encoding: Option<crate::manifest::Encoding>,
}

impl BlockInstance for Instance {
    fn kind_id(&self) -> &'static str {
        "csv.read"
    }

    fn name(&self) -> Option<&str> {
        Some(&self.spec.name)
    }

    fn output_type(&self, models: &[Model]) -> Option<TokenStream> {
        let model = models.iter().find(|m| m.name == self.spec.model)?;
        let ident = format_ident!("{}", model.name);
        Some(quote! { Vec<crate::models::#ident> })
    }

    fn log_fields(&self) -> Vec<(&'static str, LogValue)> {
        vec![
            ("model", LogValue::Str(self.spec.model.clone())),
            ("source_ref", LogValue::Str(self.source_ref.clone())),
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
                    "csv.read: unknown model `{}` — declared: {}",
                    self.spec.model,
                    ctx.models
                        .iter()
                        .map(|m| m.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
        let source_binding = scope.bindings.get(&self.source_ref).cloned().ok_or_else(|| {
            format!(
                "csv.read: `source: ${}` references an unbound block",
                self.source_ref
            )
        })?;

        // Encoding inheritance + divergence warning. Today only `utf-8` is
        // supported, so the warning branch is unreachable; the seam is in
        // place for a future second enum value.
        if let Some(decl) = self.declared_encoding
            && decl != ctx.project_encoding
        {
            eprintln!(
                "rublocks: csv.read `{}` declares encoding `{decl}` but project encoding is `{}` — divergence allowed but unusual",
                self.spec.name, ctx.project_encoding
            );
        }
        let effective_encoding = self.declared_encoding.unwrap_or(ctx.project_encoding);
        let encoding_label = effective_encoding.charset_label();

        let model_ident = format_ident!("{}", model.name);
        let name_ident = format_ident!("__block_{}", self.spec.name);
        let source_ident = &source_binding.ident;
        let delimiter = self.delimiter;
        let has_header = self.spec.has_header.unwrap_or(true);

        // Per-field parse expression. Each cell arrives as `&str`; we map
        // it to the model field's runtime type via the canonical parser
        // for the type (uuid::Uuid::parse_str, chrono RFC3339, …). Nullable
        // fields treat the empty string as `None`.
        let field_idents: Vec<proc_macro2::Ident> = model
            .fields
            .keys()
            .map(|k| format_ident!("{}", k))
            .collect();
        let field_names: Vec<String> = model.fields.keys().cloned().collect();
        let field_parsers: Vec<TokenStream> = model
            .fields
            .iter()
            .enumerate()
            .map(|(idx, (col, def))| emit_field_parser(idx, col, def.ty, def.nullable))
            .collect();
        let n_fields = field_names.len();

        let log_err = runtime::log_block_error(ctx.index, quote! { e });

        let tokens = quote! {
            let #name_ident: Vec<crate::models::#model_ident> = {
                let __src_bytes: &[u8] = ::std::convert::AsRef::<[u8]>::as_ref(&#source_ident);
                // Encoding gate — bytes that don't decode under the
                // declared encoding short-circuit with 400 carrying the
                // offending byte offset (dev-mode-visible).
                let __text: &str = match ::std::str::from_utf8(__src_bytes) {
                    Ok(s) => s,
                    Err(e) => {
                        let __offset = e.valid_up_to();
                        return crate::_rb_runtime::csv_decode_error(__offset, #encoding_label);
                    }
                };
                let mut __rdr = csv::ReaderBuilder::new()
                    .delimiter(#delimiter)
                    .has_headers(#has_header)
                    .from_reader(__text.as_bytes());

                // Column index map (one slot per model field). For header
                // rows: the header drives which CSV column feeds each
                // field, with missing/extra columns rejected up-front. For
                // headerless input: every field reads from its declaration
                // index, so the source must list columns in field order.
                let __field_names: [&str; #n_fields] = [#(#field_names),*];
                let mut __idx_map: [::std::option::Option<usize>; #n_fields] = [::std::option::Option::None; #n_fields];
                if #has_header {
                    let __headers = match __rdr.headers() {
                        Ok(h) => h.clone(),
                        Err(e) => {
                            #log_err
                            return crate::_rb_runtime::csv_error(e);
                        }
                    };
                    // Reject unknown header columns and duplicate entries.
                    let mut __seen: ::std::vec::Vec<&str> = ::std::vec::Vec::with_capacity(__headers.len());
                    for (__hi, __h) in __headers.iter().enumerate() {
                        if !__field_names.iter().any(|f| *f == __h) {
                            return crate::_rb_runtime::csv_header_error(format!(
                                "unknown column `{__h}` — known: {}",
                                __field_names.join(", ")
                            ));
                        }
                        if __seen.iter().any(|s| *s == __h) {
                            return crate::_rb_runtime::csv_header_error(format!(
                                "duplicate column `{__h}` in header"
                            ));
                        }
                        __seen.push(__h);
                        // Locate the field this column feeds.
                        for (__fi, __fn) in __field_names.iter().enumerate() {
                            if *__fn == __h {
                                __idx_map[__fi] = ::std::option::Option::Some(__hi);
                                break;
                            }
                        }
                    }
                    // Every model field must be present in the header.
                    for (__fi, __fn) in __field_names.iter().enumerate() {
                        if __idx_map[__fi].is_none() {
                            return crate::_rb_runtime::csv_header_error(format!(
                                "missing required column `{__fn}`"
                            ));
                        }
                    }
                } else {
                    for __i in 0..#n_fields {
                        __idx_map[__i] = ::std::option::Option::Some(__i);
                    }
                }

                let mut __out: ::std::vec::Vec<crate::models::#model_ident> = ::std::vec::Vec::new();
                for __res in __rdr.records() {
                    let __record = match __res {
                        Ok(r) => r,
                        Err(e) => {
                            let __line = e.position().map(|p| p.line()).unwrap_or(0);
                            #log_err
                            return crate::_rb_runtime::csv_row_error(
                                __line,
                                ::std::option::Option::None,
                                format!("{e}"),
                            );
                        }
                    };
                    let __line = __record.position().map(|p| p.line()).unwrap_or(0);
                    #(#field_parsers)*
                    __out.push(crate::models::#model_ident {
                        #(#field_idents),*
                    });
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

/// Emit one field-parse statement for `csv.read`. Reads the source cell
/// via the column index map, converts it to the model field's runtime
/// type, and short-circuits with `422` carrying the line number + column
/// name on failure. Nullable fields treat the empty string as `None`.
fn emit_field_parser(idx: usize, col: &str, ty: FieldType, nullable: bool) -> TokenStream {
    let field_ident = format_ident!("{}", col);
    let parse_call = emit_parse_call(ty);
    let col_name_lit = col.to_string();
    if nullable {
        let inner_ty = base_field_type(ty);
        quote! {
            let #field_ident: crate::_rb_util::NullDisplay<#inner_ty> = {
                let __raw = match __idx_map[#idx].and_then(|i| __record.get(i)) {
                    Some(s) => s,
                    None => "",
                };
                if __raw.is_empty() {
                    crate::_rb_util::NullDisplay(None)
                } else {
                    match #parse_call(__raw) {
                        Ok(v) => crate::_rb_util::NullDisplay(Some(v)),
                        Err(err) => {
                            return crate::_rb_runtime::csv_row_error(
                                __line,
                                Some(#col_name_lit.to_string()),
                                format!("{err}"),
                            );
                        }
                    }
                }
            };
        }
    } else {
        quote! {
            let #field_ident = {
                let __raw = match __idx_map[#idx].and_then(|i| __record.get(i)) {
                    Some(s) => s,
                    None => "",
                };
                match #parse_call(__raw) {
                    Ok(v) => v,
                    Err(err) => {
                        return crate::_rb_runtime::csv_row_error(
                            __line,
                            Some(#col_name_lit.to_string()),
                            format!("{err}"),
                        );
                    }
                }
            };
        }
    }
}

/// Closure / function that parses a `&str` into the model field's runtime
/// type. Returns a `Result<T, E: Display>`. The wrapping `match` in
/// [`emit_field_parser`] turns each `Err` into a 422 with the line
/// number + column name surfaced.
fn emit_parse_call(ty: FieldType) -> TokenStream {
    match ty {
        // `String::from` is infallible but we want a `Result` shape so the
        // emitted code is uniform. `Ok::<_, ::std::convert::Infallible>(...)`
        // makes the error type stable without affecting the happy path.
        FieldType::String | FieldType::Text | FieldType::Email => quote! {
            (|s: &str| -> ::std::result::Result<String, ::std::convert::Infallible> {
                Ok(s.to_string())
            })
        },
        FieldType::Int => quote! {
            (|s: &str| s.parse::<i32>())
        },
        FieldType::Bigint => quote! {
            (|s: &str| s.parse::<i64>())
        },
        FieldType::Bool => quote! {
            (|s: &str| s.parse::<bool>())
        },
        FieldType::Uuid => quote! {
            (|s: &str| uuid::Uuid::parse_str(s))
        },
        FieldType::Timestamptz => quote! {
            (|s: &str| {
                chrono::DateTime::parse_from_rfc3339(s)
                    .map(|dt| dt.with_timezone(&chrono::Utc))
            })
        },
    }
}

fn base_field_type(ty: FieldType) -> TokenStream {
    match ty {
        FieldType::Uuid => quote! { uuid::Uuid },
        FieldType::String | FieldType::Text | FieldType::Email => quote! { String },
        FieldType::Int => quote! { i32 },
        FieldType::Bigint => quote! { i64 },
        FieldType::Bool => quote! { bool },
        FieldType::Timestamptz => quote! { chrono::DateTime<chrono::Utc> },
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
    fn parses_minimal() {
        let r = raw(r#"{
            "block": "csv.read",
            "name": "imported",
            "source": "$raw",
            "model": "Post"
        }"#);
        let parsed = Kind.parse(&r).unwrap();
        assert_eq!(parsed.kind_id(), "csv.read");
        assert_eq!(parsed.name(), Some("imported"));
    }

    #[test]
    fn rejects_non_ref_source() {
        let r = raw(r#"{
            "block": "csv.read",
            "name": "x",
            "source": "raw",
            "model": "Post"
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("$<name>"), "got: {}", err.message);
    }

    #[test]
    fn rejects_multi_char_delimiter() {
        let r = raw(r#"{
            "block": "csv.read",
            "name": "x",
            "source": "$raw",
            "model": "Post",
            "delimiter": ",,"
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(
            err.message.contains("single ASCII"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_unknown_field() {
        let r = raw(r#"{
            "block": "csv.read",
            "name": "x",
            "source": "$raw",
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
    fn rejects_unsupported_encoding() {
        let r = raw(r#"{
            "block": "csv.read",
            "name": "x",
            "source": "$raw",
            "model": "Post",
            "encoding": "latin-1"
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("encoding"), "got: {}", err.message);
    }

    #[test]
    fn accepts_utf8_encoding() {
        let r = raw(r#"{
            "block": "csv.read",
            "name": "x",
            "source": "$raw",
            "model": "Post",
            "encoding": "utf-8"
        }"#);
        Kind.parse(&r).unwrap();
    }
}
