# `db.find_one`

Read-side block. Loads a single row and binds it to `$<name>` as
`crate::models::T`. When the lookup returns no row, the nested
`on_missing` sub-block runs and short-circuits the handler.

## Schema

```json
{
  "name":  "post",
  "block": "db.find_one",
  "table": "posts",
  "where": { "slug": "$input.path.slug", "published_at": { "is_not_null": true } },
  "on_missing": {
    "block":       "error",
    "status":      404,
    "code":        "post_not_found",
    "description": "No published post matches the given slug."
  }
}
```

| Field | Required | Type | Notes |
|-------|----------|------|-------|
| `block` | yes | `"db.find_one"` | Discriminator. |
| `name` | yes | string | Binding for `$<name>` references. |
| `table` | yes | string | Must match an existing model's `table`. |
| `where` | no | string \| object | CEL string (syntax-checked at load time) or structured filter object. |
| `on_missing` | no | block | Sub-block executed when the row is not found. Typically `error`. Parsed recursively against the registry. |

## Output

- `$<name>` resolves to `crate::models::T` for the model whose `table`
  matches.
- `$<name>.<field>` resolves through the model's field types.

## Composition

`on_missing` is itself a [block](README.md). Any registered block kind
may be used — the registry rejects unknown ids and unknown fields the
same way it does at the top level. Today the canonical choice is
[`error`](error.md).

## Status

Same as `db.find_many`: parsing + validation only. Execution lands later.
