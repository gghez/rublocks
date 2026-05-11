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
| `process` | array | no | Declared blocks: `{ name, block, table, ... }`. Slice 3 reads only `name`/`block`/`table` for type inference; execution lands in slice 5. |
| `view` | object | no | Map of `<page-variable> → "<literal>" \| "$<ref>" \| "$<ref>.<field>"`. Literals are baked into the handler; `$ref` values typecheck against `process` blocks. |

Unknown fields (`input`, `output`, `redirect`, `summary`, `description`, `tags`, `on_missing`, ...) are accepted silently — they belong to later v1 slices and are documented in [vision.md](vision.md).

## Discovery rules

- The `routes/` directory is optional; a project without any routes serves only `/health`.
- Files are sorted by path before parsing, so generated code is deterministic.
- Two routes with the same `(method, path)` pair are rejected at load time.

## Dev placeholder interaction

The dev-mode placeholder at `GET /` is suppressed when a user route already owns it. See [dev-mode.md](dev-mode.md).

## Slice status

- **Slice 1** — discovery + dispatch (handler stubs).
- **Slice 2** — model struct generation (`mod models`).
- **Slice 3 (current)** — Askama template rendering for `kind: page` GET routes. See [templates.md](templates.md) and [layouts.md](layouts.md). Layouts wire via `{% extends %}`; the page context is built from `layout.requires` + `layout.view` + `route.view`; literals are baked, references default. Livereload is injected when `RUBLOCKS_DEV=1`.
- **Next** — `input` parsing, `process` block execution (`db.find_many`, `db.find_one`, `db.insert`), `view` / `output` mapping fed by process results, `redirect`, `on_missing`.
