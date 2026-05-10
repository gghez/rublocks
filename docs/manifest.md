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

Future versions will accept additional declarative files alongside `main.json`:

```
my-project/
├── main.json
├── routes/
│   ├── users.json
│   └── posts.json
├── models/
│   └── user.json
└── jobs/
    └── send-email.json
```

Each domain has its own schema. The compiler will discover these files automatically.

This is **not implemented yet** — only `main.json` is read today.
