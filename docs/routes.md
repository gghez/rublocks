# Routes (`routes/*.json`)

Each file under `<project>/routes/` declares one HTTP endpoint. Files are discovered recursively; subdirectories are allowed (e.g. `routes/admin/users.json`). The compiler derives a unique handler name from the file path stem (`/` and `-` and `.` become `_`).

The `kind` field decides whether the route renders HTML (`page`) or JSON (`api`).

## Schema

<!-- rb:route -->
```json
{
  "path": "/posts/:slug",
  "method": "GET",
  "kind": "page",
  "template": "posts/show.html",
  "layout": "main"
}
```

### Fields recognised today

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `path` | string | yes | Must start with `/`. Path parameters use `:name` syntax (translated to Axum's matchit form). |
| `method` | enum | yes | `GET` \| `POST` \| `PUT` \| `DELETE` \| `PATCH`. |
| `kind` | enum | yes | `page` (renders a template) or `api` (returns JSON). |
| `template` | string | yes for `kind: page` (GET) | Path under `templates/`, e.g. `home.html` or `posts/show.html`. |
| `layout` | string | no | Layout name (matches `layouts/<name>.json`). Cross-checked at manifest load. See [layouts.md](layouts.md). |
| `process` | array | no | Ordered list of [blocks](blocks/README.md). Each entry is dispatched against the block registry (`src/blocks/`) — unknown ids and unknown per-block fields are rejected at load time. The full per-block schema lives in `docs/blocks/<id>.md`. |
| `view` | object | no | Map of `<page-variable> → "<literal>" \| "$<ref>" \| "$<ref>.<field>"`. Literals are baked into the handler; `$ref` values typecheck against `process` blocks. |
| `input` | object | no | Typed declaration of the route's path / query / body parameters. Each entry produces a validated extractor and feeds the `$input.*` references. See [input.md](input.md). |
| `output` | object | no | Map of `<output-key> → "<literal>" \| "$<ref>" \| nested object` used by `kind: api` routes to shape the JSON response. Nested objects recurse. |
| `redirect` | object | no | `{ "to": "/path/$ref/...", "status": 303 }`. Path segments may interpolate `$input.*` and `$<block>.<field>` references resolved at request time. |
| `on_missing` | block | no | Sub-block executed when a `db.find_one` returns no row at the route level (`route.on_missing`). Typically `error`. |
| `summary` / `description` / `tags` | string \| array | no | OpenAPI metadata for `kind: api` routes — see [openapi.md](openapi.md). |

Unknown route-level fields are rejected at load time.

## Discovery rules

- The `routes/` directory is optional; a project without any routes serves only `/health`.
- Files are sorted by path before parsing, so generated code is deterministic.
- Two routes with the same `(method, path)` pair are rejected at load time.

## Dev placeholder interaction

The dev-mode placeholder at `GET /` is suppressed when a user route already owns it. See [dev-mode.md](dev-mode.md).

## Where-clause grammar

`db.find_many` / `db.find_one` accept their `where:` filter in two
forms, both validated at build time and bound into the prepared
statement at request time.

**String form** — a CEL predicate, e.g. `"post.author_id == user.id"`.
Translated to SQL by `src/sql_where.rs`. The supported operator subset
is `==`, `!=`, `<`, `<=`, `>`, `>=`, `&&`, `||`, `in [..]`.

**Structured form** — an object map of `column -> value-or-operator`.
Multiple top-level keys are AND-joined.

```json
"where": { "slug": "$input.path.slug", "published_at": { "is_not_null": true } }
```

Operator objects on the right-hand side:

- `{ "is_null": true }` / `{ "is_not_null": true }` — null-aware.
- `{ "eq": <v> }`, `{ "ne": <v> }`, `{ "lt": <v> }`, `{ "le": <v> }`,
  `{ "gt": <v> }`, `{ "ge": <v> }` — comparisons.
- `{ "in": [<v>, ...] }` — membership; each element is a `$ref` or
  literal.

A literal RHS is sugar for `{ "eq": <v> }`. See `src/where_clause.rs`
for the canonical grammar reference.
