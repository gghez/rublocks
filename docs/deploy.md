# Deploy

Every `rublocks build` emits a `Dockerfile`, `docker-compose.yml` and
`.dockerignore` under `dist/`. Together they're enough to bring the app
up reproducibly from a fresh clone.

## Quickstart

```bash
rublocks build
cd dist
docker compose up --build
```

Once the backend services report `service_healthy`, the app binds
`http://localhost:3000`.

## What gets emitted

### `dist/Dockerfile`

Multi-stage build:

1. **builder** — `rust:1-slim`, installs the build deps, runs
   `cargo build --release --locked` with `SQLX_OFFLINE=true`.
2. **runtime** — `debian:bookworm-slim`, runs as the non-root `app:app`
   user, copies the release binary + `templates/` + `migrations/`, exposes
   port 3000, and ships a `HEALTHCHECK` that hits `/health`.

### `dist/docker-compose.yml`

One service per backend declared in `main.json`, plus the app:

| `services.*` | Compose service | Image | Volume |
|--------------|-----------------|-------|--------|
| `db.kind: postgres` | `postgres` | `postgres:16-alpine` | `postgres_data` |
| `db.kind: mysql`    | `mysql`    | `mysql:8`            | `mysql_data` |
| `db.kind: mariadb`  | `mysql`    | `mariadb:11`         | `mysql_data` |
| `db.kind: mssql`    | _(none — sqlx 0.8 has no driver; user-managed)_ | — | — |
| `redis`             | `redis`    | `redis:7-alpine`     | `redis_data` |

`depends_on` chains the app on `condition: service_healthy` so the
binary never tries to connect before the database is ready. Migrations
are applied via `./<app> migrate` (see [migrations.md](migrations.md));
the compose does not invoke it explicitly — run it once after the first
`compose up` to seed the schema.

### `dist/.dockerignore`

Excludes `target/`. The builder stage rebuilds from `Cargo.toml + src/`.

## Environment variables

When `main.json` references a service URL via `env:VAR_NAME`, the compose
sets `VAR_NAME` on the `app` service to the in-network URL of the bundled
container (e.g. `postgres://rublocks:rublocks@postgres:5432/<db>`). The
user can still override at deploy time by setting the variable in the
host environment that runs `docker compose`.

## Reverse proxy

The generated stack is opinionated about port 3000 inside the network.
TLS termination / multi-instance load balancing / static-file caching
should live in a separate reverse-proxy container — see issue #4 for the
declarative middleware + reverse-proxy config story.
