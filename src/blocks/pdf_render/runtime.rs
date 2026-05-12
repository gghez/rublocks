//! Runtime renderer for the `pdf.render` block — pure Rust, no external
//! process, no headless browser. Single source of truth: this file is
//! compiled by rublocks (for unit tests) and embedded verbatim into the
//! generated dist crate's `_rb_pdf` module via `include_str!`.
//!
//! The renderer keeps two non-negotiables in scope:
//! - **No external runtime** — rules out `weasyprint` and any
//!   browser-backed engine (recorded in `docs/decisions.md`).
//! - **Bounded dependency surface** — rules out the `typst` family,
//!   which would pull in a font shaper, math typesetter, and several MB
//!   of layout machinery for a v1 that only needs paragraphs, headings
//!   and lists.
//!
//! The implementation flow:
//!
//! 1. Source string → [`RenderBlock`] list. Markdown is parsed via
//!    `comrak` and walked; HTML goes through a minimal hand-rolled
//!    tag-aware tokenizer (the supported subset is documented in
//!    `docs/blocks/pdf.render.md`).
//! 2. Blocks are laid out top-to-bottom onto pages with `printpdf`,
//!    using built-in Helvetica so no font file is bundled. Line wrap
//!    is word-aware with a width estimate that errs on the side of
//!    fitting a character or two extra — slightly tight is preferable
//!    to a stray word crawling off the page.

// Every item in this module is either driven by the unit tests below
// or appears in the generated dist code only — both paths look "dead"
// to the rublocks-side compiler. Silence the lint here rather than
// scattering attributes over every item.
#![allow(dead_code)]

use comrak::nodes::{AstNode, ListType, NodeValue};
use comrak::{Arena, Options, parse_document};
use printpdf::{
    BuiltinFont, Color, Mm, Op, PdfDocument, PdfFontHandle, PdfPage, PdfSaveOptions, Point, Pt,
    Rgb, TextItem,
};

/// Discriminator for the two source dialects the block accepts. Selected
/// at the manifest level via `source_format` — no auto-detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    Html,
    Markdown,
}

/// Resolved page geometry. All distances in millimetres.
#[derive(Debug, Clone, Copy)]
pub struct PageConfig {
    pub width_mm: f32,
    pub height_mm: f32,
    /// CSS-style four sides — top, right, bottom, left — in mm.
    pub margin: [f32; 4],
}

impl PageConfig {
    /// A4 portrait with 20mm margins — the default emitted when the
    /// manifest omits `page`.
    pub fn default_a4() -> Self {
        Self {
            width_mm: 210.0,
            height_mm: 297.0,
            margin: [20.0, 20.0, 20.0, 20.0],
        }
    }
}

/// Errors surfaced by [`render`]. The dev-mode 500 page consumes the
/// `Display` form; structured chains (`source().chain`) are skipped on
/// purpose — every variant is a leaf with the user-relevant context
/// already inlined.
#[derive(Debug)]
pub enum RenderError {
    /// Source string was empty (or stripped down to no blocks).
    EmptySource,
    /// Catch-all for the underlying engine — wraps the `printpdf` /
    /// I/O message verbatim so the dev overlay shows it as-is.
    Engine(String),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::EmptySource => write!(f, "pdf.render: source produced no content"),
            RenderError::Engine(m) => write!(f, "pdf.render: {m}"),
        }
    }
}

impl std::error::Error for RenderError {}

/// One layout block produced by the source-parser stage. Kept tiny on
/// purpose — the renderer maps a small flat list of these onto pages.
#[derive(Debug, Clone)]
pub enum RenderBlock {
    /// `level` is 1..=6 — clamped on parse so the font-size lookup is
    /// total.
    Heading { level: u8, text: String },
    Paragraph(String),
    /// Bullet list item with an unparsed bullet glyph (`•`).
    BulletItem(String),
    /// Numbered list item. The number is held externally so renumbering
    /// across nested lists stays linear.
    NumberedItem { n: u32, text: String },
    /// Pre-formatted text. No wrapping inside lines — wrapping happens
    /// at character boundaries.
    CodeBlock(String),
}

/// Render `source` into a PDF byte buffer.
///
/// Returns `Err(EmptySource)` when the source contains no recognised
/// blocks — easier to surface a clear error than to ship a blank PDF
/// the user has to debug from a viewer.
pub fn render(source: &str, format: SourceFormat, page: &PageConfig) -> Result<Vec<u8>, RenderError> {
    let blocks = match format {
        SourceFormat::Markdown => parse_markdown(source),
        SourceFormat::Html => parse_html(source),
    };
    if blocks.is_empty() {
        return Err(RenderError::EmptySource);
    }
    layout_pdf(&blocks, page)
}

// ---------------------------------------------------------------------------
// Markdown -> RenderBlock
// ---------------------------------------------------------------------------

fn parse_markdown(source: &str) -> Vec<RenderBlock> {
    let arena = Arena::new();
    let opts = Options::default();
    let root = parse_document(&arena, source, &opts);
    let mut out = Vec::new();
    walk_md(root, &mut out);
    out
}

fn walk_md<'a>(node: &'a AstNode<'a>, out: &mut Vec<RenderBlock>) {
    for child in node.children() {
        match &child.data.borrow().value {
            NodeValue::Heading(h) => {
                let level = h.level.clamp(1, 6);
                let text = collect_text(child);
                out.push(RenderBlock::Heading { level, text });
            }
            NodeValue::Paragraph => {
                let text = collect_text(child);
                if !text.is_empty() {
                    out.push(RenderBlock::Paragraph(text));
                }
            }
            NodeValue::List(list) => {
                let bullet = matches!(list.list_type, ListType::Bullet);
                let mut counter: u32 = list.start as u32;
                for item in child.children() {
                    if !matches!(item.data.borrow().value, NodeValue::Item(_)) {
                        continue;
                    }
                    let text = collect_text(item);
                    if bullet {
                        out.push(RenderBlock::BulletItem(text));
                    } else {
                        out.push(RenderBlock::NumberedItem { n: counter, text });
                        counter += 1;
                    }
                }
            }
            NodeValue::CodeBlock(cb) => {
                out.push(RenderBlock::CodeBlock(cb.literal.clone()));
            }
            NodeValue::BlockQuote => {
                // Render the inner text as a paragraph for v1.
                let text = collect_text(child);
                if !text.is_empty() {
                    out.push(RenderBlock::Paragraph(text));
                }
            }
            _ => {
                walk_md(child, out);
            }
        }
    }
}

fn collect_text<'a>(node: &'a AstNode<'a>) -> String {
    let mut buf = String::new();
    collect_text_into(node, &mut buf);
    buf.trim().to_string()
}

fn collect_text_into<'a>(node: &'a AstNode<'a>, out: &mut String) {
    for child in node.children() {
        match &child.data.borrow().value {
            NodeValue::Text(t) => out.push_str(t),
            NodeValue::Code(c) => out.push_str(&c.literal),
            NodeValue::SoftBreak | NodeValue::LineBreak => out.push(' '),
            _ => collect_text_into(child, out),
        }
    }
}

// ---------------------------------------------------------------------------
// HTML -> RenderBlock
//
// Minimal walker — recognises h1..h6, p, ul/ol/li, pre/code, br. Unknown
// tags are transparent: their text content flows into the current block
// container. CSS is ignored; inline styling that the user expects to see
// (bold, italic) does not affect v1 layout.
// ---------------------------------------------------------------------------

fn parse_html(source: &str) -> Vec<RenderBlock> {
    let tokens = tokenize_html(source);
    let mut out = Vec::new();
    let mut idx = 0;
    let mut list_stack: Vec<ListCtx> = Vec::new();
    let mut buf = String::new();
    let mut current: Option<BlockCtx> = None;

    while idx < tokens.len() {
        match &tokens[idx] {
            HtmlToken::Open(tag) => {
                let tag = tag.as_str();
                match tag {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        flush_current(&mut current, &mut buf, &mut out);
                        let level: u8 = tag[1..].parse().unwrap_or(1);
                        current = Some(BlockCtx::Heading(level));
                    }
                    "p" => {
                        flush_current(&mut current, &mut buf, &mut out);
                        current = Some(BlockCtx::Paragraph);
                    }
                    "ul" => list_stack.push(ListCtx::Bullet),
                    "ol" => list_stack.push(ListCtx::Numbered(1)),
                    "li" => {
                        flush_current(&mut current, &mut buf, &mut out);
                        match list_stack.last_mut() {
                            Some(ListCtx::Bullet) => current = Some(BlockCtx::BulletItem),
                            Some(ListCtx::Numbered(n)) => {
                                let value = *n;
                                *n += 1;
                                current = Some(BlockCtx::NumberedItem(value));
                            }
                            None => current = Some(BlockCtx::BulletItem),
                        }
                    }
                    "pre" => {
                        flush_current(&mut current, &mut buf, &mut out);
                        current = Some(BlockCtx::Code);
                    }
                    "br" => buf.push('\n'),
                    _ => {}
                }
            }
            HtmlToken::Close(tag) => {
                let tag = tag.as_str();
                match tag {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "p" | "li" | "pre" => {
                        flush_current(&mut current, &mut buf, &mut out);
                    }
                    "ul" | "ol" => {
                        list_stack.pop();
                    }
                    _ => {}
                }
            }
            HtmlToken::Text(t) => {
                if current.is_some() {
                    buf.push_str(t);
                }
            }
        }
        idx += 1;
    }
    flush_current(&mut current, &mut buf, &mut out);
    out
}

#[derive(Debug)]
enum HtmlToken {
    Open(String),
    Close(String),
    Text(String),
}

#[derive(Debug, Clone, Copy)]
enum BlockCtx {
    Heading(u8),
    Paragraph,
    BulletItem,
    NumberedItem(u32),
    Code,
}

#[derive(Debug)]
enum ListCtx {
    Bullet,
    Numbered(u32),
}

fn flush_current(current: &mut Option<BlockCtx>, buf: &mut String, out: &mut Vec<RenderBlock>) {
    let Some(ctx) = current.take() else {
        buf.clear();
        return;
    };
    let raw = std::mem::take(buf);
    let trimmed = match ctx {
        BlockCtx::Code => raw, // preserve internal whitespace
        _ => collapse_whitespace(&raw),
    };
    if trimmed.is_empty() {
        return;
    }
    match ctx {
        BlockCtx::Heading(level) => out.push(RenderBlock::Heading {
            level,
            text: trimmed,
        }),
        BlockCtx::Paragraph => out.push(RenderBlock::Paragraph(trimmed)),
        BlockCtx::BulletItem => out.push(RenderBlock::BulletItem(trimmed)),
        BlockCtx::NumberedItem(n) => out.push(RenderBlock::NumberedItem { n, text: trimmed }),
        BlockCtx::Code => out.push(RenderBlock::CodeBlock(trimmed)),
    }
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_was_space && !out.is_empty() {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(c);
            last_was_space = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

fn tokenize_html(source: &str) -> Vec<HtmlToken> {
    let mut tokens = Vec::new();
    let bytes = source.as_bytes();
    let mut i = 0;
    let mut text_buf = String::new();
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if !text_buf.is_empty() {
                tokens.push(HtmlToken::Text(decode_entities(&std::mem::take(&mut text_buf))));
            }
            if let Some(end) = find_byte(bytes, i + 1, b'>') {
                let raw = &source[i + 1..end];
                let raw_trim = raw.trim();
                if let Some(rest) = raw_trim.strip_prefix('/') {
                    let name = rest.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
                    if !name.is_empty() {
                        tokens.push(HtmlToken::Close(name));
                    }
                } else if !raw_trim.starts_with('!') && !raw_trim.starts_with('?') {
                    let name = raw_trim
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .trim_end_matches('/')
                        .to_ascii_lowercase();
                    if !name.is_empty() {
                        tokens.push(HtmlToken::Open(name.clone()));
                        if raw_trim.ends_with('/') || is_void_html_element(&name) {
                            tokens.push(HtmlToken::Close(name));
                        }
                    }
                }
                i = end + 1;
            } else {
                // Unterminated `<` — bail out, treat the rest as text.
                text_buf.push_str(&source[i..]);
                break;
            }
        } else {
            text_buf.push(bytes[i] as char);
            i += 1;
        }
    }
    if !text_buf.is_empty() {
        tokens.push(HtmlToken::Text(decode_entities(&text_buf)));
    }
    tokens
}

fn find_byte(haystack: &[u8], from: usize, needle: u8) -> Option<usize> {
    haystack[from..].iter().position(|b| *b == needle).map(|p| p + from)
}

fn is_void_html_element(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "source"
            | "track"
            | "wbr"
    )
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut iter = s.chars().peekable();
    while let Some(c) = iter.next() {
        if c != '&' {
            out.push(c);
            continue;
        }
        let mut name = String::new();
        let mut closed = false;
        for nc in iter.by_ref() {
            if nc == ';' {
                closed = true;
                break;
            }
            if name.len() >= 8 {
                break;
            }
            name.push(nc);
        }
        if !closed {
            out.push('&');
            out.push_str(&name);
            continue;
        }
        let decoded = match name.as_str() {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => {
                if let Some(rest) = name.strip_prefix('#') {
                    if let Some(hex) = rest.strip_prefix(['x', 'X']) {
                        u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                    } else {
                        rest.parse::<u32>().ok().and_then(char::from_u32)
                    }
                } else {
                    None
                }
            }
        };
        match decoded {
            Some(c) => out.push(c),
            None => {
                out.push('&');
                out.push_str(&name);
                out.push(';');
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// Font size in points for each heading level (1..=6) and body text.
/// Body sits at index 0; `heading_size(level)` indexes 1..=6.
const FONT_SIZES_PT: [f32; 7] = [11.0, 22.0, 18.0, 15.0, 13.0, 12.0, 11.0];

fn heading_size_pt(level: u8) -> f32 {
    FONT_SIZES_PT[level.clamp(1, 6) as usize]
}

const BODY_FONT_SIZE_PT: f32 = FONT_SIZES_PT[0];
/// Line height as a multiplier on font size — 1.4 matches the typical
/// body-text default in modern print stylesheets.
const LINE_HEIGHT_RATIO: f32 = 1.4;
/// Vertical gap after a block, expressed as a fraction of the body
/// line height. A full extra line is more visual breathing than docs
/// generators typically use; 0.6 keeps blocks readable without padding
/// pages excessively.
const BLOCK_SPACING_RATIO: f32 = 0.6;
/// Mean character width for Helvetica as a fraction of the font size.
/// Used to estimate the wrap column without loading the font metrics —
/// 0.50 errs slightly tight, which means lines may break a character
/// or two earlier than the ideal but no glyph runs off the page.
const HELVETICA_AVG_CHAR_WIDTH_RATIO: f32 = 0.50;

fn layout_pdf(blocks: &[RenderBlock], page: &PageConfig) -> Result<Vec<u8>, RenderError> {
    let mut doc = PdfDocument::new("rublocks");
    let page_width_mm = page.width_mm;
    let page_height_mm = page.height_mm;
    let [m_top, m_right, m_bottom, m_left] = page.margin;
    let content_width_mm = (page_width_mm - m_left - m_right).max(10.0);
    let content_top_mm = page_height_mm - m_top;
    let content_bottom_mm = m_bottom;

    let mut pages: Vec<Vec<Op>> = Vec::new();
    let mut current_ops: Vec<Op> = Vec::new();
    let mut cursor_mm: f32 = content_top_mm;
    let mut text_open = false;

    let ensure_text_section = |ops: &mut Vec<Op>, open: &mut bool, x_mm: f32, y_mm: f32| {
        if !*open {
            ops.push(Op::StartTextSection);
            *open = true;
        }
        ops.push(Op::SetTextCursor {
            pos: Point::new(Mm(x_mm), Mm(y_mm)),
        });
    };

    let emit_lines = |lines: &[(String, f32, BuiltinFont)],
                         cursor: &mut f32,
                         current_ops: &mut Vec<Op>,
                         pages: &mut Vec<Vec<Op>>,
                         text_open: &mut bool| {
        for (line, font_size_pt, font) in lines {
            let line_height_pt = font_size_pt * LINE_HEIGHT_RATIO;
            let line_height_mm = pt_to_mm(line_height_pt);
            if *cursor - line_height_mm < content_bottom_mm {
                if *text_open {
                    current_ops.push(Op::EndTextSection);
                    *text_open = false;
                }
                pages.push(std::mem::take(current_ops));
                *cursor = content_top_mm;
            }
            *cursor -= line_height_mm;
            ensure_text_section(current_ops, text_open, m_left, *cursor);
            current_ops.push(Op::SetFont {
                font: PdfFontHandle::Builtin(*font),
                size: Pt(*font_size_pt),
            });
            current_ops.push(Op::SetLineHeight {
                lh: Pt(line_height_pt),
            });
            current_ops.push(Op::SetFillColor {
                col: Color::Rgb(Rgb {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    icc_profile: None,
                }),
            });
            current_ops.push(Op::ShowText {
                items: vec![TextItem::Text(line.clone())],
            });
        }
    };

    for block in blocks {
        match block {
            RenderBlock::Heading { level, text } => {
                let size = heading_size_pt(*level);
                let font = if *level <= 3 {
                    BuiltinFont::HelveticaBold
                } else {
                    BuiltinFont::Helvetica
                };
                let lines = wrap_block(text, content_width_mm, size, font);
                emit_lines(&lines, &mut cursor_mm, &mut current_ops, &mut pages, &mut text_open);
                cursor_mm -= pt_to_mm(size * LINE_HEIGHT_RATIO * BLOCK_SPACING_RATIO);
            }
            RenderBlock::Paragraph(text) => {
                let lines = wrap_block(text, content_width_mm, BODY_FONT_SIZE_PT, BuiltinFont::Helvetica);
                emit_lines(&lines, &mut cursor_mm, &mut current_ops, &mut pages, &mut text_open);
                cursor_mm -= pt_to_mm(BODY_FONT_SIZE_PT * LINE_HEIGHT_RATIO * BLOCK_SPACING_RATIO);
            }
            RenderBlock::BulletItem(text) => {
                let prefixed = format!("\u{2022}  {text}");
                let lines = wrap_block(&prefixed, content_width_mm, BODY_FONT_SIZE_PT, BuiltinFont::Helvetica);
                emit_lines(&lines, &mut cursor_mm, &mut current_ops, &mut pages, &mut text_open);
                cursor_mm -= pt_to_mm(BODY_FONT_SIZE_PT * LINE_HEIGHT_RATIO * 0.2);
            }
            RenderBlock::NumberedItem { n, text } => {
                let prefixed = format!("{n}. {text}");
                let lines = wrap_block(&prefixed, content_width_mm, BODY_FONT_SIZE_PT, BuiltinFont::Helvetica);
                emit_lines(&lines, &mut cursor_mm, &mut current_ops, &mut pages, &mut text_open);
                cursor_mm -= pt_to_mm(BODY_FONT_SIZE_PT * LINE_HEIGHT_RATIO * 0.2);
            }
            RenderBlock::CodeBlock(text) => {
                let mut lines: Vec<(String, f32, BuiltinFont)> = Vec::new();
                for raw_line in text.split('\n') {
                    let wrapped = wrap_code_line(raw_line, content_width_mm, BODY_FONT_SIZE_PT);
                    for w in wrapped {
                        lines.push((w, BODY_FONT_SIZE_PT, BuiltinFont::Courier));
                    }
                }
                emit_lines(&lines, &mut cursor_mm, &mut current_ops, &mut pages, &mut text_open);
                cursor_mm -= pt_to_mm(BODY_FONT_SIZE_PT * LINE_HEIGHT_RATIO * BLOCK_SPACING_RATIO);
            }
        }
    }

    if text_open {
        current_ops.push(Op::EndTextSection);
    }
    if !current_ops.is_empty() {
        pages.push(current_ops);
    }
    if pages.is_empty() {
        // Defensive: every non-empty `RenderBlock` list should produce
        // at least one page. Push a sentinel page rather than letting
        // `printpdf` reject an empty document.
        pages.push(vec![Op::Marker {
            id: "empty".to_string(),
        }]);
    }

    let pdf_pages: Vec<PdfPage> = pages
        .into_iter()
        .map(|ops| PdfPage::new(Mm(page_width_mm), Mm(page_height_mm), ops))
        .collect();
    let mut warnings = Vec::new();
    let bytes = doc
        .with_pages(pdf_pages)
        .save(&PdfSaveOptions::default(), &mut warnings);
    Ok(bytes)
}

fn pt_to_mm(pt: f32) -> f32 {
    pt * 25.4 / 72.0
}

fn mm_to_pt(mm: f32) -> f32 {
    mm * 72.0 / 25.4
}

fn wrap_block(
    text: &str,
    width_mm: f32,
    font_size_pt: f32,
    font: BuiltinFont,
) -> Vec<(String, f32, BuiltinFont)> {
    let max_pt = mm_to_pt(width_mm);
    let avg_char_pt = font_size_pt * HELVETICA_AVG_CHAR_WIDTH_RATIO;
    let max_chars = ((max_pt / avg_char_pt).floor() as usize).max(10);
    let mut out: Vec<(String, f32, BuiltinFont)> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
            continue;
        }
        if current.chars().count() + 1 + word.chars().count() <= max_chars {
            current.push(' ');
            current.push_str(word);
        } else {
            out.push((std::mem::take(&mut current), font_size_pt, font));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        out.push((current, font_size_pt, font));
    }
    if out.is_empty() {
        out.push((String::new(), font_size_pt, font));
    }
    out
}

fn wrap_code_line(line: &str, width_mm: f32, font_size_pt: f32) -> Vec<String> {
    // Courier is monospace — using the same avg-char ratio overestimates
    // slightly but stays inside the page.
    let max_pt = mm_to_pt(width_mm);
    let avg_char_pt = font_size_pt * HELVETICA_AVG_CHAR_WIDTH_RATIO;
    let max_chars = ((max_pt / avg_char_pt).floor() as usize).max(10);
    if line.chars().count() <= max_chars {
        return vec![line.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for c in line.chars() {
        current.push(c);
        if current.chars().count() >= max_chars {
            chunks.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

