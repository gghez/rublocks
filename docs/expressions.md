# Expressions (CEL)

rublocks uses [Common Expression Language][cel] (CEL) as its sub-language
for guards, filters, validators, and view conditionals. CEL is
non-Turing-complete, side-effect-free, and used in production by
Kubernetes admission controllers and Envoy.

Every CEL snippet is **syntactically validated at build time**, and
every reference is **scope-checked**: an identifier that is not in the
local scope fails the build with the offender and the in-scope names
listed. The `guard` block, `input.*.validate`, and `models/*.json`
`fields.<col>.validate` are evaluated at request time; the string form
of `db.find_*.where` is translated to a SQL fragment at build time and
executed by the `db.find_*` blocks against the wired pool.

[cel]: https://github.com/google/cel-spec

## Where CEL appears

| JSON site | Purpose |
|-----------|---------|
| `process[*]` `guard.if` (see [`guard` block](blocks/guard.md)) | `403 Forbidden` when the expression evaluates to false. |
| `process[*].where` on `db.find_*` | Filter rows on the database side. Translated to SQL at build time and bound into the prepared statement at request time. |
| `models/*.json` `fields.<col>.validate` | `422 Unprocessable Content` when the expression is false on an inbound payload. |
| `routes/*.json` `input.*.<field>.validate` | `422 Unprocessable Content` on the parsed input value. |

Authorization is a block, not a route-level field — see the
[`guard` block](blocks/guard.md) and the
[design rationale](decisions.md#authorization-a-block-not-a-route-level-field).

## Examples

<!-- rb:route -->
```jsonc
// routes/admin-posts.json
{
  "path": "/admin/posts",
  "method": "GET",
  "kind": "page",
  "template": "admin/posts.html",
  "process": [
    { "block": "guard", "if": "user.is_admin" },
    {
      "name": "posts",
      "block": "db.find_many",
      "table": "posts",
      "where": "post.author_id == user.id"
    }
  ]
}
```

<!-- rb:model -->
```jsonc
// models/post.json
{
  "name": "Post",
  "table": "posts",
  "fields": {
    "title": {
      "type": "string",
      "max_length": 200,
      "validate": "length(title) >= 1 && length(title) <= 200"
    }
  }
}
```

## Build-time guarantees

- The expression parses as CEL (`cel-interpreter::Program::compile`).
- Empty expressions are rejected.
- The parser is wrapped in `catch_unwind` so a panic on malformed input
  surfaces as a structured manifest error rather than crashing the build.
- **Scope is enforced.** Each CEL site declares which identifiers it can
  reference, and unknown references fail the build:
  - `input.*.<field>.validate` → only the field's own name.
  - `models/*.json` `fields.<col>.validate` → only the column's name.
  - `process[*]` `guard.if` → the route's input top-level names plus
    every `$<name>` already bound by a prior block.
  - `process[*]` `db.find_*.where` (string form) → the target table's
    column names.
- The string form of `where:` is also fed through the SQL translator at
  build time (`src/sql_where.rs`). Operators outside the supported
  subset (`==`, `!=`, `<`, `<=`, `>`, `>=`, `&&`, `||`, `in [..]`) fail
  the build with a pointer at the feature.

## Runtime

- `guard` block ⇒ `403 Forbidden` (page = plain text, api = JSON
  `{"error":{"code":"forbidden"}}`). Context = the route's input fields
  plus every `$<name>` bound by a prior block, all under their own names.
- `input.*.<field>.validate` ⇒ a `FieldError` in the 422 response.
- `models/*.json` `fields.<col>.validate` ⇒ checked at `db.insert` time;
  failure short-circuits the handler with `422 Unprocessable Content`
  and the offending field name.
- The translated `where:` fragment is bound into the prepared statement
  by `db.find_many` / `db.find_one` against the wired pool.

## Not yet implemented

- User-defined CEL functions in JSON (v2).
- Cross-route expression reuse (v2).
