# `xlsx.write`

Write-side conversion block. Assembles an XLSX workbook from N named
row collections — one sheet per entry — and binds the encoded body to
`$<name>` as `bytes::Bytes`. The resulting bytes carry the MIME type
`application/vnd.openxmlformats-officedocument.spreadsheetml.sheet`
when shipped over HTTP.

Pairs with [`xlsx.read`](xlsx.read.md). The two blocks share the
same value contract: rows in / rows out, with the model's field order
driving the cell schema unless `headers` overrides it.

## Schema

```json
{
  "name":   "report",
  "block":  "xlsx.write",
  "sheets": {
    "Posts":    { "rows": "$posts" },
    "Comments": { "rows": "$comments", "headers": ["id", "post_id", "body"] }
  }
}
```

| Field | Required | Type | Notes |
|-------|----------|------|-------|
| `block` | yes | `"xlsx.write"` | Discriminator. |
| `name` | yes | string | Binds `bytes::Bytes` (workbook body) to `$<name>`. |
| `sheets` | yes | object | Map `sheet_name -> SheetSpec`. At least one entry. Sheet names validated at load time. |
| `sheets.<n>.rows` | yes | `$<block_name>` | Reference to a prior `Vec<T>` list binding (typically [`db.find_many`](db.find_many.md)). Field access (`$posts.id`) is rejected — the block needs the whole collection. |
| `sheets.<n>.headers` | no | array of string | Optional explicit header row. When omitted, the matched model's field order is used. Length must match the field count. |

## Sheet-name rules

OOXML caps sheet names at **31 characters** and rejects the
characters `: \ / ? * [ ]`. Duplicates are rejected case-insensitively
(`Posts` and `posts` cannot coexist) so the workbook always opens
cleanly in Excel / LibreOffice / Numbers. All three rules are
checked at manifest load — typos surface in the dev overlay with the
offending sheet name, not at runtime.

## Output

`$<name>` resolves to `bytes::Bytes` (the encoded workbook body).
Downstream blocks accept it as their `source` input — no
re-serialization required. The expected MIME type, when serving the
binding directly from an HTTP route, is
`application/vnd.openxmlformats-officedocument.spreadsheetml.sheet`.

## Cell types

| Model field type | Excel cell type |
|------------------|-----------------|
| `int`, `bigint` | Number |
| `bool` | Boolean |
| `string`, `text`, `email` | String |
| `uuid` | String (`Uuid::to_string()`) |
| `timestamptz` | String (`DateTime::to_string()`) |

Nullable fields write an empty cell when the value is `None` and the
typed cell otherwise — Excel sees numeric columns as numbers, not as
strings holding numeric digits.

## Out of scope (v1)

- Cell formatting (number formats, colors, frozen panes, formulas).
  V1 emits plain values; cosmetic styling lands in a follow-up.
- Reading multiple sheets in a single block call — see
  [`xlsx.read`](xlsx.read.md). Authors compose multiple `xlsx.read`
  blocks instead, which keeps each block's binding shape obvious.
- Streaming for very large workbooks (>100k rows). The whole
  workbook is buffered into `bytes::Bytes` first.
