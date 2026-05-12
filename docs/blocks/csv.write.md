# `csv.write`

Write-side block. Takes an iterable bound earlier in the pipeline
(typically `Vec<crate::models::T>` from `db.find_many`) and produces a
CSV byte buffer ready to ship as a response body. The encoded bytes are
bound to `$<name>` as `bytes::Bytes` — `view` / `output` can splice them
into a response and downstream blocks can consume them as a byte
stream.

## Schema

```json
{
  "name":      "posts_csv",
  "block":     "csv.write",
  "rows":      "$posts",
  "headers":   ["id", "title", "published_at"],
  "delimiter": ",",
  "quoting":   "necessary",
  "encoding":  "utf-8"
}
```

| Field | Required | Type | Notes |
|-------|----------|------|-------|
| `block` | yes | `"csv.write"` | Discriminator. |
| `name` | yes | string | Binds `bytes::Bytes` to `$<name>`. |
| `rows` | yes | `$ref` | Reference to a prior `db.find_many` binding (`Vec<crate::models::T>`). Compile-time check that the binding exists and is iterable. |
| `headers` | no | array of string | When omitted, derived from the model's field order. When provided, only those columns are emitted (in that order). Every entry must match a declared model field — unknown columns fail at build time. |
| `delimiter` | no | string (1 char) | Default `,`. Any single ASCII character. Multi-character or non-ASCII values fail at build time. |
| `quoting` | no | enum | `"necessary"` (default), `"always"`, `"never"`. |
| `encoding` | no | enum | Character encoding for the emitted CSV bytes. Default = the project-wide `encoding` declared in `main.json` (see [`encoding.md`](../encoding.md)). Validated against the same closed enum as `main.json.encoding` — today only `"utf-8"` is accepted; any other value fails at build time with a browser-visible error naming the offending block. |

## Output

`$<name>` resolves to `bytes::Bytes` (the encoded CSV body). Streaming
that buffer as a downloadable response depends on a `route.kind: "file"`
which is tracked separately; until then, page or API routes can return
the bytes verbatim in `view` / `output`.

## Encoding

The block inherits the project-wide encoding declared in `main.json`. An
explicit `encoding` field overrides the default and must be a member of
the closed enum shared with `main.json.encoding`. Today only `"utf-8"`
is accepted; an `encoding` value on a block that doesn't match the
project's also emits a build-time warning so a future divergence is
intentional, not accidental.

## Out of scope (v1)

- Streaming the produced bytes directly to the HTTP response without
  buffering. v1 builds the whole CSV in memory.
- Writing to disk. The block always produces an in-memory `Bytes` value.
- Per-row transformations. The model fields are emitted verbatim — for
  derived columns, project them through a prior block.
