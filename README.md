# rublocks

[![CI](https://github.com/gghez/rublocks/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/gghez/rublocks/actions/workflows/ci.yml)

Declarative JSON language that compiles to Rust/Axum web applications. Designed to be authored primarily by coding agents — declare your app in JSON, get a clean Rust project.

**Status: pre-alpha — the runtime is in place, the surface is still evolving.**

## Concept

The name says it: **rublocks = rust blocks**. You compose a route out of small declarative *blocks* — each one a self-contained unit of logic with a standardised input/output contract. `rublocks build` then emits a clean Rust/Axum project under `./dist`: typed structs, wired services, a router, idiomatic async `main` — code you would have written by hand. See [`docs/blocks/`](docs/blocks/README.md) for the block catalogue.

**What you write — `main.json`:**

```json
{
  "name": "myblog",
  "version": "0.1.0",
  "description": "A blog with public posts and admin moderation.",
  "language": "en-US",
  "encoding": "utf-8",
  "logging": { "level": "info" },
  "services": {
    "postgres": { "url": "env:DATABASE_URL" }
  }
}
```

`language` is a required BCP 47 tag — see [`docs/manifest.md`](docs/manifest.md#language) for the rationale and the locales the dev-mode error overlay ships strings for. `encoding` is required and currently only accepts `"utf-8"` — see [`docs/encoding.md`](docs/encoding.md) for the policy. `logging` is required and configures the structured NDJSON pipeline emitted to stdout — see [`docs/logging.md`](docs/logging.md).

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

> `kind: page` GET routes render Askama templates on top of the example above. Each page route emits a typed context struct in a `ctx_<route>` module derived from `layout.requires` + `layout.view` + `route.view`; layouts wire via `{% extends %}`; literal view values are baked into the handler; `templates/` is mirrored to `dist/templates/` on every build; livereload is injected into rendered pages when `RUBLOCKS_DEV=1`. Process blocks (`db.find_many`, `db.find_one`, `db.insert`, `guard`, `time.now`, `error`, `sftp.list`, `sftp.read`, `csv.read`, `csv.write`) execute at request time against the wired database pool / SFTP target — see [`docs/templates.md`](docs/templates.md), [`docs/layouts.md`](docs/layouts.md), and [`docs/blocks/`](docs/blocks/README.md). The `csv.*` family carries CSV import/export end-to-end: a `db.find_many` → `csv.write` chain ships rows as a CSV body, and a `sftp.read` → `csv.read` chain imports a remote drop into a model-typed `Vec<T>`.

## Why not just generate Rust directly?

The usual reflex when looking at this is "why not just have the agent emit Rust?" Three reasons rublocks is the JSON-shaped middle layer:

- **Typed slots beat free-form code.** LLMs reason far more reliably about filling in well-shaped JSON fields than about Axum extractors, sqlx macros, lifetimes, and trait gymnastics. The block catalogue is the language; the agent's job collapses to picking the right block and filling its slots.
- **Canonical JSON ⇒ idempotent output.** The same intent always maps to the same Rust output. An agent can re-emit a route after a tiny edit without churning the surrounding generated code — no diff noise, no regression risk in unrelated handlers.
- **The escape hatch is just `dist/`.** The generated Rust is idiomatic, readable, and `cargo build`-able. If rublocks ever gets in the way, the project lives on as a normal Rust crate — no lock-in, no runtime, no interpreter.

See [`docs/vision.md`](docs/vision.md) for the longer version, and [`docs/decisions.md`](docs/decisions.md) for the per-choice rationale.

## Install

### Standalone installers (recommended)

The shell and PowerShell installers detect your OS and architecture, download the matching archive, verify its SHA-256 checksum, and drop the `rublocks` binary into `~/.rublocks/bin` (or `$RUBLOCKS_HOME/bin` if set).

**Linux / macOS:**

```bash
curl -LsSf https://github.com/gghez/rublocks/releases/latest/download/install.sh | sh
```

**Windows:**

```powershell
powershell -c "irm https://github.com/gghez/rublocks/releases/latest/download/install.ps1 | iex"
```

**Pin a specific version** — swap `latest/download` for `download/v<x.y.z>`:

```bash
curl -LsSf https://github.com/gghez/rublocks/releases/download/v0.1.0/install.sh | sh
```

```powershell
powershell -c "irm https://github.com/gghez/rublocks/releases/download/v0.1.0/install.ps1 | iex"
```

Add `~/.rublocks/bin` (or its Windows equivalent) to your `PATH` — the installer prints the exact line to copy if it's missing.

### Manual download

Every tagged release on the [Releases page](https://github.com/gghez/rublocks/releases) attaches prebuilt archives for Linux (x86_64 gnu + musl), macOS (x86_64 + arm64), and Windows (x86_64), plus a `SHA256SUMS` file covering every archive.

### From source

Requires a stable Rust toolchain:

```bash
cargo install --git https://github.com/gghez/rublocks --tag v0.1.0 rublocks
```

## Dev workflow

`rublocks dev` is the iteration loop. One command runs codegen, `cargo build`, the generated binary, a file watcher, and a livereload bridge for the browser.

- **Watch** — `*.json` and `*.html` under the project (excluding `dist/`), debounced 300 ms, deduplicated by content hash so a no-op re-save does nothing.
- **Rebuild** — file change → kill child → codegen → `cargo build` → respawn. ~0.4 s on a warm cargo cache.
- **Livereload** — open browser tabs reconnect after every restart via SSE at `/__rublocks/events` and trigger `location.reload()`.
- **Ephemeral services** — for any `postgres` / `redis` service declared via `env:VAR` that isn't set, `rublocks dev` provisions a labelled Docker container with a persistent volume, injects the resolved URL into the child, and `docker stop`s it cleanly on `Ctrl+C`.
- **Dotenv loading** — a `.env` next to `main.json` is loaded by default at startup (both for `rublocks dev` and the generated binary), so `env:VAR` references resolve without a manual `export`. Disable with `"load_dotenv": false`, or point at a custom file with `"load_dotenv": "<path>"`. See [`docs/manifest.md`](docs/manifest.md#dotenv-loading).
- **Service catalogue** — `services.db` (Postgres / MySQL / MariaDB / MSSQL), `services.redis`, and `services.<name>: { kind: "sftp", ... }` for SFTP targets — see [`docs/manifest.md`](docs/manifest.md) and [`docs/blocks/sftp.md`](docs/blocks/sftp.md).
- **Browser-first errors** — codegen panics, manifest parse errors, and `cargo build` failures render in the browser with file, line, and the offending snippet — not just in the terminal.
- **UTF-8 everywhere** — declared in `main.json`, enforced at every HTTP boundary, on every project-file read, and on the Postgres session. See [`docs/encoding.md`](docs/encoding.md).
- **Structured logs on stdout** — declared in `main.json.logging`, one compact JSON object per event. Per-block events carry `block`, `duration_us`, and the block's static metadata; per-request events carry `request_id`, `method`, `path`, `route`. See [`docs/logging.md`](docs/logging.md).

A representative line emitted by a generated app:

```
{"timestamp":"2026-05-11T18:34:21.842Z","level":"INFO","fields":{"message":"ok","duration_us":4123},"target":"rublocks::block","span":{"block":"db.find_many","table":"posts","name":"block"},"spans":[{"request_id":"01HXY...","method":"GET","route":"/api/posts","name":"http_request"},{"block":"db.find_many","table":"posts","name":"block"}]}
```

See [`docs/dev-mode.md`](docs/dev-mode.md) for the full protocol.

## Local git hooks

The repo ships hooks under `.githooks/` that mirror the CI gate so failures land in your shell, not in a red check.

```bash
./scripts/install-hooks.sh
```

This sets `core.hooksPath = .githooks`. Hooks installed:

- `pre-commit` — `cargo fmt --all --check`, `cargo clippy --all-targets --all-features --locked -- -D warnings`.
- `pre-push` — `cargo test --locked --all-targets`.

`cargo audit` / `cargo deny` stay CI-only: they're slow and react to upstream advisories rather than to your changes.

Codegen carries a snapshot test layer (`src/snapshots/*.snap`). After an intentional codegen change, run `cargo insta review` (install once with `cargo install cargo-insta`) to inspect and accept the diff. See [`docs/testing.md`](docs/testing.md).

## Agent integration

rublocks is meant to be authored by coding agents, not by humans writing JSON by hand. Every `rublocks build` refreshes three per-project files so any agent that opens the repository immediately knows the JSON shapes and conventions of the binary that produced them:

- `.claude/skills/rublocks/SKILL.md` — autoloaded Claude skill.
- `AGENTS.md` — rublocks-managed block (delimited by `<!-- rublocks:start --> / <!-- rublocks:end -->`) for Codex and other AGENTS.md consumers; preserves user-authored content above and below the block.
- `.cursor/rules/rublocks.mdc` — Cursor rule with `alwaysApply: true`.
- `.rublocks/schemas/*.schema.json` + `.vscode/settings.json` — Draft-07 JSON Schemas on disk plus the `json.schemas[]` mapping that wires every rublocks JSON file (`main.json`, `models/*.json`, `routes/**/*.json`, `layouts/*.json`) to its schema. VS Code, Zed, and other JSON-aware editors get autocomplete and in-place validation out of the box. Unrelated settings keys in an existing `.vscode/settings.json` are preserved.

All three agent artifacts embed the same body: project tour, canonical examples, field-type table, conventions, dev workflow, and the full Draft-07 JSON schemas for `main.json`, `models/*.json`, `routes/*.json`, and `layouts/*.json` (derived from the parsing types via `schemars`, so they cannot drift from what the compiler actually accepts).

No install command, no global setup: the artifacts ship with the project. A `git clone` is the only step a teammate or another agent needs.

See [`docs/agents.md`](docs/agents.md) for the full reference.

## What's next

- `rublocks new` (scaffolding) and `rublocks run` (build-then-run without watching).
- Background jobs (`jobs/*.json`).

Open issues track everything finer-grained — see the [GitHub issue tracker](https://github.com/gghez/rublocks/issues).

## License

MIT
