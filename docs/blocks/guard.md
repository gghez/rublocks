# `guard`

Authorize the current request against a CEL predicate. Insert anywhere in
`process`: the names the `if` expression can reference are exactly what
has been bound *before* this point — route input plus any `$<name>` from
prior blocks.

This is the **only** way to declare authorization. There is no
`route.guard` field; authorization is itself a block so the scope is
explicit and composes with the rest of the pipeline.

## Schema

```json
{
  "block": "guard",
  "if":    "user.is_admin"
}
```

| Field | Required | Type | Notes |
|-------|----------|------|-------|
| `block` | yes | `"guard"` | Discriminator. |
| `if` | yes | string | CEL predicate. Syntactically validated at build time. `false` ⇒ `403 Forbidden`. |

## Examples

### Early check — before any data load

```jsonc
{
  "path": "/admin/posts",
  "method": "GET",
  "kind": "page",
  "template": "admin/posts.html",
  "process": [
    { "block": "guard", "if": "user.is_admin" },
    { "name": "posts", "block": "db.find_many", "table": "posts" }
  ]
}
```

### Post-load check — after the resource is bound

```jsonc
{
  "path": "/posts/:slug/edit",
  "method": "GET",
  "kind": "page",
  "template": "posts/edit.html",
  "input": { "path": { "slug": { "type": "string", "required": true } } },
  "process": [
    {
      "name": "post",
      "block": "db.find_one",
      "table": "posts",
      "where": { "slug": "$input.path.slug" }
    },
    { "block": "guard", "if": "post.author_id == user.id" }
  ]
}
```

`post` is in scope because the preceding `db.find_one` bound it; the
guard rejects the request with `403` when the author check fails.

## Behaviour

- `kind: api` route — emits `application/json` with
  `{ "error": { "code": "forbidden" } }` and `403`.
- `kind: page` route — renders a plain `403` response (template-side
  surface lands with the process-execution slice).

## Output

None. The block either passes through (predicate is `true`) or
short-circuits the handler with `403`.

## Status

Parsing + syntactic CEL validation only. Runtime evaluation lands with
the process-execution slice.
