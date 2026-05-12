# `docx.render`

Read-side block. Renders an HTML or markdown source string to a DOCX
byte buffer and binds the result to `$<name>` as `bytes::Bytes`.

Mirrors the planned `pdf.render` shape: one `source` reference plus an
explicit `source_format`, no page/style options in v1.

## Schema

```json
{
  "name":          "report_docx",
  "block":         "docx.render",
  "source":        "$report_md",
  "source_format": "markdown"
}
```

| Field | Required | Type | Notes |
|-------|----------|------|-------|
| `block` | yes | `"docx.render"` | Discriminator. |
| `name` | yes | string | Binds `bytes::Bytes` (DOCX body) to `$<name>`. |
| `source` | yes | `$ref` | Reference to a prior `String` binding holding the HTML or markdown source. Literals are rejected — point at a prior block that builds the body. |
| `source_format` | yes | enum | `"html"` or `"markdown"`. No default — every call site names the format explicitly. |

## Output

`$<name>` resolves to `bytes::Bytes`. The body's MIME type is
`application/vnd.openxmlformats-officedocument.wordprocessingml.document`;
when shipped through a route's `output`, set the response
`Content-Type` accordingly (the block does not stamp the header itself
— `output` shape is the route's concern).

## Engine

Pure Rust, no external runtime. The conversion pipeline is:

1. **Markdown** sources flow through [`pulldown-cmark`][cmark] to an
   HTML fragment.
2. The HTML is parsed with [`html5ever`][h5] into an `RcDom` tree.
3. A walker translates the supported subset (see below) into
   [`docx-rs`][docx-rs] builder calls.
4. The resulting `Docx` is packed into a zip-backed byte buffer.

The same module ships into every generated dist project — the
crate-graph cost is conditional on a route declaring `docx.render`.

[cmark]: https://crates.io/crates/pulldown-cmark
[h5]: https://crates.io/crates/html5ever
[docx-rs]: https://crates.io/crates/docx-rs

## Supported subset (v1)

Block elements:

| Source | Maps to |
|--------|---------|
| `<p>` | Paragraph |
| `<h1>` – `<h6>` | Paragraph with the matching `HeadingN` style |
| `<ul>` / `<ol>` + `<li>` | Bullet / decimal-numbered paragraphs |
| `<table>` / `<thead>` / `<tbody>` / `<tr>` / `<td>` / `<th>` | DOCX table; `<th>` cells are emitted bold |
| `<blockquote>` / `<div>` / `<section>` / `<article>` / `<main>` / `<header>` / `<footer>` / `<nav>` / `<aside>` | Transparent wrappers — children flow through |
| `<hr>` | Empty paragraph (border-style hr is a v2 follow-up) |
| `<br>` | Line break inside the surrounding run |

Inline elements:

| Source | Maps to |
|--------|---------|
| Text | Plain run |
| `<strong>` / `<b>` | Bold run |
| `<em>` / `<i>` | Italic run |
| `<code>` | Monospaced run (Consolas fallback) |
| `<a>` | Inner text only — the URL is dropped in v1 |
| `<span>` | Transparent inline wrapper |

`<head>`, `<title>`, `<meta>`, `<link>`, `<script>`, `<style>`, and
`<noscript>` are stripped silently — they never reach the document body
in a sane source.

## Unsupported constructs (v1)

Anything not listed above (e.g. `<img>`, inline SVG, `<iframe>`,
`<form>`, MathML…) produces a build-time-style error surfaced through
the dev-mode browser overlay. The response is a `415 Unsupported Media
Type` whose body names the offending tag:

```
rublocks: docx.render: unsupported HTML/markdown construct `<svg>` — \
  see docs/blocks/docx.render.md for the supported subset
```

This is intentional: silently dropping an `<img>` would turn a missing
chart into a paragraph the user cannot diagnose without leaving the
loop. The browser overlay is where the agent gets told to switch the
construct or extend the supported subset.

## Example

A report route that pulls a markdown file from SFTP, renders it to
DOCX, and ships the bytes back to the caller:

```json
{
  "path":   "/reports/:slug.docx",
  "method": "GET",
  "kind":   "api",
  "process": [
    {
      "name":    "raw_md",
      "block":   "sftp.read",
      "service": "reports",
      "path":    "/incoming/$input.path.slug.md"
    },
    {
      "name":          "docx_body",
      "block":         "docx.render",
      "source":        "$raw_md",
      "source_format": "markdown"
    }
  ],
  "output": { "size": "$docx_body" }
}
```

## Out of scope (v1)

- Style templates (loading a `.dotx` and applying it). v1 emits the
  default Word styles only.
- Embedded images. `<img>` is rejected.
- Table-of-contents generation, footnotes, comments, hyperlinks.
- DOC (legacy binary), RTF, ODT output.
- `pdf.render` — tracked separately.
