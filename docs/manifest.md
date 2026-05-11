# Manifest reference (`main.json`)

The entry point of every rublocks project. Lives at the project root.

## Schema (current)

```json
{
  "name": "myapp",
  "services": {
    "postgres": { "url": "env:DATABASE_URL" },
    "redis":    { "url": "env:REDIS_URL" }
  }
}
```

### Fields

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `name` | string | yes | Lowercase ASCII letters, digits, `_`, `-`. Used as the generated cargo crate name. |
| `services` | object | no | Optional service declarations. |
| `services.postgres` | object | no | Adds `sqlx::PgPool` to `AppState`. |
| `services.postgres.url` | string | yes (if `postgres` set) | Connection URL — see [URL syntax](#url-syntax). |
| `services.redis` | object | no | Adds `deadpool_redis::Pool` to `AppState`. |
| `services.redis.url` | string | yes (if `redis` set) | Connection URL — see [URL syntax](#url-syntax). |

## URL syntax

Service URLs accept two forms:

| Form | Generated code |
|------|----------------|
| `"postgres://..."` (literal) | The string is embedded directly. |
| `"env:VAR_NAME"` | `std::env::var("VAR_NAME")?` at startup. |

The `env:` prefix is the recommended form for any secret/connection-string-like value.

## Multi-file plan

Additional declarative files live alongside `main.json`. The `playground/` blog example follows this layout:

```
my-project/
├── main.json
├── models/            # one JSON per entity (table + fields + indexes)
│   └── post.json
├── migrations/        # versioned SQL, hand-authored or generated from model diffs
│   └── 0001_init.sql
├── layouts/           # shared template + context (master pages)
│   └── main.json
├── routes/            # one JSON per HTTP route (page or api) -- see routes.md
│   └── home.json
├── templates/         # Askama HTML; .html files referenced by routes/layouts
│   └── home.html
└── jobs/              # background work (not yet sketched)
    └── send-email.json
```

Each domain has its own schema. The compiler discovers these files automatically.

Implemented today:

- `routes/*.json` — discovery + dispatch + Askama rendering for `kind: page` GET routes ([reference](routes.md)).
- `models/*.json` — struct generation ([reference](models.md)).
- `layouts/*.json` — parsing + `requires`/`view` projection into the page context ([reference](layouts.md)).
- `templates/*.html` — copied to `dist/templates/` and consumed by Askama at compile time ([reference](templates.md)).

Not yet implemented: `migrations/`, `process` block execution, `input` parsing, `jobs/`.
