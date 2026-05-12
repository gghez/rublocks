# `xlsx.read`

Read-side conversion block. Parses one named sheet of an XLSX workbook
out of a prior `bytes::Bytes` binding and produces a typed
`Vec<crate::models::T>` for the matched model — the same shape a
[`db.find_many`](db.find_many.md) would yield.

Pairs with [`xlsx.write`](xlsx.write.md). The two blocks share the
same value contract, so a `db.find_many → xlsx.write` followed by
`xlsx.read` against the produced bytes round-trips the same records.

## Schema

```json
{
  "name":       "imported",
  "block":      "xlsx.read",
  "source":     "$workbook",
  "sheet":      "Posts",
  "model":      "Post",
  "has_header": true
}
```

| Field | Required | Type | Notes |
|-------|----------|------|-------|
| `block` | yes | `"xlsx.read"` | Discriminator. |
| `name` | yes | string | Binds `Vec<crate::models::T>` to `$<name>`. |
| `source` | yes | `$<block_name>` | Reference to a prior block producing `bytes::Bytes` — typically [`sftp.read`](sftp.read.md). Field access (`$workbook.body`) is rejected — the binding must resolve to the whole byte body. |
| `sheet` | yes | string | Sheet name to parse. Missing sheet → `422` with the list of available sheets in the response body. |
| `model` | yes | string | Existing model whose fields define the record schema. Must match a declared model `name`. |
| `has_header` | no | bool | Default `true`. When `false`, every row (including row 0) is treated as data. The header row is not validated against the model — it's there so an Excel reader sees column names. |

## Output

`$<name>` resolves to `Vec<crate::models::T>` for the matched model.
Downstream blocks (`db.insert` in a loop, `view`, `output`) consume it
exactly like a `db.find_many` binding.

## Cell coercion

| Model field type | Accepted cell shapes | Failure mode |
|------------------|---------------------|--------------|
| `int`, `bigint` | Number (integer or whole-number float), numeric string | `422` with `xlsx.read: cell `Posts!B5` cannot be parsed as int: …` |
| `bool` | Boolean, `"true"`/`"false"`/`"1"`/`"0"`/`"yes"`/`"no"` (case-insensitive) | `422` with the cell ref |
| `string`, `text`, `email` | Any cell — coerced via `Display` | Never errors |
| `uuid` | String matching the UUID grammar | `422` on parse failure |
| `timestamptz` | RFC 3339 string | `422` on parse failure |

Nullable fields accept an empty cell as `None`; non-empty cells flow
through the same coercion rules as the non-nullable variant.

## Errors surfaced in dev mode

- **Workbook open failure** (corrupt file, unsupported format) →
  `422` with `xlsx.read: workbook open failed: …`.
- **Missing sheet** → `422` with the list of available sheets so the
  author can fix the typo from the browser.
- **Row too short** (fewer cells than the model has columns) → `422`
  with sheet name + row number.
- **Bad cell coercion** → `422` with `Sheet!ColRow` and the cell
  contents that failed to parse.

Every error path emits a `tracing::error!` event with the same message
before returning, so logs and the in-browser response carry the same
text.

## Out of scope (v1)

- Reading multiple sheets in a single block call. Authors stack
  multiple `xlsx.read` blocks (one per sheet) instead — keeps each
  binding's type obvious.
- ODS / XLS support. `calamine` reads them, but the block surface
  only exposes XLSX so the contract stays narrow.
- Streaming for very large workbooks. The whole sheet is loaded into
  a `Vec` before any downstream block sees it.
