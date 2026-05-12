//! `csv.write` — turn a `Vec<crate::models::T>` into a CSV byte buffer.
//!
//! Write-side counterpart of [`super::csv_read`]. Takes a `$ref` pointing
//! at an iterable bound earlier in the pipeline (typically a
//! `db.find_many` result) and binds the encoded CSV body to `$<name>` as
//! `bytes::Bytes`. The headers default to the model's field order; an
//! explicit `headers` array restricts and re-orders the columns.
//!
//! See `docs/blocks/csv.write.md` and `docs/encoding.md` for the
//! project-wide encoding contract this block inherits.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;
use serde_json::Value;

use super::runtime::BlockCodegenCtx;
use super::{BlockInstance, BlockKind, LogValue, RawBlock, model_for_table};
use crate::manifest::ManifestError;
use crate::models::Model;
use crate::value_ref::{BindingKind, ScopeBinding, ValueScope};

#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub enum Tag {
    #[serde(rename = "csv.write")]
    Tag,
}

/// Quoting policy for the CSV writer. Mirrors `csv::QuoteStyle` 1:1 — kept
/// as a project-local enum so the manifest spelling is declarative and the
/// dependency on the `csv` crate is not exposed at the parse layer.
#[derive(Debug, Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Quoting {
    #[default]
    Necessary,
    Always,
    Never,
}

// `block` is a serde discriminator — consumed by deserialization only.
#[allow(dead_code)]
#[derive(Debug, Deserialize, JsonSchema, Clone)]
#[serde(deny_unknown_fields)]
#[schemars(title = "block: csv.write")]
pub struct Spec {
    pub block: Tag,
    /// Binding name. `$<name>` resolves to `bytes::Bytes` for downstream
    /// blocks and `view` / `output`.
    pub name: String,
    /// `$<binding>` reference to a prior `Vec<crate::models::T>` (today
    /// only `db.find_many` produces this shape).
    pub rows: Value,
    /// Optional explicit column list. When omitted, every model field is
    /// emitted in declaration order. When provided, only those columns
    /// are emitted (in that order). Every entry must match a model field.
    #[serde(default)]
    pub headers: Option<Vec<String>>,
    /// Single-byte CSV delimiter. Default `,`.
    #[serde(default)]
    pub delimiter: Option<String>,
    /// Quoting policy. Default `necessary` — every value is quoted only
    /// when it would otherwise be ambiguous.
    #[serde(default)]
    pub quoting: Option<Quoting>,
    /// Character encoding for the emitted bytes. When omitted, inherits
    /// the project-wide `main.json.encoding`. Today only `"utf-8"` is
    /// accepted; any other value fails at build time.
    #[serde(default)]
    pub encoding: Option<String>,
}

pub struct Kind;

impl BlockKind for Kind {
    fn id(&self) -> &'static str {
        "csv.write"
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
        let rows_ref = parse_rows_ref(&spec.rows, raw)?;
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
            rows_ref,
            delimiter,
            declared_encoding,
        }))
    }
}

/// `rows` accepts a single `$<binding>` form. Anything else (including a
/// literal CSV-row array) is rejected at load time so the rest of the
/// pipeline has one shape to reason about.
fn parse_rows_ref(value: &Value, raw: &RawBlock) -> Result<String, ManifestError> {
    match value {
        Value::String(s) => match s.strip_prefix('$') {
            Some(rest) if !rest.is_empty() && !rest.contains('.') => Ok(rest.to_string()),
            _ => Err(raw.validation_error(format!(
                "`rows` must reference a prior block as `$<name>` (got `{s}`)"
            ))),
        },
        _ => Err(raw.validation_error("`rows` must be a `$<name>` string")),
    }
}

/// Single-byte ASCII delimiter, default `,`. Multi-character or non-ASCII
/// values are rejected at load time so emission can rely on a known byte.
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
    rows_ref: String,
    delimiter: u8,
    declared_encoding: Option<crate::manifest::Encoding>,
}

impl BlockInstance for Instance {
    fn kind_id(&self) -> &'static str {
        "csv.write"
    }

    fn name(&self) -> Option<&str> {
        Some(&self.spec.name)
    }

    fn output_type(&self, _models: &[Model]) -> Option<TokenStream> {
        Some(quote! { bytes::Bytes })
    }

    fn log_fields(&self) -> Vec<(&'static str, LogValue)> {
        vec![("rows_ref", LogValue::Str(self.rows_ref.clone()))]
    }

    fn emit_code(
        &self,
        ctx: &BlockCodegenCtx,
        scope: &mut ValueScope,
    ) -> Result<TokenStream, String> {
        // Resolve `rows` against the running scope. We expect a `FindMany`
        // binding so we can look up the model and project its fields into
        // CSV columns. Anything else is a load-time rejection.
        let binding = scope.bindings.get(&self.rows_ref).cloned().ok_or_else(|| {
            format!(
                "csv.write: `rows: ${}` references an unbound block",
                self.rows_ref
            )
        })?;
        let table = match &binding.kind {
            BindingKind::FindMany { table } => table.clone(),
            _ => {
                return Err(format!(
                    "csv.write: `rows: ${}` must reference a `db.find_many` binding (Vec<T>)",
                    self.rows_ref
                ));
            }
        };
        let model = model_for_table(ctx.models, &table)
            .ok_or_else(|| format!("csv.write: no model declares table `{table}`"))?;

        // Validate explicit headers against the model fields. Unknown columns
        // become a manifest-level error so the user never reaches a runtime
        // surprise (issue #18 acceptance).
        let header_cols: Vec<String> = if let Some(hs) = self.spec.headers.as_ref() {
            for col in hs {
                if !model.fields.contains_key(col) {
                    return Err(format!(
                        "csv.write: unknown column `{col}` for model `{}` — known: {}",
                        model.name,
                        model.fields.keys().cloned().collect::<Vec<_>>().join(", ")
                    ));
                }
            }
            hs.clone()
        } else {
            model.fields.keys().cloned().collect()
        };

        // Encoding inheritance + warning. Today only `utf-8` is supported, so
        // the warning branch is dead in practice; the seam is in place for a
        // future second enum value.
        if let Some(decl) = self.declared_encoding
            && decl != ctx.project_encoding
        {
            eprintln!(
                "rublocks: csv.write `{}` declares encoding `{decl}` but project encoding is `{}` — divergence allowed but unusual",
                self.spec.name, ctx.project_encoding
            );
        }

        let name_ident = format_ident!("__block_{}", self.spec.name);
        let rows_ident = &binding.ident;
        let delimiter = self.delimiter;
        let quote_style = match self.spec.quoting.unwrap_or_default() {
            Quoting::Necessary => quote! { csv::QuoteStyle::Necessary },
            Quoting::Always => quote! { csv::QuoteStyle::Always },
            Quoting::Never => quote! { csv::QuoteStyle::Never },
        };
        let header_literals: Vec<TokenStream> =
            header_cols.iter().map(|c| quote! { #c }).collect();
        let field_exprs: Vec<TokenStream> = header_cols
            .iter()
            .map(|col| {
                let f = format_ident!("{}", col);
                // `NullDisplay<T>` already renders nullable fields as an empty
                // string for `None`; non-nullable fields format through their
                // own `Display`. One uniform shape keeps the writer call site
                // narrow.
                quote! { format!("{}", __row.#f) }
            })
            .collect();

        let log_err = super::runtime::log_block_error(ctx.index, quote! { e });

        let tokens = quote! {
            let #name_ident: bytes::Bytes = {
                let mut __wtr = csv::WriterBuilder::new()
                    .delimiter(#delimiter)
                    .quote_style(#quote_style)
                    .has_headers(false)
                    .from_writer(::std::vec::Vec::<u8>::new());
                let __header: ::std::vec::Vec<&str> = ::std::vec![#(#header_literals),*];
                if let Err(e) = __wtr.write_record(__header.iter().copied()) {
                    #log_err
                    return crate::_rb_runtime::csv_error(e);
                }
                for __row in #rows_ident.iter() {
                    let __record: ::std::vec::Vec<String> = ::std::vec![#(#field_exprs),*];
                    if let Err(e) = __wtr.write_record(__record.iter()) {
                        #log_err
                        return crate::_rb_runtime::csv_error(e);
                    }
                }
                // Writing to `Vec<u8>` never returns `io::Error`; `expect` is
                // safe and keeps the happy-path tokens compact. Any csv-level
                // serialization error already surfaced on a prior write call.
                let __buf: ::std::vec::Vec<u8> = __wtr.into_inner().expect("csv writer over Vec<u8> cannot fail");
                bytes::Bytes::from(__buf)
            };
        };

        // Bind the produced bytes for downstream blocks and the response
        // builder. Scalar binding kind so view/output emit it directly.
        scope.bindings.insert(
            self.spec.name.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fake_path() -> PathBuf {
        PathBuf::from("/fake/route.json")
    }

    fn raw(body: &str) -> RawBlock {
        let v: Value = serde_json::from_str(body).unwrap();
        RawBlock::from_value(&v, &fake_path(), "process[1]").unwrap()
    }

    #[test]
    fn parses_minimal() {
        let r = raw(r#"{
            "block": "csv.write",
            "name": "posts_csv",
            "rows": "$posts"
        }"#);
        let parsed = Kind.parse(&r).unwrap();
        assert_eq!(parsed.kind_id(), "csv.write");
        assert_eq!(parsed.name(), Some("posts_csv"));
    }

    #[test]
    fn rejects_non_ref_rows() {
        let r = raw(r#"{
            "block": "csv.write",
            "name": "x",
            "rows": "posts"
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("$<name>"), "got: {}", err.message);
    }

    #[test]
    fn rejects_multi_char_delimiter() {
        let r = raw(r#"{
            "block": "csv.write",
            "name": "x",
            "rows": "$posts",
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
            "block": "csv.write",
            "name": "x",
            "rows": "$posts",
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
            "block": "csv.write",
            "name": "x",
            "rows": "$posts",
            "encoding": "latin-1"
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("encoding"), "got: {}", err.message);
    }

    #[test]
    fn accepts_utf8_encoding() {
        let r = raw(r#"{
            "block": "csv.write",
            "name": "x",
            "rows": "$posts",
            "encoding": "utf-8"
        }"#);
        Kind.parse(&r).unwrap();
    }
}
