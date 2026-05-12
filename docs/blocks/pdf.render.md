# `pdf.render`

Read-side block. Renders an HTML or markdown source string to a PDF
byte buffer and binds the result to `$<name>` as `bytes::Bytes`.

The engine is pure Rust — `comrak` parses markdown, a minimal hand-rolled
walker covers a useful subset of HTML, and `printpdf` lays out
paragraphs, headings and lists onto pages with the built-in Helvetica
and Courier fonts. No headless browser, no external runtime, no font
file bundled. The rationale is recorded in
[`docs/decisions.md`](../decisions.md#pdf-rendering-engine-issue-20).

## Schema

```json
{
  "name":          "invoice_pdf",
  "block":         "pdf.render",
  "source":        "$invoice_html",
  "source_format": "html",
  "page": {
    "size":      "A4",
    "margin":    "20mm",
    "landscape": false
  }
}
```

| Field | Required | Type | Notes |
|-------|----------|------|-------|
| `block` | yes | `"pdf.render"` | Discriminator. |
| `name` | yes | string | Binds `bytes::Bytes` (PDF body) to `$<name>`. |
| `source` | yes | `$ref` / literal string | Reference to a `String` binding — HTML or markdown content. |
| `source_format` | yes | `"html"` \| `"markdown"` | Explicit dialect — no auto-detection. |
| `page` | no | object | Optional layout overrides. |
| `page.size` | no | `"A4"` \| `"Letter"` \| `"<W> x <H>"` | Default `"A4"`. Custom form requires explicit units, e.g. `"210mm x 297mm"`. |
| `page.margin` | no | string | CSS-style shorthand with explicit units (`mm` / `in`). Default `"20mm"`. |
| `page.landscape` | no | bool | Default `false`. Swaps width and height when `true`. |

## Output

`$<name>` resolves to `bytes::Bytes`. Most consumers either:

- ship the bytes back as the route's response body with the
  `application/pdf` content type (handled by a future writer block /
  output mapping — for v1, route to a custom handler that owns the
  response shape), or
- forward to an upload-side block (`sftp.write` …) that accepts `Bytes`.

## Source format support

| Format | Parser | Block-level elements supported |
|--------|--------|--------------------------------|
| `markdown` | [`comrak`](https://github.com/kivikakk/comrak) | headings (1..=6), paragraphs, bullet & ordered lists, code blocks, block quotes (rendered as paragraphs in v1) |
| `html` | Minimal hand-rolled walker | `<h1>`–`<h6>`, `<p>`, `<ul>` / `<ol>` / `<li>`, `<pre>`, `<br>`. Unknown tags are transparent — their text content flows into the current container; CSS is ignored. HTML entities (`&amp;`, `&lt;`, `&gt;`, `&quot;`, `&apos;`, `&nbsp;`, `&#NNN;`, `&#xNN;`) are decoded. |

Anything outside the supported subset is rendered as plain text, not
as an error — the renderer prefers "best effort" so a casually-authored
HTML snippet still produces a readable PDF.

## Layout

- Page size: A4 portrait, 20mm margins (defaults).
- Body font: Helvetica 11pt, line height 1.4×.
- Headings: H1 22pt bold, H2 18pt bold, H3 15pt bold, H4 13pt, H5 12pt,
  H6 11pt — all Helvetica.
- Code blocks: Courier 11pt, character-boundary wrapping.
- Line wrap uses an average-character-width estimate that errs a
  character or two tight — preferable to glyphs running off the page.

## Errors

Render failures surface as `500 Internal Server Error` with the engine
message embedded in the body. The dev overlay shows the message inline
so the author can correct the source without leaving the browser. An
empty `source` (or a source that strips down to no content blocks)
produces `pdf.render: source produced no content`.

Manifest-time validation rejects:

- Missing or mistyped `source_format`.
- `page.size` that is neither a known paper name nor a `<W> x <H>`
  form with explicit `mm` / `in` units.
- `page.margin` that omits the unit suffix on any value.

## Out of scope (v1)

- Page headers / footers, page numbers.
- Embedded images, inline styling (`<strong>`, `<em>` — text flows
  through but does not change appearance).
- Tables, full CSS layout, custom fonts.
- PDF/A archival profile, encryption, digital signatures.
- Streaming render of huge documents (the whole PDF is buffered in
  memory).
