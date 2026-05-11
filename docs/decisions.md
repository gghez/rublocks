# Decisions

A running log of the design choices that shape rublocks. Entries are append-only; if a decision is reversed, add a new entry referencing the old one.

## Target framework: Axum

**Decision:** the generated Rust project uses `axum`.

**Why:** standard of the tokio ecosystem, idiomatic, easy to generate cleanly, large user base. Considered alternatives: `actix-web` (actor model harder to template), `rocket` (macro-heavy, opinionated).

## Language surface: declarative JSON

**Decision:** source files are JSON, not a custom DSL or YAML.

**Why:** target audience is coding agents. JSON has unambiguous parse semantics, schema validation is trivial, every model has been trained on millions of JSON documents. The verbosity that humans dislike is a feature here — every field is explicit.

## File structure: multi-file by domain

**Decision:** a project is a collection of JSON files, organized by domain (`main.json`, `routes/`, `models/`, etc.). Not one monolithic file.

**Why:** lets agents diff and modify single domains without touching the rest. Better for partial regeneration, smaller context windows, clearer ownership.

Currently only `main.json` is read; the multi-file plan is documented in [manifest.md](manifest.md#multi-file-plan).

## Tooling: dedicated CLI binary

**Decision:** rublocks ships as its own binary with subcommands (`new`, `build`, `run`, `dev`).

**Why:** clear UX, no dependency on `cargo` invocation patterns, simpler to install and document. Considered: `cargo` subcommand, build-script library.

## Codegen: `quote!` + `prettyplease`

**Decision:** Rust code is built as a `TokenStream` via `quote::quote!`, then parsed with `syn` and formatted with `prettyplease`.

**Why:** AST-based generation guarantees syntactic validity at compile time of the compiler itself. Output is consistently formatted. Avoids the bugs and unreadability of string templates. `Cargo.toml` is emitted as a string only because TOML has no quote-equivalent.

## Service URL syntax: literal or `env:VAR`

**Decision:** service URL fields accept either a literal connection string or `env:VAR_NAME` to defer resolution to runtime.

**Why:** `env:` is the obvious idiom for secrets. Keeping literals supported lets simple cases stay simple (e.g. local dev with a fixed URL).

## Project workflow: main-only, no pipeline

**Decision:** all work on `main`, no feature branches, no CI.

**Why:** project is in pre-alpha rapid construction by a single user/agent. Branching and CI ceremony would slow iteration without adding value yet. Will be revisited.

## Sandbox: `playground/`, user-controlled

**Decision:** a gitignored `playground/` directory exists for the user to test compiler outputs. The agent may not modify it without approval.

**Why:** the user needs a stable testing surface that captures their current generation experiment. If the agent rewrites it freely, the user loses their setup.

## Dev-mode dedup: content hash, not mtime

**Decision:** `rublocks dev` deduplicates rebuild triggers by hashing all project `*.json` files, not by mtime.

**Why:** during development of dev mode we observed `inotify` on WSL2 emitting repeated events for a single edit, often across multiple debounce windows, causing infinite rebuild loops. Content hashing tolerates these phantom events: a re-save with identical content does nothing. mtime would not be enough since the file's mtime changes on every Write tool invocation.

## Dev-mode reload protocol: SSE drop-then-reconnect

**Decision:** the browser livereload signal is "the SSE connection was dropped and then reconnected." No payload events on the SSE stream itself.

**Why:** simplest mechanism that requires no coordination between the supervisor and the generated app. The supervisor just kills the child; the client snippet detects the disconnect, retries, and reloads on successful reconnect. The dist binary doesn't need to know it's being supervised.

## OpenAPI generation: automatic via utoipa

**Decision:** every route declared with `kind: api` contributes automatically to a single OpenAPI 3 spec, generated at build time using the `utoipa` + `utoipa-axum` + `utoipa-swagger-ui` crates. The spec is served at `/openapi.json`, the interactive UI at `/docs`. Page routes are excluded.

**Why:** hand-written API docs drift the moment a handler changes. With rublocks emitting both the handler and the schema from the same JSON source, the spec is a derived artifact — it cannot lie. utoipa is the de-facto Rust/Axum standard, code-first (matches our codegen philosophy), and ships an `OpenApiRouter` that integrates registration with definition — there is no separate registry the agent could forget to update. See [openapi.md](openapi.md) for the field-by-field contract.

## Sandbox: tracked in git, blog as running example

**Decision:** `playground/` is now tracked in git (its `dist/` excepted) and holds one ongoing end-to-end example, a blog. Supersedes the gitignore part of [Sandbox: `playground/`, user-controlled](#sandbox-playground-user-controlled); the user-controlled access policy is unchanged.

**Why:** as the language grows past `main.json` (routes, models, migrations, templates) the playground becomes the canonical demo of what rublocks can express. Versioning it gives the user and agent a shared reference state, makes regressions diffable, and lets readers see the language's current expressive reach at any commit. `playground/dist/` stays gitignored — it is regenerable and would add noise to every commit.

## Generated `dist/`: preserve `target/` across regenerations

**Decision:** `codegen::emit` wipes everything in `dist/` except the `target/` subdirectory.

**Why:** `cargo` uses `target/` to do incremental compilation. Wiping it on every regeneration would force a full rebuild each time (~30s+) and make dev mode unusable. Preserving it allows ~0.4s incremental rebuilds.

## MongoDB: parked for now

**Decision:** rublocks does not support MongoDB as a backend in v1. The manifest does not accept `kind: "mongo"`, no driver is wired, and process blocks remain SQL-shaped. Revisit when the SQL backends have shipped a stable surface and a real user asks for it.

**Why:** Mongo does not fit the declarative-models → DDL diff pipeline that drives the SQL backends. Migrations would be data rewrites, not structural; `process.db.find_many` semantics would need a Mongo-specific translation (no joins, explicit `$lookup`); the model schema would carry an optional `$jsonSchema` validator but no DDL. Supporting all that is real work that would slow the SQL effort without delivering visible value yet. Closing issue #10 as a wontfix-for-now keeps the door open: the manifest's `services` block is forward-compatible, so a future Mongo backend can land without a schema break.

## Multi-backend SQL: dialect dispatch, not sea-query (yet)

**Decision:** `services.db.kind` selects one of `postgres` / `mysql` / `mariadb` / `mssql`. The migration generator dispatches column types per kind through a small match statement; the rest of the DDL stays template-driven. `sea-query` is **not** adopted yet.

**Why:** the bulk of dialect work is the column-type mapping (UUID, TEXT, bool, TIMESTAMPTZ across the four backends). Once that table is in place, `CREATE TABLE` / `ALTER TABLE` are nearly identical across postgres / mysql / mariadb, and tunneling everything through `sea-query`'s `SchemaBuilder` would add a dependency and an extra layer of indirection without unlocking new value at this stage. The choice keeps the door open: a follow-up can swap the renderer for sea-query without touching the manifest surface. `mssql` is parsed today and the column types are mapped correctly, but `sqlx 0.8` dropped its official MSSQL driver — the manifest accepts the kind so a future driver swap does not require schema changes.

## CI: fmt, clippy, audit, deny all blocking from day one

**Decision:** CI runs `cargo fmt --check`, `cargo clippy -D warnings`, `cargo build`, `cargo test`, `cargo audit` and `cargo deny check` on every push and PR. All gates are blocking.

**Why:** the codebase is still small enough that retrofitting these checks costs nothing; deferring them is the well-known way to accumulate latent debt. `deny.toml` starts with a permissive licence allowlist and `unknown-registry = deny` so any new dep with an unfamiliar licence or source is a visible review event.
