# rublocks

[![CI](https://github.com/gghez/rublocks/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/gghez/rublocks/actions/workflows/ci.yml)

Declarative JSON language that compiles to Rust/Axum web applications. Designed to be authored primarily by coding agents — declare your app in JSON, get a clean Rust project.

**Status: pre-alpha. Not usable yet.**

## Concept

The name says it: **rublocks = rust blocks**. You compose a route out of small declarative *blocks* — each one a self-contained unit of logic with a standardised input/output contract. `rublocks build` then emits a clean Rust/Axum project under `./dist`: typed structs, wired services, a router, idiomatic async `main` — code you would have written by hand. See [`docs/blocks/`](docs/blocks/README.md) for the block catalogue.

**What you write — `main.json`:**

```json
{
  "name": "myblog",
  "version": "0.1.0",
  "description": "A blog with public posts and admin moderation.",
  "language": "en-US",
  "services": {
    "postgres": { "url": "env:DATABASE_URL" }
  }
}
```

`language` is a required BCP 47 tag — see [`docs/manifest.md`](docs/manifest.md#language) for the rationale and the locales the dev-mode error overlay ships strings for.

**And `models/post.json`:**

```json
{
  "name": "Post",
  "table": "posts",
  "fields": {
    "id":           { "type": "uuid",        "primary_key": true },
    "slug":         { "type": "string",      "max_length": 200, "unique": true },
    "title":        { "type": "string",      "max_length": 200 },
    "body":         { "type": "text" },
    "published_at": { "type": "timestamptz", "nullable": true }
  }
}
```

**What rublocks generates — `dist/src/main.rs` (excerpt):**

```rust
use axum::{routing::get, Router};

pub mod models {
    #[derive(Debug, Clone, Default, serde::Serialize, sqlx::FromRow)]
    pub struct Post {
        pub id: uuid::Uuid,
        pub slug: String,
        pub title: String,
        pub body: String,
        // `nullable: true` fields use a tiny wrapper that's transparent for
        // serde/sqlx but renders as the inner `Display` (or empty) under
        // Askama — see docs/templates.md.
        pub published_at: crate::_rb_util::NullDisplay<chrono::DateTime<chrono::Utc>>,
    }
}

#[derive(Clone)]
pub struct AppState {
    pub pg: sqlx::PgPool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let pg = sqlx::PgPool::connect(&std::env::var("DATABASE_URL")?).await?;
    let state = AppState { pg };
    let app = Router::new()
        .route("/health", get(health))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> &'static str { "ok" }
```

Then:

```bash
rublocks build   # generate ./dist (codegen only — no cargo build)
rublocks dev     # build, run, watch sources, livereload the browser
```

> Slice 3 ships Askama rendering for `kind: page` GET routes on top of the example above. Each page route emits a typed context struct in a `ctx_<route>` module derived from `layout.requires` + `layout.view` + `route.view`; layouts wire via `{% extends %}`; literal view values are baked into the handler; `templates/` is mirrored to `dist/templates/` on every build; livereload is injected into rendered pages when `RUBLOCKS_DEV=1`. Process block execution (`db.find_many`, `db.find_one`, `db.insert`) lands in slice 5 — see [`docs/templates.md`](docs/templates.md) and [`docs/layouts.md`](docs/layouts.md).

## Dev workflow

`rublocks dev` is the iteration loop. One command runs codegen, `cargo build`, the generated binary, a file watcher, and a livereload bridge for the browser.

- **Watch** — `*.json` and `*.html` under the project (excluding `dist/`), debounced 300 ms, deduplicated by content hash so a no-op re-save does nothing.
- **Rebuild** — file change → kill child → codegen → `cargo build` → respawn. ~0.4 s on a warm cargo cache.
- **Livereload** — open browser tabs reconnect after every restart via SSE at `/__rublocks/events` and trigger `location.reload()`.
- **Ephemeral services** — for any `postgres` / `redis` service declared via `env:VAR` that isn't set, `rublocks dev` provisions a labelled Docker container with a persistent volume, injects the resolved URL into the child, and `docker stop`s it cleanly on `Ctrl+C`.
- **Browser-first errors** — codegen panics, manifest parse errors, and `cargo build` failures render in the browser with file, line, and the offending snippet — not just in the terminal.

See [`docs/dev-mode.md`](docs/dev-mode.md) for the full protocol.

## Agent integration

rublocks is meant to be authored by coding agents, not by humans writing JSON by hand. Every `rublocks build` refreshes three per-project files so any agent that opens the repository immediately knows the JSON shapes and conventions of the binary that produced them:

- `.claude/skills/rublocks/SKILL.md` — autoloaded Claude skill.
- `AGENTS.md` — rublocks-managed block (delimited by `<!-- rublocks:start --> / <!-- rublocks:end -->`) for Codex and other AGENTS.md consumers; preserves user-authored content above and below the block.
- `.cursor/rules/rublocks.mdc` — Cursor rule with `alwaysApply: true`.

All three embed the same body: project tour, canonical examples, field-type table, conventions, dev workflow, and the full Draft-07 JSON schemas for `main.json`, `models/*.json`, `routes/*.json`, and `layouts/*.json` (derived from the parsing types via `schemars`, so they cannot drift from what the compiler actually accepts).

No install command, no global setup: the artifacts ship with the project. A `git clone` is the only step a teammate or another agent needs.

See [`docs/agents.md`](docs/agents.md) for the full reference.

## Roadmap

- **v0** — CLI skeleton, `main.json` parsing, Cargo project generation with optional postgres/redis wiring, `/health` endpoint.
- **v1** — Route declarations (HTTP methods, paths, handlers).
- **v2** — Model and migration declarations.
- **v3** — Background jobs.

## License

MIT
