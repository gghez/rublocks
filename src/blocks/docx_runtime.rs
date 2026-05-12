//! Runtime DOCX rendering used by the `docx.render` block.
//!
//! Single source of truth: this file is embedded verbatim into the
//! generated dist crate as `_rb_docx` (so the runtime conversion logic
//! is shared with the compiler's own round-trip tests), and is also
//! `mod`-ed into the compiler crate behind `#[cfg(test)]` so the
//! same code is exercised by `cargo test` without spinning up a dist
//! project.
//!
//! Pipeline: source (markdown or HTML) → HTML fragment → DOM walk →
//! `docx-rs` builder → DOCX zip bytes. Going through HTML for both
//! source formats keeps the walker single-pathed; the markdown route
//! just adds a `pulldown_cmark::html::push_html` prelude.
//!
//! The supported HTML/markdown subset and the error contract for
//! unsupported constructs are documented in `docs/blocks/docx.render.md`.

use docx_rs::{
    AbstractNumbering, Docx, Level, LevelJc, LevelText, NumberFormat, Numbering, NumberingId,
    Paragraph, Run, RunFonts, SpecialIndentType, Start, Style, StyleType, Table, TableCell,
    TableRow,
};
use html5ever::driver::ParseOpts;
use html5ever::parse_document;
use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};

/// Source format accepted by [`render`]. The block layer rejects every
/// other value at manifest load time so this enum stays minimal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    Markdown,
    Html,
}

/// Conversion failures surfaced to the block layer. `Unsupported`
/// carries the offending tag so the dev-mode 500 response can name it
/// exactly — that's the contract documented in
/// `docs/blocks/docx.render.md`.
#[derive(Debug, Clone)]
pub enum DocxError {
    Unsupported { tag: String },
    Build(String),
}

impl std::fmt::Display for DocxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DocxError::Unsupported { tag } => write!(
                f,
                "docx.render: unsupported HTML/markdown construct `<{tag}>` — \
                 see docs/blocks/docx.render.md for the supported subset"
            ),
            DocxError::Build(msg) => write!(f, "docx.render: build failed: {msg}"),
        }
    }
}

impl std::error::Error for DocxError {}

/// Render `source` to a DOCX byte buffer. Errors are returned rather
/// than panicked so the block layer can dispatch them to the dev-mode
/// 500 response with the user-friendly message.
pub fn render(source: &str, format: SourceFormat) -> Result<bytes::Bytes, DocxError> {
    let html = match format {
        SourceFormat::Html => source.to_string(),
        SourceFormat::Markdown => {
            let parser = pulldown_cmark::Parser::new_ext(source, pulldown_cmark::Options::all());
            let mut buf = String::with_capacity(source.len() + 32);
            pulldown_cmark::html::push_html(&mut buf, parser);
            buf
        }
    };

    let dom: RcDom = parse_document(RcDom::default(), ParseOpts::default())
        .from_utf8()
        .read_from(&mut html.as_bytes())
        .map_err(|e| DocxError::Build(format!("html parse: {e}")))?;

    let mut builder = DocxBuilder::new();
    walk_document(&dom.document, &mut builder)?;
    builder.finish()
}

/// `docx-rs` numbering ids — fixed per writer so the dist code does
/// not need to thread a counter through every nested list emission.
/// One numbering per list style is enough for the v1 supported subset.
const BULLET_NUM_ID: usize = 1;
const ORDERED_NUM_ID: usize = 2;
const BULLET_ABSTRACT_ID: usize = 1;
const ORDERED_ABSTRACT_ID: usize = 2;

/// Accumulates the top-level paragraphs/tables before assembling the
/// `Docx`. Keeping the buffer flat means the walker can recurse without
/// owning the `Docx` itself; nested constructs flatten into paragraphs
/// with the right indentation/numbering level at emission time.
struct DocxBuilder {
    blocks: Vec<DocxBlock>,
}

/// Boxed because `docx-rs`'s `Paragraph` is ~2 KiB and dwarfs `Table`
/// — keeping a flat enum here would let the larger variant dominate
/// every entry of `blocks: Vec<DocxBlock>` and clippy rightly flags it.
enum DocxBlock {
    Paragraph(Box<Paragraph>),
    Table(Box<Table>),
}

impl DocxBuilder {
    fn new() -> Self {
        Self { blocks: Vec::new() }
    }

    fn push_paragraph(&mut self, p: Paragraph) {
        self.blocks.push(DocxBlock::Paragraph(Box::new(p)));
    }

    fn push_table(&mut self, t: Table) {
        self.blocks.push(DocxBlock::Table(Box::new(t)));
    }

    fn finish(self) -> Result<bytes::Bytes, DocxError> {
        // Headings styles. `docx-rs` does not auto-create them, so the
        // dist document declares them up-front. The id pattern mirrors
        // Word's built-ins so any external viewer (LibreOffice, Pages)
        // recognises the style on import.
        let mut doc = Docx::new()
            .add_style(Style::new("Heading1", StyleType::Paragraph).name("heading 1"))
            .add_style(Style::new("Heading2", StyleType::Paragraph).name("heading 2"))
            .add_style(Style::new("Heading3", StyleType::Paragraph).name("heading 3"))
            .add_style(Style::new("Heading4", StyleType::Paragraph).name("heading 4"))
            .add_style(Style::new("Heading5", StyleType::Paragraph).name("heading 5"))
            .add_style(Style::new("Heading6", StyleType::Paragraph).name("heading 6"));

        let bullet = AbstractNumbering::new(BULLET_ABSTRACT_ID).add_level(
            Level::new(
                0,
                Start::new(1),
                NumberFormat::new("bullet"),
                LevelText::new("•"),
                LevelJc::new("left"),
            )
            .indent(Some(720), Some(SpecialIndentType::Hanging(360)), None, None),
        );
        let ordered = AbstractNumbering::new(ORDERED_ABSTRACT_ID).add_level(
            Level::new(
                0,
                Start::new(1),
                NumberFormat::new("decimal"),
                LevelText::new("%1."),
                LevelJc::new("left"),
            )
            .indent(Some(720), Some(SpecialIndentType::Hanging(360)), None, None),
        );

        doc = doc
            .add_abstract_numbering(bullet)
            .add_abstract_numbering(ordered)
            .add_numbering(Numbering::new(BULLET_NUM_ID, BULLET_ABSTRACT_ID))
            .add_numbering(Numbering::new(ORDERED_NUM_ID, ORDERED_ABSTRACT_ID));

        for block in self.blocks {
            doc = match block {
                DocxBlock::Paragraph(p) => doc.add_paragraph(*p),
                DocxBlock::Table(t) => doc.add_table(*t),
            };
        }

        // `XMLDocx::pack` writes the zip via the `zip` crate, which
        // requires `Write + Seek`. `Vec<u8>` is `Write`-only, so wrap
        // it in `Cursor` before handing it over.
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        doc.build()
            .pack(&mut cursor)
            .map_err(|e| DocxError::Build(format!("zip pack: {e}")))?;
        Ok(bytes::Bytes::from(cursor.into_inner()))
    }
}

/// Inline run accumulator carried through recursive walks of mixed
/// content (paragraphs, headings, list items, table cells). Tracks the
/// emphasis flags so nested `<strong><em>foo</em></strong>` emits a
/// single run with both flags set.
#[derive(Debug, Clone, Default)]
struct InlineCtx {
    bold: bool,
    italic: bool,
    code: bool,
}

fn walk_document(root: &Handle, builder: &mut DocxBuilder) -> Result<(), DocxError> {
    for child in root.children.borrow().iter() {
        match &child.data {
            NodeData::Document => walk_document(child, builder)?,
            NodeData::Doctype { .. } | NodeData::Comment { .. } => {}
            NodeData::Element { name, .. } => {
                let tag = name.local.as_ref();
                match tag {
                    // `parse_document` always synthesises `<html><head/>
                    // <body>...</body></html>` around the actual
                    // content. Descend through the wrappers and drop
                    // metadata silently — `<head>` is never user-
                    // authored DOCX content.
                    "html" | "body" => walk_document(child, builder)?,
                    "head" | "script" | "style" | "noscript" | "title" | "meta" | "link" => {}
                    _ => walk_block(child, builder)?,
                }
            }
            NodeData::Text { contents } => {
                let text = contents.borrow();
                if text.trim().is_empty() {
                    continue;
                }
                let mut para = Paragraph::new();
                para = para.add_run(Run::new().add_text(text.to_string()));
                builder.push_paragraph(para);
            }
            NodeData::ProcessingInstruction { .. } => {}
        }
    }
    Ok(())
}

fn walk_block_children(node: &Handle, builder: &mut DocxBuilder) -> Result<(), DocxError> {
    for child in node.children.borrow().iter() {
        match &child.data {
            NodeData::Element { name, .. } => {
                let tag = name.local.as_ref();
                if matches!(tag, "head" | "script" | "style" | "noscript") {
                    continue;
                }
                walk_block(child, builder)?;
            }
            _ => walk_block(child, builder)?,
        }
    }
    Ok(())
}

fn walk_block(node: &Handle, builder: &mut DocxBuilder) -> Result<(), DocxError> {
    match &node.data {
        NodeData::Element { name, .. } => {
            let tag = name.local.as_ref();
            match tag {
                "p" => {
                    let mut para = Paragraph::new();
                    collect_inline_children(node, &InlineCtx::default(), &mut para)?;
                    builder.push_paragraph(para);
                }
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    let level = tag.as_bytes()[1] - b'0';
                    let style = format!("Heading{level}");
                    let mut para = Paragraph::new().style(&style);
                    collect_inline_children(node, &InlineCtx::default(), &mut para)?;
                    builder.push_paragraph(para);
                }
                "ul" => emit_list(node, builder, BULLET_NUM_ID)?,
                "ol" => emit_list(node, builder, ORDERED_NUM_ID)?,
                "table" => builder.push_table(build_table(node)?),
                "br" => {
                    let para = Paragraph::new();
                    builder.push_paragraph(para);
                }
                // Inline at the block level: wrap into a single
                // paragraph. Covers `<strong>foo</strong>` floating
                // outside a `<p>` (rare but legal).
                "span" | "strong" | "b" | "em" | "i" | "code" | "a" => {
                    let mut para = Paragraph::new();
                    collect_inline(node, &InlineCtx::default(), &mut para)?;
                    builder.push_paragraph(para);
                }
                "blockquote" | "div" | "section" | "article" | "main" | "header" | "footer"
                | "nav" | "aside" => walk_block_children(node, builder)?,
                "hr" => {
                    // Render as an empty paragraph in v1 — Word's
                    // border-paragraph form is a follow-up.
                    builder.push_paragraph(Paragraph::new());
                }
                other => {
                    return Err(DocxError::Unsupported {
                        tag: other.to_string(),
                    });
                }
            }
        }
        NodeData::Text { contents } => {
            let text = contents.borrow();
            if text.trim().is_empty() {
                return Ok(());
            }
            let mut para = Paragraph::new();
            para = para.add_run(Run::new().add_text(text.to_string()));
            builder.push_paragraph(para);
        }
        NodeData::Comment { .. }
        | NodeData::Doctype { .. }
        | NodeData::ProcessingInstruction { .. }
        | NodeData::Document => {}
    }
    Ok(())
}

fn emit_list(node: &Handle, builder: &mut DocxBuilder, num_id: usize) -> Result<(), DocxError> {
    for child in node.children.borrow().iter() {
        if let NodeData::Element { name, .. } = &child.data {
            let tag = name.local.as_ref();
            if tag != "li" {
                // Tolerate stray whitespace text nodes (handled by
                // outer match); reject anything else explicitly.
                return Err(DocxError::Unsupported {
                    tag: tag.to_string(),
                });
            }
            let mut para =
                Paragraph::new().numbering(NumberingId::new(num_id), docx_rs::IndentLevel::new(0));
            collect_inline_children(child, &InlineCtx::default(), &mut para)?;
            builder.push_paragraph(para);
        }
    }
    Ok(())
}

fn build_table(node: &Handle) -> Result<Table, DocxError> {
    let mut rows: Vec<TableRow> = Vec::new();
    walk_table_children(node, &mut rows)?;
    Ok(Table::new(rows))
}

fn walk_table_children(node: &Handle, rows: &mut Vec<TableRow>) -> Result<(), DocxError> {
    for child in node.children.borrow().iter() {
        if let NodeData::Element { name, .. } = &child.data {
            let tag = name.local.as_ref();
            match tag {
                "thead" | "tbody" | "tfoot" => walk_table_children(child, rows)?,
                "tr" => rows.push(build_row(child)?),
                _ => {
                    return Err(DocxError::Unsupported {
                        tag: tag.to_string(),
                    });
                }
            }
        }
    }
    Ok(())
}

fn build_row(node: &Handle) -> Result<TableRow, DocxError> {
    let mut cells: Vec<TableCell> = Vec::new();
    for child in node.children.borrow().iter() {
        if let NodeData::Element { name, .. } = &child.data {
            let tag = name.local.as_ref();
            match tag {
                "td" | "th" => {
                    let mut para = Paragraph::new();
                    let ctx = InlineCtx {
                        bold: tag == "th",
                        ..Default::default()
                    };
                    collect_inline_children(child, &ctx, &mut para)?;
                    cells.push(TableCell::new().add_paragraph(para));
                }
                _ => {
                    return Err(DocxError::Unsupported {
                        tag: tag.to_string(),
                    });
                }
            }
        }
    }
    Ok(TableRow::new(cells))
}

fn collect_inline_children(
    node: &Handle,
    ctx: &InlineCtx,
    para: &mut Paragraph,
) -> Result<(), DocxError> {
    for child in node.children.borrow().iter() {
        collect_inline(child, ctx, para)?;
    }
    Ok(())
}

fn collect_inline(node: &Handle, ctx: &InlineCtx, para: &mut Paragraph) -> Result<(), DocxError> {
    match &node.data {
        NodeData::Text { contents } => {
            let text = contents.borrow().to_string();
            if text.is_empty() {
                return Ok(());
            }
            let mut run = Run::new().add_text(text);
            if ctx.bold {
                run = run.bold();
            }
            if ctx.italic {
                run = run.italic();
            }
            if ctx.code {
                run = run.fonts(RunFonts::new().ascii("Consolas"));
            }
            // `add_run` is consuming; reborrow via mem::take to keep the
            // walker single-pass without dropping the accumulator.
            let owned = std::mem::take(para);
            *para = owned.add_run(run);
        }
        NodeData::Element { name, .. } => {
            let tag = name.local.as_ref();
            let mut nested = ctx.clone();
            match tag {
                "strong" | "b" => nested.bold = true,
                "em" | "i" => nested.italic = true,
                "code" => nested.code = true,
                "br" => {
                    let owned = std::mem::take(para);
                    *para = owned.add_run(Run::new().add_break(docx_rs::BreakType::TextWrapping));
                    return Ok(());
                }
                // Links: drop the URL in v1, render the inner text
                // inline. Surface fidelity is acceptable; hyperlink
                // emission lands when a real need appears.
                "a" => {}
                // Inline-neutral wrappers: just recurse.
                "span" => {}
                other => {
                    return Err(DocxError::Unsupported {
                        tag: other.to_string(),
                    });
                }
            }
            collect_inline_children(node, &nested, para)?;
        }
        NodeData::Comment { .. }
        | NodeData::Doctype { .. }
        | NodeData::ProcessingInstruction { .. }
        | NodeData::Document => {}
    }
    Ok(())
}
