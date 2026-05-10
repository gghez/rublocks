# rublocks

Declarative JSON language that compiles to Rust/Axum web applications. Designed to be authored primarily by coding agents — declare your app in JSON, get a clean Rust project.

**Status: pre-alpha. Not usable yet.**

## Concept

You declare your app in a handful of JSON files. `rublocks build` emits a clean Rust/Axum project under `./dist`: typed structs, wired services, a router, idiomatic async `main` — code you would have written by hand.

**What you write — `main.json`:**

```json
{
  "name": "myblog",
  "services": {
    "postgres": { "url": "env:DATABASE_URL" }
  }
}
```

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
    #[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
    pub struct Post {
        pub id: uuid::Uuid,
        pub slug: String,
        pub title: String,
        pub body: String,
        pub published_at: Option<chrono::DateTime<chrono::Utc>>,
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

> The example above reflects what slice 2 emits today: model structs, service wiring, router skeleton, `/health`. Route handlers declared under `routes/` currently compile as stubs — see the roadmap.

## Dev workflow

`rublocks dev` is the iteration loop. One command runs codegen, `cargo build`, the generated binary, a file watcher, and a livereload bridge for the browser.

- **Watch** — `*.json` and `*.html` under the project (excluding `dist/`), debounced 300 ms, deduplicated by content hash so a no-op re-save does nothing.
- **Rebuild** — file change → kill child → codegen → `cargo build` → respawn. ~0.4 s on a warm cargo cache.
- **Livereload** — open browser tabs reconnect after every restart via SSE at `/__rublocks/events` and trigger `location.reload()`.
- **Ephemeral services** — for any `postgres` / `redis` service declared via `env:VAR` that isn't set, `rublocks dev` provisions a labelled Docker container with a persistent volume, injects the resolved URL into the child, and `docker stop`s it cleanly on `Ctrl+C`.
- **Browser-first errors** — codegen panics, manifest parse errors, and `cargo build` failures render in the browser with file, line, and the offending snippet — not just in the terminal.

See [`docs/dev-mode.md`](docs/dev-mode.md) for the full protocol.

## Roadmap

- **v0** — CLI skeleton, `main.json` parsing, Cargo project generation with optional postgres/redis wiring, `/health` endpoint.
- **v1** — Route declarations (HTTP methods, paths, handlers).
- **v2** — Model and migration declarations.
- **v3** — Background jobs.

## License

MIT
