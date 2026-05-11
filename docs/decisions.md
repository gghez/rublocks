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

## CEL as the declarative expression sub-language

**Decision:** rublocks adopts [Common Expression Language][cel] (via the [`cel`][cel-rust] crate, the maintained successor to `cel-interpreter`) as the expression sub-language for the `guard` block's `if`, `field.validate`, `process[*].where`, and view conditionals. Build-time validates every CEL snippet syntactically; runtime evaluation lands with process-block execution.

**Why:** CEL is non-Turing-complete by design (no loops, no recursion, no I/O), already production-grade through Kubernetes admission controllers and Envoy, and trivially sandboxable. Alternatives considered: `rhai` (full scripting language — more power than we need, larger attack surface in JSON config), `evalexpr` (arithmetic-only, no rich object navigation), a hand-rolled mini-DSL (would reinvent CEL badly). The `cel` crate's parser can panic on certain malformed inputs; the validator wraps compilation in `catch_unwind` so a build error is structured rather than a crash.

The crate was renamed (and the legacy `cel-interpreter 0.10` migrated off the unmaintained `paste` proc-macro to `pastey`), so `cel = "0.13"` is what both the rublocks compiler and every emitted dist crate depend on.

[cel]: https://github.com/google/cel-spec
[cel-rust]: https://crates.io/crates/cel

## Authorization: a block, not a route-level field

**Decision:** authorization is the `guard` block placed inside `process`. There is no `route.guard` field; the only way to declare a guard is the block.

**Why:** a route-level field only sees the input and a handful of globals, so it can express `user.is_admin` but not `post.author_id == user.id` (the row is not loaded yet). Making `guard` a block lets it sit anywhere in the pipeline — its scope is exactly what prior blocks have bound, so a post-load ownership check composes naturally. Build-time scope analysis becomes a single linear pass over `process`. Per CLAUDE.md ("one feature = one declarative form"), we did not keep the field as syntactic sugar: two ways to spell the same authorization site would have forced the type-checker, the runtime, and the docs to handle both forever for no expressive gain.

## MongoDB: parked for now

**Decision:** rublocks does not support MongoDB as a backend in v1. The manifest does not accept `kind: "mongo"`, no driver is wired, and process blocks remain SQL-shaped. Revisit when the SQL backends have shipped a stable surface and a real user asks for it.

**Why:** Mongo does not fit the declarative-models → DDL diff pipeline that drives the SQL backends. Migrations would be data rewrites, not structural; `process.db.find_many` semantics would need a Mongo-specific translation (no joins, explicit `$lookup`); the model schema would carry an optional `$jsonSchema` validator but no DDL. Supporting all that is real work that would slow the SQL effort without delivering visible value yet. Closing issue #10 as a wontfix-for-now keeps the door open: the manifest's `services` block is forward-compatible, so a future Mongo backend can land without a schema break.

## Multi-backend SQL: dialect dispatch, not sea-query (yet)

**Decision:** `services.db.kind` selects one of `postgres` / `mysql` / `mariadb` / `mssql`. The migration generator dispatches column types per kind through a small match statement; the rest of the DDL stays template-driven. `sea-query` is **not** adopted yet.

**Why:** the bulk of dialect work is the column-type mapping (UUID, TEXT, bool, TIMESTAMPTZ across the four backends). Once that table is in place, `CREATE TABLE` / `ALTER TABLE` are nearly identical across postgres / mysql / mariadb, and tunneling everything through `sea-query`'s `SchemaBuilder` would add a dependency and an extra layer of indirection without unlocking new value at this stage. The choice keeps the door open: a follow-up can swap the renderer for sea-query without touching the manifest surface. `mssql` is parsed today and the column types are mapped correctly, but `sqlx 0.8` dropped its official MSSQL driver — the manifest accepts the kind so a future driver swap does not require schema changes.

## Project locale: mandatory `language` field, no implicit default

**Decision:** every `main.json` declares a top-level required `language` field carrying a BCP 47 tag (e.g. `"en-US"`, `"fr-FR"`). There is no implicit default and no per-route override yet; the project value flows into `<html lang="...">`, the `Content-Language` HTTP header, and the dev-mode error overlay's localized strings. Built-in tables ship for English and French; other tags resolve to English with a build-time warning.

**Why:** like `name`, the project locale is too consequential to be implicit. Accessibility (`<html lang>` is required for screen readers and SEO), correct response headers, and any future i18n surface all need a single source of truth. A per-route override would force every consumer (template renderer, header layer, future translations) to handle two precedence tiers from day one, for a use case nobody has asked for yet — easier to add later when a real driver appears than to remove. Built-in copy is limited to `en` and `fr` because shipping more without a user driving them turns into stale strings; the build-time warning makes the fallback a visible event rather than a silent quality loss. See issue #14.

## Logging: `tracing` + mandatory NDJSON on stdout

**Decision:** the only library used is `tracing` / `tracing-subscriber`; the only output format is one compact JSON object per line on `stdout`. `main.json.logging` is mandatory — a rublocks project without declared logging fails the load step with an error pointing at `main.json`. `logging.level` is mandatory (one of `trace`/`debug`/`info`/`warn`/`error`); `logging.include` is optional and accepts the `env:VAR` form already used for service URLs. The trait method `BlockInstance::log_fields` has no default impl, so adding a new block without declaring its log metadata is a Rust compile error. Codegen wraps every block execution in a `tracing::info_span!` plus an `Instant::now()` and emits success / error events around the body. Errors carry the `source()` chain (`error.chain`) and an opt-in backtrace (`RUST_BACKTRACE=1`). Sinks other than `stdout`, pretty format, sampling, redaction, metrics, and distributed tracing are explicit follow-ups. See [logging.md](logging.md) and issue #17.

**Why:** rublocks is designed to be instructed by an agent from the browser (`docs/dev-mode.md`), so business behaviour invisible to the developer wastes the loop. `tracing` is the de-facto Rust facade with native Axum / tower-http support; NDJSON is the lowest-friction structured shape (`jq`, `grep`, any aggregator). Mandating the manifest field — rather than defaulting silently — means every project is self-describing on observability, and a future second sink can land without a silent default flip. The hard-failure compile gate on `log_fields` mirrors the policy for `output_type`: new contributors cannot silently break the contract.

## Encoding: UTF-8 everywhere, declared in `main.json`

**Decision:** rublocks adopts **UTF-8 everywhere, strict on input,
explicit on output** as the project-wide character-encoding contract.
`main.json` carries a mandatory top-level `encoding` field — only
`"utf-8"` is accepted today, any other value is rejected at build time.
The codegen, the file I/O, the HTTP middleware, and the Postgres session
read from that single declaration. See [encoding.md](encoding.md) for the
full policy.

**Why:** Rust strings are already UTF-8, Axum's defaults are already
UTF-8, and most JSON consumers assume UTF-8 — but the implicit behaviour
leaks at every seam (a UTF-16 manifest fails with a cryptic JSON parse
error; a `charset=iso-8859-1` request body fails late inside
`serde_json`; a Windows-edited `Cargo.toml` smuggles CRLF into the dist
project). Declaring the contract turns each of those into a single,
browser-visible error that names the file and the fix. The field exists
even though there is only one valid value today so a future encoding can
land without a silent default flip — and so every project's `main.json`
is self-describing on this dimension.

## CI: fmt, clippy, audit, deny all blocking from day one

**Decision:** CI runs `cargo fmt --check`, `cargo clippy -D warnings`, `cargo build`, `cargo test`, `cargo audit` and `cargo deny check` on every push and PR. All gates are blocking.

**Why:** the codebase is still small enough that retrofitting these checks costs nothing; deferring them is the well-known way to accumulate latent debt. `deny.toml` starts with a permissive licence allowlist and `unknown-registry = deny` so any new dep with an unfamiliar licence or source is a visible review event.

## Distribution: GitHub Releases + standalone installer scripts

**Decision:** every `v*` tag triggers `.github/workflows/release.yml`, which cross-builds the `rublocks` binary for `x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`, `x86_64-apple-darwin`, `aarch64-apple-darwin`, and `x86_64-pc-windows-msvc`, packages each as `rublocks-v<version>-<target>.{tar.gz,zip}` with a sibling `.sha256`, and attaches them plus a `SHA256SUMS` index and two version-pinned standalone installers (`install.sh`, `install.ps1`) to the release. The installers (`scripts/install.{sh,ps1}.in`, substituted at release time) detect host OS/arch, fetch and verify the matching archive, and drop the binary into `${RUBLOCKS_HOME:-$HOME/.rublocks}/bin`. The workflow rejects a tag whose `Cargo.toml` version does not match.

**Why:** the target audience is coding agents and their human operators, not just Rust developers — requiring a `cargo install` round-trip would gate adoption on a working toolchain. A one-liner `curl … | sh` (and the PowerShell equivalent) is the lowest-friction path that still ships verified, version-pinned binaries; baking the version into the installer at release time means `releases/latest/download/install.sh` always resolves to a script that downloads the matching archive, and `releases/download/v<x.y.z>/install.sh` gives pinned reproducibility. `cargo-dist`/`dist` would automate the same shape but adds a generated workflow we cannot easily audit; hand-rolling stays small (one workflow + two templates) and keeps the security surface inspectable.

## No escape hatch: capability gaps land as new blocks

**Decision:** rublocks ships no escape hatch. There is no raw-Rust handler block, no raw-SQL field on `db.*` blocks, no inline `unsafe-rust` / `raw` block kind. When the declarative surface cannot express a capability, the answer is a **new block kind** under `docs/blocks/<id>.md` plus the matching `src/blocks/<id>.rs`, not a backdoor that bypasses the JSON layer.

**Why:** an escape hatch would corrode every property the language was built to give.

- It violates **one feature = one declarative form** (see CLAUDE.md). A raw-Rust door is a second spelling for what blocks already spell — agents would gravitate to it because it is the shortest path to "make it work right now," and the typed JSON surface decays into glue around `unsafe-rust` inserts.
- It breaks **idempotence**. Two valid spellings of the same intent mean two possible diffs from the same agent prompt; review loses its signal.
- It breaks **the escape-hatch-is-`dist/` promise**. Today the generated Rust is the read-out: if rublocks gets in the way, you keep `dist/` and walk. A raw-Rust block inside the source files inverts that — JSON becomes a thin wrapper around hand-written Rust and the user owes both languages forever.
- The real mitigation for capability gaps is **shipping new block kinds fast**. The block authoring loop is one `Spec` struct, one `BlockKind` impl, one doc page (the registry test enforces the doc); review surface stays narrow.

Cross-link: [vision.md](vision.md#what-rublocks-is-not).
