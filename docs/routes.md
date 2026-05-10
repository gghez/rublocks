# Routes (`routes/*.json`)

Each file under `<project>/routes/` declares one HTTP endpoint. Files are discovered recursively; subdirectories are allowed (e.g. `routes/admin/users.json`). The compiler derives a unique handler name from the file path stem (`/` and `-` and `.` become `_`).

The `kind` field decides whether the route renders HTML (`page`) or JSON (`api`).

## Schema

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
| `layout` | string | no | Layout name (matches `layouts/<name>.json`). Wired by the template-rendering slice. |

Unknown fields (`input`, `process`, `view`, `output`, `redirect`, `summary`, `description`, `tags`, `on_missing`, ...) are accepted silently — they belong to later v1 slices and are documented in [vision.md](vision.md).

## Discovery rules

- The `routes/` directory is optional; a project without any routes serves only `/health`.
- Files are sorted by path before parsing, so generated code is deterministic.
- Two routes with the same `(method, path)` pair are rejected at load time.

## Dev placeholder interaction

The dev-mode placeholder at `GET /` is suppressed when a user route already owns it. See [dev-mode.md](dev-mode.md).

## Slice status

- **Slice 1 (current)** — discovery + dispatch only. Each handler returns a placeholder string identifying itself; templates and `process` blocks are not yet executed.
- **Next** — Askama template rendering (`page` routes).
- **Then** — `input` parsing, `process` blocks (`db.find_many`, `db.find_one`, `db.insert`), `view` / `output` mapping, `redirect`.
