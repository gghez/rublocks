//! Tests for the renderer pipeline. Kept in a sibling file so the
//! sibling `runtime.rs` stays embed-clean for `include_str!` — the dist
//! crate gets the renderer only, never the test harness.

use super::runtime::{PageConfig, RenderError, SourceFormat, render};

#[test]
fn renders_markdown_with_heading_and_paragraph() {
    let src = "# Hello\n\nThis is a paragraph.\n";
    let page = PageConfig::default_a4();
    let bytes = render(src, SourceFormat::Markdown, &page).unwrap();
    let text = pdf_extract::extract_text_from_mem(&bytes).unwrap();
    assert!(text.contains("Hello"), "got: {text}");
    assert!(text.contains("This is a paragraph"), "got: {text}");
}

#[test]
fn renders_html_with_heading_paragraph_list() {
    let src = "<h1>Title</h1><p>Body line.</p><ul><li>one</li><li>two</li></ul>";
    let page = PageConfig::default_a4();
    let bytes = render(src, SourceFormat::Html, &page).unwrap();
    let text = pdf_extract::extract_text_from_mem(&bytes).unwrap();
    assert!(text.contains("Title"), "got: {text}");
    assert!(text.contains("Body line"), "got: {text}");
    assert!(text.contains("one"), "got: {text}");
    assert!(text.contains("two"), "got: {text}");
}

#[test]
fn empty_source_returns_error() {
    let page = PageConfig::default_a4();
    let err = render("", SourceFormat::Markdown, &page).unwrap_err();
    assert!(matches!(err, RenderError::EmptySource), "{err}");
}

#[test]
fn html_entity_decoding_round_trips() {
    let src = "<p>Tom &amp; Jerry &lt;3</p>";
    let bytes = render(src, SourceFormat::Html, &PageConfig::default_a4()).unwrap();
    let text = pdf_extract::extract_text_from_mem(&bytes).unwrap();
    assert!(text.contains("Tom & Jerry <3"), "got: {text}");
}

#[test]
fn long_paragraph_wraps_across_lines() {
    let words = "lorem ipsum dolor sit amet ".repeat(60);
    let src = format!("# Long\n\n{words}\n");
    let page = PageConfig::default_a4();
    let bytes = render(&src, SourceFormat::Markdown, &page).unwrap();
    let text = pdf_extract::extract_text_from_mem(&bytes).unwrap();
    let occurrences = text.matches("lorem ipsum").count();
    assert!(occurrences >= 5, "want >=5 occurrences, got {occurrences}: {text}");
}

#[test]
fn ordered_list_numbering_increments() {
    let src = "1. first\n2. second\n3. third\n";
    let page = PageConfig::default_a4();
    let bytes = render(src, SourceFormat::Markdown, &page).unwrap();
    let text = pdf_extract::extract_text_from_mem(&bytes).unwrap();
    for n in 1..=3 {
        assert!(text.contains(&format!("{n}.")), "want `{n}.` in: {text}");
    }
}
