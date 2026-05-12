# `csv.read`

Read-side block. Consumes raw bytes (typically the output of
`sftp.read`, or a multipart body field once that lands) and binds the
parsed rows to `$<name>` as `Vec<crate::models::T>` for the named
model. Header validation runs up-front so a misshapen source fails
fast; per-row parse errors short-circuit with `422` carrying the
offending line number and column name.

## Schema

```json
{
  "name":       "imported",
  "block":      "csv.read",
  "source":     "$raw",
  "model":      "Post",
  "delimiter":  ",",
  "has_header": true,
  "encoding":   "utf-8"
}
```

| Field | Required | Type | Notes |
|-------|----------|------|-------|
| `block` | yes | `"csv.read"` | Discriminator. |
| `name` | yes | string | Binds `Vec<crate::models::T>` to `$<name>`. |
| `source` | yes | `$ref` | Reference to a `bytes::Bytes` / `&[u8]` binding (typically the output of `sftp.read`). |
| `model` | yes | string | Existing model whose fields define the record schema. |
| `delimiter` | no | string (1 char) | Default `,`. Any single ASCII character. Multi-character or non-ASCII values fail at build time. |
| `has_header` | no | bool | Default `true`. When `true`, the first record is the header row, validated against the model fields (extra columns rejected, missing required columns rejected). When `false`, columns are positional in declared field order. |
| `encoding` | no | enum | Character encoding for the incoming bytes. Default = the project-wide `encoding` declared in `main.json` (see [`encoding.md`](../encoding.md)). Validated against the same closed enum as `main.json.encoding` — today only `"utf-8"` is accepted; any other value fails at build time with a browser-visible error naming the offending block. Bytes that don't decode under the chosen encoding return `400 Bad Request` with the offending byte offset surfaced in dev-mode. |

## Output

`$<name>` resolves to `Vec<crate::models::T>` where `T` is the model
declared in `model`. The binding behaves like a `db.find_many` result —
downstream blocks can iterate it, `view` / `output` can project it,
`csv.write` can round-trip it.

## Runtime contract

1. **Decoding.** The block decodes the source bytes under the declared
   encoding. A byte sequence that doesn't decode returns `400` with the
   first invalid byte offset in the response body — the dev-mode reader
   can pinpoint the corruption without leaving the browser.
2. **Header validation.** When `has_header` is `true`, the first record
   is matched against the model field set. Unknown columns, duplicate
   columns, and missing required columns each fail with `400` and a
   message naming the offending column.
3. **Per-row parsing.** Each record's cells are parsed into the model
   field types (UUID, RFC3339 timestamp, integer, …). On parse failure
   the block returns `422` carrying the 1-based line number plus the
   offending column name — the source can be fixed without leaving the
   browser.
4. **Nullable fields.** An empty cell parses as `None` for nullable
   fields. Non-nullable fields with an empty cell fail with `422`.

## Encoding

The block inherits the project-wide encoding declared in `main.json`.
An explicit `encoding` field overrides the default and must be a member
of the closed enum shared with `main.json.encoding`. Today only
`"utf-8"` is accepted; an `encoding` value on a block that doesn't
match the project's also emits a build-time warning so a future
divergence is intentional, not accidental.

## Out of scope (v1)

- Streaming reads of huge CSVs (`>100 MB`). v1 reads the whole source
  into memory before parsing.
- Custom date / decimal parsing beyond what the bound model already
  accepts (RFC3339 timestamps, base-10 integers, the standard Rust
  `bool` grammar, …).
- Recovering and continuing past a per-row error — the first failure
  short-circuits the handler.
