# Expressions (CEL)

rublocks uses [Common Expression Language][cel] (CEL) as its sub-language
for guards, filters, validators, and view conditionals. CEL is
non-Turing-complete, side-effect-free, and used in production by
Kubernetes admission controllers and Envoy.

Every CEL snippet is **syntactically validated at build time**. Invalid
expressions fail the build with a manifest error that names the offending
file and field. Runtime evaluation (against a typed `user` / row /
request context) is wired once process-block execution lands — see
issue #11 for the open work.

[cel]: https://github.com/google/cel-spec

## Where CEL appears

| JSON site | Purpose |
|-----------|---------|
| `process[*]` `guard.if` (see [`guard` block](blocks/guard.md)) | `403 Forbidden` when the expression evaluates to false. |
| `process[*].where` on `db.find_*` | Filter rows on the database side. Will be translated to SQL by the runtime layer. |
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

Type-checking against the runtime context (e.g. "`user.is_admin` must
exist") happens at runtime today; an offline type-check is on the
roadmap.

## Not yet implemented

- Runtime evaluation of `guard` / `validate` / view conditionals
  (handlers are stubs in slice 4).
- SQL translation of `process[*].where` — the expression is parsed and
  stored on the block, but no SQL is emitted yet.
- User-defined CEL functions in JSON (v2).
- Cross-route expression reuse (v2).
