//! `pdf.render` — produce a PDF byte buffer from an HTML or markdown
//! source binding.
//!
//! The block sits in the read-side family — it takes one prior binding
//! whose value is a `String` (the source) and binds the rendered PDF
//! body to `$<name>` as `bytes::Bytes`. The rendering engine is a
//! pure-Rust pipeline (`comrak` for markdown, a minimal hand-rolled HTML
//! walker, `printpdf` for layout) recorded in `docs/decisions.md`.
//!
//! See `docs/blocks/pdf.render.md`.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;
use serde_json::Value;

use super::runtime::BlockCodegenCtx;
use super::{BlockInstance, BlockKind, LogValue, RawBlock};
use crate::manifest::ManifestError;
use crate::models::Model;
use crate::value_ref::{BindingKind, ScopeBinding, ValueRef, ValueScope};

pub mod runtime;
#[cfg(test)]
mod runtime_tests;

#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub enum Tag {
    #[serde(rename = "pdf.render")]
    Tag,
}

#[derive(Debug, Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SourceFormatSpec {
    Html,
    Markdown,
}

#[derive(Debug, Deserialize, JsonSchema, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct PageSpec {
    /// Page paper. `"A4"`, `"Letter"`, or `"<W> x <H>"` with explicit
    /// units (`mm` or `in`), e.g. `"210mm x 297mm"`.
    #[serde(default)]
    pub size: Option<String>,
    /// CSS-style margin shorthand. 1, 2, 3, or 4 values, each with an
    /// explicit unit (`mm` / `in`). Default `"20mm"`.
    #[serde(default)]
    pub margin: Option<String>,
    /// Default `false`.
    #[serde(default)]
    pub landscape: Option<bool>,
}

// `block` is the serde discriminator — read by deserialization only.
#[allow(dead_code)]
#[derive(Debug, Deserialize, JsonSchema, Clone)]
#[serde(deny_unknown_fields)]
#[schemars(title = "block: pdf.render")]
pub struct Spec {
    pub block: Tag,
    /// Binding name. `$<name>` resolves to `bytes::Bytes` (PDF body).
    pub name: String,
    /// `$ref` pointing at a prior `String` binding (HTML or markdown).
    pub source: Value,
    /// Explicit source dialect — no auto-detection.
    pub source_format: SourceFormatSpec,
    #[serde(default)]
    pub page: Option<PageSpec>,
}

/// Parsed, validated page geometry as the renderer expects it. Held on
/// the [`Instance`] so codegen can lower the four mm distances into
/// constant literals without re-parsing at request time.
#[derive(Debug, Clone, Copy)]
pub struct PageConfig {
    pub width_mm: f32,
    pub height_mm: f32,
    /// CSS order: top, right, bottom, left.
    pub margin: [f32; 4],
}

impl Default for PageConfig {
    fn default() -> Self {
        Self {
            width_mm: 210.0,
            height_mm: 297.0,
            margin: [20.0, 20.0, 20.0, 20.0],
        }
    }
}

pub struct Kind;

impl BlockKind for Kind {
    fn id(&self) -> &'static str {
        "pdf.render"
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
        let source_ref = ValueRef::parse(&spec.source).map_err(|e| {
            raw.validation_error(format!("`source` must be a $ref / literal — {e}"))
        })?;
        let page = match spec.page.as_ref() {
            Some(p) => resolve_page(p, raw)?,
            None => PageConfig::default(),
        };
        Ok(Box::new(Instance {
            spec,
            source_ref,
            page,
        }))
    }
}

fn resolve_page(p: &PageSpec, raw: &RawBlock) -> Result<PageConfig, ManifestError> {
    let mut cfg = PageConfig::default();
    if let Some(size) = p.size.as_deref() {
        let (w, h) = parse_size(size)
            .map_err(|m| raw.validation_error(format!("`page.size`: {m} (got `{size}`)")))?;
        cfg.width_mm = w;
        cfg.height_mm = h;
    }
    if let Some(margin) = p.margin.as_deref() {
        cfg.margin = parse_margin(margin)
            .map_err(|m| raw.validation_error(format!("`page.margin`: {m} (got `{margin}`)")))?;
    }
    if p.landscape.unwrap_or(false) {
        std::mem::swap(&mut cfg.width_mm, &mut cfg.height_mm);
    }
    Ok(cfg)
}

/// Parse `"A4"`, `"Letter"`, or `"<W> x <H>"` with explicit units —
/// returns the dimensions in millimetres.
fn parse_size(s: &str) -> Result<(f32, f32), String> {
    let lower = s.trim().to_ascii_lowercase();
    match lower.as_str() {
        "a4" => return Ok((210.0, 297.0)),
        "letter" => return Ok((215.9, 279.4)),
        _ => {}
    }
    let mut parts = lower.split('x');
    let w_str = parts
        .next()
        .ok_or_else(|| "expected `<W> x <H>` with explicit units".to_string())?;
    let h_str = parts
        .next()
        .ok_or_else(|| "expected `<W> x <H>` with explicit units".to_string())?;
    if parts.next().is_some() {
        return Err("expected exactly one `x` separator".to_string());
    }
    let w_mm = parse_length(w_str.trim())?;
    let h_mm = parse_length(h_str.trim())?;
    Ok((w_mm, h_mm))
}

/// Parse a CSS-style margin shorthand into 4 sides in mm (top, right,
/// bottom, left).
fn parse_margin(s: &str) -> Result<[f32; 4], String> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.is_empty() || parts.len() > 4 {
        return Err("expected 1..=4 whitespace-separated values".to_string());
    }
    let values: Vec<f32> = parts.iter().map(|p| parse_length(p)).collect::<Result<_, _>>()?;
    let m = match values.len() {
        1 => [values[0]; 4],
        2 => [values[0], values[1], values[0], values[1]],
        3 => [values[0], values[1], values[2], values[1]],
        4 => [values[0], values[1], values[2], values[3]],
        _ => unreachable!(),
    };
    Ok(m)
}

/// Parse `"20mm"` / `"1in"` — returns millimetres. Bare numbers are
/// rejected on purpose: explicit units sit closer to "one canonical
/// form per concept" than guessing pixels.
fn parse_length(s: &str) -> Result<f32, String> {
    let s = s.trim();
    if let Some(num) = s.strip_suffix("mm") {
        let n: f32 = num
            .trim()
            .parse()
            .map_err(|_| format!("invalid number `{num}` in length"))?;
        if n.is_nan() || n.is_infinite() || n < 0.0 {
            return Err(format!("length must be a finite non-negative number (got `{s}`)"));
        }
        Ok(n)
    } else if let Some(num) = s.strip_suffix("in") {
        let n: f32 = num
            .trim()
            .parse()
            .map_err(|_| format!("invalid number `{num}` in length"))?;
        if n.is_nan() || n.is_infinite() || n < 0.0 {
            return Err(format!("length must be a finite non-negative number (got `{s}`)"));
        }
        Ok(n * 25.4)
    } else {
        Err(format!("explicit unit required (`mm` or `in`), got `{s}`"))
    }
}

#[derive(Debug)]
pub struct Instance {
    spec: Spec,
    source_ref: ValueRef,
    page: PageConfig,
}

impl BlockInstance for Instance {
    fn kind_id(&self) -> &'static str {
        "pdf.render"
    }

    fn name(&self) -> Option<&str> {
        Some(&self.spec.name)
    }

    fn output_type(&self, _models: &[Model]) -> Option<TokenStream> {
        Some(quote! { bytes::Bytes })
    }

    fn log_fields(&self) -> Vec<(&'static str, LogValue)> {
        let fmt = match self.spec.source_format {
            SourceFormatSpec::Html => "html",
            SourceFormatSpec::Markdown => "markdown",
        };
        vec![
            ("source_format", LogValue::Str(fmt.to_string())),
            ("page_width_mm", LogValue::Int(self.page.width_mm as i64)),
            ("page_height_mm", LogValue::Int(self.page.height_mm as i64)),
        ]
    }

    fn emit_code(
        &self,
        ctx: &BlockCodegenCtx,
        scope: &mut ValueScope,
    ) -> Result<TokenStream, String> {
        let name_ident = format_ident!("__block_{}", self.spec.name);
        let source_expr = self
            .source_ref
            .emit_expr(scope)
            .map_err(|e| format!("pdf.render: {e}"))?;
        let source_tokens = source_expr.expr;
        let format_variant = match self.spec.source_format {
            SourceFormatSpec::Html => quote! { Html },
            SourceFormatSpec::Markdown => quote! { Markdown },
        };
        let width = self.page.width_mm;
        let height = self.page.height_mm;
        let m_top = self.page.margin[0];
        let m_right = self.page.margin[1];
        let m_bottom = self.page.margin[2];
        let m_left = self.page.margin[3];
        let log_err = super::runtime::log_block_error_message(ctx.index, quote! { __rb_msg });

        let tokens = quote! {
            let #name_ident: bytes::Bytes = {
                let __src_owned: String = (#source_tokens).to_string();
                let __page = crate::_rb_pdf::PageConfig {
                    width_mm: #width,
                    height_mm: #height,
                    margin: [#m_top, #m_right, #m_bottom, #m_left],
                };
                match crate::_rb_pdf::render(
                    &__src_owned,
                    crate::_rb_pdf::SourceFormat::#format_variant,
                    &__page,
                ) {
                    Ok(b) => bytes::Bytes::from(b),
                    Err(e) => {
                        let __rb_msg: String = format!("{e}");
                        #log_err
                        return axum::response::IntoResponse::into_response((
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            format!("rublocks: {}", __rb_msg),
                        ));
                    }
                }
            };
        };

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
    use indexmap::IndexMap;
    use serde_json::Value;
    use std::path::PathBuf;

    fn fake_path() -> PathBuf {
        PathBuf::from("/fake/route.json")
    }

    fn raw(body: &str) -> RawBlock {
        let v: Value = serde_json::from_str(body).unwrap();
        RawBlock::from_value(&v, &fake_path(), "process[0]").unwrap()
    }

    fn minimal_raw() -> RawBlock {
        RawBlock {
            block: "pdf.render".to_string(),
            fields: IndexMap::new(),
            source: fake_path(),
            label: "process[0]".to_string(),
        }
    }

    #[test]
    fn parses_canonical_form() {
        let r = raw(r#"{
            "block": "pdf.render",
            "name": "invoice_pdf",
            "source": "$invoice_html",
            "source_format": "html",
            "page": { "size": "A4", "margin": "20mm", "landscape": false }
        }"#);
        let inst = Kind.parse(&r).unwrap();
        assert_eq!(inst.kind_id(), "pdf.render");
        assert_eq!(inst.name(), Some("invoice_pdf"));
    }

    #[test]
    fn requires_source_format() {
        let r = raw(r#"{
            "block": "pdf.render",
            "name": "pdf",
            "source": "$body"
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("source_format"), "got: {}", err.message);
    }

    #[test]
    fn rejects_invalid_source_format() {
        let r = raw(r#"{
            "block": "pdf.render",
            "name": "pdf",
            "source": "$body",
            "source_format": "rtf"
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(
            err.message.contains("source_format") || err.message.contains("unknown variant"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_unknown_field() {
        let r = raw(r#"{
            "block": "pdf.render",
            "name": "pdf",
            "source": "$body",
            "source_format": "html",
            "junk": true
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("unknown field"), "got: {}", err.message);
    }

    #[test]
    fn rejects_invalid_page_size() {
        let r = raw(r#"{
            "block": "pdf.render",
            "name": "pdf",
            "source": "$body",
            "source_format": "html",
            "page": { "size": "wat" }
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("page.size"), "got: {}", err.message);
    }

    #[test]
    fn rejects_size_without_units() {
        let r = raw(r#"{
            "block": "pdf.render",
            "name": "pdf",
            "source": "$body",
            "source_format": "html",
            "page": { "size": "210 x 297" }
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("page.size"), "got: {}", err.message);
    }

    #[test]
    fn parses_custom_size_with_units() {
        let r = raw(r#"{
            "block": "pdf.render",
            "name": "pdf",
            "source": "$body",
            "source_format": "html",
            "page": { "size": "210mm x 297mm" }
        }"#);
        Kind.parse(&r).unwrap();
    }

    #[test]
    fn landscape_swaps_dimensions() {
        let r = raw(r#"{
            "block": "pdf.render",
            "name": "pdf",
            "source": "$body",
            "source_format": "html",
            "page": { "size": "A4", "landscape": true }
        }"#);
        let inst = Kind.parse(&r).unwrap();
        // We can't downcast through the trait — rely on log_fields,
        // which we asserted reports the page geometry.
        let fields = inst.log_fields();
        let map: std::collections::HashMap<_, _> = fields.into_iter().collect();
        let w = match map["page_width_mm"] {
            LogValue::Int(n) => n,
            _ => panic!(),
        };
        let h = match map["page_height_mm"] {
            LogValue::Int(n) => n,
            _ => panic!(),
        };
        // A4 landscape: width 297, height 210.
        assert_eq!((w, h), (297, 210));
    }

    #[test]
    fn parses_margin_shorthand_four_values() {
        let r = raw(r#"{
            "block": "pdf.render",
            "name": "pdf",
            "source": "$body",
            "source_format": "html",
            "page": { "margin": "10mm 20mm 15mm 25mm" }
        }"#);
        Kind.parse(&r).unwrap();
    }

    #[test]
    fn rejects_margin_without_units() {
        let r = raw(r#"{
            "block": "pdf.render",
            "name": "pdf",
            "source": "$body",
            "source_format": "html",
            "page": { "margin": "20" }
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("page.margin"), "got: {}", err.message);
    }

    #[test]
    fn rejects_empty_name() {
        let mut r = raw(r#"{
            "block": "pdf.render",
            "name": "x",
            "source": "$body",
            "source_format": "html"
        }"#);
        r.fields.insert("name".to_string(), Value::String("   ".to_string()));
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("name"), "got: {}", err.message);
    }

    #[test]
    fn block_does_not_break_on_missing_block_discriminator() {
        // Smoke: bare RawBlock without our discriminator is rejected by
        // serde with a clear field message.
        let r = minimal_raw();
        let err = Kind.parse(&r).unwrap_err();
        assert!(
            err.message.contains("missing field") || err.message.contains("source"),
            "got: {}",
            err.message
        );
    }
}
