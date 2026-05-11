# `db.find_many`

Read-side block. Loads a list of rows from a SQL table and binds them to
`$<name>` as `Vec<crate::models::T>`, where `T` is the model whose `table`
matches.

## Schema

```json
{
  "name":     "posts",
  "block":    "db.find_many",
  "table":    "posts",
  "where":    { "published_at": { "is_not_null": true } },
  "order_by": "-published_at",
  "limit":    "$input.query.limit",
  "offset":   "$input.query.offset"
}
```

| Field | Required | Type | Notes |
|-------|----------|------|-------|
| `block` | yes | `"db.find_many"` | Discriminator. |
| `name` | yes | string | Binding for `$<name>` references. |
| `table` | yes | string | Must match an existing model's `table`. |
| `where` | no | string \| object | CEL string (syntax-checked at load time) or structured filter object. |
| `order_by` | no | string \| array | `"col"` ascending, `"-col"` descending, or an array of those. |
| `limit` | no | int \| `$ref` | Result cap. Either a literal int or a `$input.X.X` reference. |
| `offset` | no | int \| `$ref` | Pagination offset, same accepted shapes. |

## Output

- `$<name>` resolves to `Vec<crate::models::T>` for the model whose
  `table` matches.
- `$<name>.<index>` and template iteration follow standard Askama
  semantics on the Rust `Vec`.

## Runtime

The block runs at request time against the wired `sqlx::Pool`. The
`where:` (string CEL or structured form), `order_by`, `limit`, and
`offset` are bound into a prepared statement; the result is loaded as
`Vec<crate::models::T>` and bound to `$<name>` for downstream blocks,
`view`, and `output`.

See [routes.md](../routes.md#where-clause-grammar) for the full
`where:` grammar.
