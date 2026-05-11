# Manifest reference (`main.json`)

The entry point of every rublocks project. Lives at the project root.

## Schema (current)

<!-- rb:manifest -->
```json
{
  "name": "myapp",
  "version": "0.1.0",
  "description": "A blog with public posts and admin moderation.",
  "services": {
    "db":    { "kind": "postgres", "url": "env:DATABASE_URL" },
    "redis": { "url": "env:REDIS_URL" }
  }
}
```

### Fields

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `name` | string | yes | Lowercase ASCII letters, digits, `_`, `-`. Used as the generated cargo crate name. |
| `version` | string | yes | [SemVer 2.0.0](https://semver.org/spec/v2.0.0.html) — e.g. `"0.1.0"`, `"1.4.2-rc.1"`. Single source of truth for `Cargo.toml` `package.version`, OpenAPI `info.version`, the `X-App-Version` response header, and the dev-mode error page footer. No fallback default. |
| `description` | string | yes | Single-line synopsis. Trimmed; max 280 characters; no newlines. Threaded to `Cargo.toml`, the dev-mode overlay, and the OpenAPI `info.description` (once the spec emitter ships). |
| `services` | object | no | Optional service declarations. |
| `services.db` | object | no | Database service — explicit `kind` + `url`. Preferred over the legacy `services.postgres`. |
| `services.db.kind` | string | no | One of `postgres` (default), `mysql`, `mariadb`, `mssql`. |
| `services.db.url` | string | yes (if `db` set) | Connection URL — see [URL syntax](#url-syntax). |
| `services.postgres` | object | no | Legacy shorthand equivalent to `{ "db": { "kind": "postgres", ... } }`. Setting both `db` and `postgres` is rejected. |
| `services.redis` | object | no | Adds `deadpool_redis::Pool` to `AppState`. |
| `services.redis.url` | string | yes (if `redis` set) | Connection URL — see [URL syntax](#url-syntax). |

### Backends

| `kind` | sqlx pool type | sqlx feature | UUID column | TEXT column | bool column | TIMESTAMPTZ column |
|--------|----------------|--------------|-------------|-------------|-------------|--------------------|
| `postgres` | `sqlx::PgPool` | `postgres` | `UUID` | `TEXT` | `BOOLEAN` | `TIMESTAMPTZ` |
| `mysql` | `sqlx::MySqlPool` | `mysql` | `BINARY(16)` | `LONGTEXT` | `TINYINT(1)` | `DATETIME` |
| `mariadb` | `sqlx::MySqlPool` | `mysql` | `BINARY(16)` | `LONGTEXT` | `TINYINT(1)` | `DATETIME` |
| `mssql` | `sqlx::MssqlPool` | `mssql` | `UNIQUEIDENTIFIER` | `NVARCHAR(MAX)` | `BIT` | `DATETIMEOFFSET` |

`mssql` is currently parsed and the dialect maps the column types correctly, but `sqlx 0.8` dropped the official MSSQL driver — projects using `kind: "mssql"` will fail to compile until a replacement driver lands. See issue #9 for the follow-up.

## HTTP middleware

`main.json.http` declares an opt-in set of `tower-http` layers wired
around the generated Axum router. Anything not set produces no extra
dependencies and no layer:

<!-- rb:manifest -->
```json
{
  "name": "myblog",
  "version": "0.1.0",
  "description": "A blog with public posts and admin moderation.",
  "http": {
    "compression": true,
    "cors": { "origins": ["https://example.com"] },
    "timeout_ms": 30000,
    "security_headers": true
  }
}
```

| Field | Effect |
|-------|--------|
| `compression` | `tower_http::compression::CompressionLayer` (gzip + brotli + zstd by `Accept-Encoding`). |
| `cors.origins` | `tower_http::cors::CorsLayer`. `["*"]` allows any origin (and any method/header). |
| `timeout_ms` | `tower_http::timeout::TimeoutLayer`. |
| `security_headers` | Static headers: `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`, `Referrer-Policy: strict-origin-when-cross-origin`, `Strict-Transport-Security: max-age=31536000; includeSubDomains`. |

Layers are stacked in declaration order via `Router::layer`. See
[`deploy.md`](deploy.md) for when to put a real reverse proxy in front
and when to rely on these layers alone.

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
- `models/*.json` — struct generation, full table-level `indexes`/`foreign_keys`/`checks` parsing with field-level shorthand ([reference](models.md)).
- `layouts/*.json` — parsing + `requires`/`view` projection into the page context ([reference](layouts.md)).
- `templates/*.html` — copied to `dist/templates/` and consumed by Askama at compile time ([reference](templates.md)).
- `migrations/` — forward-only DDL generated on every build from `models/*.json` diffs ([reference](migrations.md)).

Not yet implemented: `process` block execution, `input` parsing, `jobs/`.
