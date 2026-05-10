# rublocks

Declarative JSON language that compiles to Rust/Axum web applications. Designed to be authored primarily by coding agents — declare your app in JSON, get a clean Rust project.

**Status: pre-alpha. Not usable yet.**

## Concept

You declare your app in a handful of JSON files. `rublocks build` emits a clean Rust/Axum project under `./dist`: typed structs, wired services, a router, idiomatic async `main` — code you would have written by hand.

<table>
<tr>
<th>What you write</th>
<th>What rublocks generates</th>
</tr>
<tr>
<td>

```json
// main.json
{
  "name": "myblog",
  "services": {
    "postgres": { "url": "env:DATABASE_URL" }
  }
}
```

```json
// models/post.json
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

</td>
<td>

```rust
// dist/src/main.rs (generated, excerpt)
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

</td>
</tr>
</table>

Then:

```bash
rublocks build   # generates a Rust/Axum project under ./dist
rublocks run     # build + cargo run
```

> The example above reflects what slice 2 emits today: model structs, service wiring, router skeleton, `/health`. Route handlers declared under `routes/` currently compile as stubs — see the roadmap.

## Roadmap

- **v0** — CLI skeleton, `main.json` parsing, Cargo project generation with optional postgres/redis wiring, `/health` endpoint.
- **v1** — Route declarations (HTTP methods, paths, handlers).
- **v2** — Model and migration declarations.
- **v3** — Background jobs.

## License

MIT
