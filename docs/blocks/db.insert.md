# `db.insert`

Write-side block. Inserts a single row into a SQL table. Does not bind a
value — `$<name>` references against an insert block are not supported.

## Schema

```json
{
  "block": "db.insert",
  "table": "comments",
  "values": {
    "post_id":      "$post.id",
    "author_name":  "$input.body.author_name",
    "author_email": "$input.body.author_email",
    "body":         "$input.body.body"
  }
}
```

| Field | Required | Type | Notes |
|-------|----------|------|-------|
| `block` | yes | `"db.insert"` | Discriminator. |
| `table` | yes | string | Must match an existing model's `table`. |
| `values` | yes | object | Column → value map. Each value is a literal or a `$<ref>` to input/path/query/body or a prior block. Must contain at least one entry. |
| `name` | no | string | Reserved for a future return-affected-row mode. Not bindable today. |

## Output

None. Insert blocks are write-side; the handler's response is shaped by
the route's `redirect` / `output` / template, not by the insert.

## Runtime

The block emits an `INSERT` statement via `sqlx::QueryBuilder`. Each
`$<ref>` in `values:` is bound with `push_bind` at request time; literal
values are formatted in place. Model fields with a CEL `validate:`
predicate are evaluated before the bind — a `false` result short-circuits
the handler with `422 Unprocessable Content` and the offending column
name.
