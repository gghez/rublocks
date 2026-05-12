//! `docx.render` — render an HTML or markdown source string to a DOCX
//! byte buffer.
//!
//! Same shape as the planned `pdf.render`: one `source` reference plus
//! an explicit `source_format`, producing `bytes::Bytes` bound to
//! `$<name>`. The conversion logic lives in `docx_runtime` so the
//! exact same code path runs in the compiler's tests and in every
//! generated dist project.
//!
//! See `docs/blocks/docx.render.md` for the supported HTML/markdown
//! subset and the dev-mode error surface.

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

#[derive(Debug, Deserialize, JsonSchema, Clone)]
pub enum Tag {
    #[serde(rename = "docx.render")]
    Tag,
}

#[derive(Debug, Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SourceFormat {
    Html,
    Markdown,
}

impl SourceFormat {
    fn as_str(self) -> &'static str {
        match self {
            SourceFormat::Html => "html",
            SourceFormat::Markdown => "markdown",
        }
    }
}

/// Raw shape of a `docx.render` block.
// `block` is the serde discriminator — consumed by deserialization only.
#[allow(dead_code)]
#[derive(Debug, Deserialize, JsonSchema, Clone)]
#[serde(deny_unknown_fields)]
#[schemars(title = "block: docx.render")]
pub struct Spec {
    pub block: Tag,
    /// Binding name. `$<name>` resolves to `bytes::Bytes` (DOCX body).
    pub name: String,
    /// Reference to a prior `String` binding holding the HTML or
    /// markdown source. Always written as `$<name>` or `$<name>.<field>`.
    pub source: Value,
    /// Explicit source format — no default, the author always names it
    /// at the call site. `"html"` or `"markdown"`.
    pub source_format: SourceFormat,
}

pub struct Kind;

impl BlockKind for Kind {
    fn id(&self) -> &'static str {
        "docx.render"
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
        let source = ValueRef::parse(&spec.source)
            .map_err(|e| raw.validation_error(format!("`source`: {e}")))?;
        // Literal source values would be a JSON string baked into the
        // manifest — a real source body belongs in a file or a prior
        // block. Reject literals so authors don't accidentally try to
        // embed multi-line markdown in JSON.
        if matches!(&source, ValueRef::Literal(_)) {
            return Err(raw.validation_error(
                "`source` must be a `$ref` to a prior `String` binding (use sftp.read + a text \
                 conversion block, or chain another block that builds the body)",
            ));
        }
        Ok(Box::new(Instance { spec, source }))
    }
}

#[derive(Debug)]
pub struct Instance {
    spec: Spec,
    source: ValueRef,
}

impl BlockInstance for Instance {
    fn kind_id(&self) -> &'static str {
        "docx.render"
    }

    fn name(&self) -> Option<&str> {
        Some(&self.spec.name)
    }

    fn output_type(&self, _models: &[Model]) -> Option<TokenStream> {
        Some(quote! { bytes::Bytes })
    }

    fn log_fields(&self) -> Vec<(&'static str, LogValue)> {
        vec![(
            "source_format",
            LogValue::Str(self.spec.source_format.as_str().to_string()),
        )]
    }

    fn emit_code(
        &self,
        ctx: &BlockCodegenCtx,
        scope: &mut ValueScope,
    ) -> Result<TokenStream, String> {
        let name_ident = format_ident!("__block_{}", self.spec.name);
        let emitted = self.source.emit_expr(scope)?;
        let source_expr = emitted.expr;
        let format_variant = match self.spec.source_format {
            SourceFormat::Html => quote! { crate::_rb_docx::SourceFormat::Html },
            SourceFormat::Markdown => quote! { crate::_rb_docx::SourceFormat::Markdown },
        };
        let log_err = super::runtime::log_block_error(ctx.index, quote! { e });
        let tokens = quote! {
            let #name_ident: bytes::Bytes = {
                let __src: String = (#source_expr).to_string();
                match crate::_rb_docx::render(&__src, #format_variant) {
                    Ok(b) => b,
                    Err(e) => {
                        #log_err
                        return crate::_rb_runtime::docx_error(e);
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

/// True when at least one route's `process` contains a `docx.render`
/// block. Drives the conditional emission of `docx-rs`,
/// `pulldown-cmark`, `html5ever`, and `markup5ever_rcdom` in the dist
/// `Cargo.toml`, and the `_rb_docx` module in the dist `main.rs`.
pub fn project_uses_docx_render(routes: &[crate::routes::Route]) -> bool {
    routes
        .iter()
        .any(|r| r.process.iter().any(|b| b.kind_id() == "docx.render"))
}

#[cfg(test)]
#[path = "docx_runtime.rs"]
mod docx_runtime;

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
    fn parses_canonical_markdown_form() {
        let r = raw(r#"{
            "block": "docx.render",
            "name": "report_docx",
            "source": "$report_md",
            "source_format": "markdown"
        }"#);
        let parsed = Kind.parse(&r).unwrap();
        assert_eq!(parsed.kind_id(), "docx.render");
        assert_eq!(parsed.name(), Some("report_docx"));
    }

    #[test]
    fn parses_canonical_html_form() {
        let r = raw(r#"{
            "block": "docx.render",
            "name": "report_docx",
            "source": "$report_html",
            "source_format": "html"
        }"#);
        Kind.parse(&r).unwrap();
    }

    #[test]
    fn rejects_unknown_field() {
        let r = raw(r#"{
            "block": "docx.render",
            "name": "report_docx",
            "source": "$report_md",
            "source_format": "markdown",
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
    fn rejects_missing_source_format() {
        // The schema has no default for `source_format` — every call
        // site names the format explicitly.
        let r = raw(r#"{
            "block": "docx.render",
            "name": "report_docx",
            "source": "$report_md"
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(
            err.message.contains("source_format"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_unknown_source_format() {
        let r = raw(r#"{
            "block": "docx.render",
            "name": "report_docx",
            "source": "$report_md",
            "source_format": "pdf"
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        // serde reports the bad variant with the catalogue, not the
        // field name — match on the variant string so authors see the
        // exact value they typed.
        assert!(
            err.message.contains("`pdf`")
                && err.message.contains("`html`")
                && err.message.contains("`markdown`"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_literal_source() {
        // Multi-line markdown inside JSON is a footgun — surface the
        // restriction at load time so authors are pushed toward a prior
        // block (sftp.read / time.now / a future text-source block).
        let r = raw(r#"{
            "block": "docx.render",
            "name": "report_docx",
            "source": "literal text",
            "source_format": "markdown"
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("$ref"), "got: {}", err.message);
    }

    #[test]
    fn rejects_empty_name() {
        let r = raw(r#"{
            "block": "docx.render",
            "name": "",
            "source": "$report_md",
            "source_format": "markdown"
        }"#);
        let err = Kind.parse(&r).unwrap_err();
        assert!(err.message.contains("name"), "got: {}", err.message);
    }

    #[test]
    fn project_uses_docx_render_detects_block() {
        // Wire only enough state to exercise the helper — a manifest
        // with one route, one process block, that block's kind_id ==
        // "docx.render". `Route` carries a lot of fields irrelevant to
        // this check, so we use `..Default::default()` rather than name
        // each one — the helper only reads `process`.
        let r = raw(r#"{
            "block": "docx.render",
            "name": "report_docx",
            "source": "$report_md",
            "source_format": "markdown"
        }"#);
        let inst = Kind.parse(&r).unwrap();
        let route = crate::routes::Route {
            source: fake_path(),
            name: "r".to_string(),
            path: "/r".to_string(),
            method: crate::routes::HttpMethod::Get,
            kind: crate::routes::RouteKind::Api,
            template: None,
            layout: None,
            process: vec![inst],
            view: indexmap::IndexMap::new(),
            input: None,
            output: None,
            redirect: None,
        };
        assert!(project_uses_docx_render(std::slice::from_ref(&route)));
    }

    // ---------------- Round-trip tests against the runtime ----------------
    //
    // These exercise `docx_runtime::render` with a small markdown and
    // HTML source, re-open the produced zip via `docx-rs`'s reader, and
    // assert that every textual fragment from the input survives the
    // round-trip. Per the issue spec, byte-equality is not stable across
    // runs (timestamps inside the docx zip), so we compare on text.

    use docx_runtime::{DocxError, SourceFormat as RtFormat, render};

    fn extract_all_text(bytes: &[u8]) -> String {
        // `docx-rs` exposes a reader that returns a `Docx`; we walk
        // every paragraph/table and concatenate the run texts. Keep
        // the extractor here so the test owns the contract and is not
        // tied to dist-side helpers.
        let docx = docx_rs::read_docx(bytes).expect("re-parse generated docx");
        let mut buf = String::new();
        for child in docx.document.children.iter() {
            walk_doc_child(child, &mut buf);
        }
        buf
    }

    fn walk_doc_child(child: &docx_rs::DocumentChild, buf: &mut String) {
        match child {
            docx_rs::DocumentChild::Paragraph(p) => {
                push_paragraph_text(p, buf);
                buf.push('\n');
            }
            docx_rs::DocumentChild::Table(t) => {
                for row in t.rows.iter() {
                    let docx_rs::TableChild::TableRow(r) = row;
                    for cell in r.cells.iter() {
                        let docx_rs::TableRowChild::TableCell(c) = cell;
                        for ch in c.children.iter() {
                            if let docx_rs::TableCellContent::Paragraph(p) = ch {
                                push_paragraph_text(p, buf);
                                buf.push('\t');
                            }
                        }
                    }
                    buf.push('\n');
                }
            }
            _ => {}
        }
    }

    fn push_paragraph_text(p: &docx_rs::Paragraph, buf: &mut String) {
        for c in p.children.iter() {
            if let docx_rs::ParagraphChild::Run(r) = c {
                for rc in r.children.iter() {
                    if let docx_rs::RunChild::Text(t) = rc {
                        buf.push_str(&t.text);
                    }
                }
            }
        }
    }

    #[test]
    fn round_trip_markdown_preserves_text() {
        let src = "# Title\n\nA **bold** paragraph with *emphasis*.\n\n- one\n- two\n";
        let bytes = render(src, RtFormat::Markdown).expect("markdown renders");
        let text = extract_all_text(&bytes);
        assert!(text.contains("Title"), "missing Title: {text:?}");
        assert!(text.contains("bold"), "missing bold: {text:?}");
        assert!(text.contains("emphasis"), "missing emphasis: {text:?}");
        assert!(text.contains("one"), "missing list item one: {text:?}");
        assert!(text.contains("two"), "missing list item two: {text:?}");
    }

    #[test]
    fn round_trip_html_preserves_text_and_table() {
        let src = "<h2>Report</h2><p>Hello <strong>world</strong>.</p>\
                   <table><tr><th>k</th><th>v</th></tr><tr><td>a</td><td>1</td></tr></table>";
        let bytes = render(src, RtFormat::Html).expect("html renders");
        let text = extract_all_text(&bytes);
        assert!(text.contains("Report"));
        assert!(text.contains("Hello"));
        assert!(text.contains("world"));
        // Table header + data cells are present.
        assert!(text.contains('k') && text.contains('v'));
        assert!(text.contains('a') && text.contains('1'));
    }

    #[test]
    fn unsupported_tag_returns_named_error() {
        let src = "<p>Hi</p><img src=\"x.png\">";
        let err = render(src, RtFormat::Html).expect_err("img is unsupported in v1");
        match err {
            DocxError::Unsupported { tag } => assert_eq!(tag, "img"),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_tag_in_markdown_surfaces_inline_html() {
        // Inline HTML in markdown still flows through the same walker,
        // so the unsupported error names the actual HTML tag.
        let src = "Some text <svg></svg>";
        let err = render(src, RtFormat::Markdown).expect_err("svg is unsupported in v1");
        match err {
            DocxError::Unsupported { tag } => assert_eq!(tag, "svg"),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }
}
