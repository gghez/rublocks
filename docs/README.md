# rublocks documentation

Living reference for the rublocks language and compiler. The codebase changes fast — this directory is the source of truth for what the project currently does and why.

## Index

- [Vision](vision.md) — what rublocks is and who it is for.
- [Architecture](architecture.md) — compiler modules and data flow.
- [CLI reference](cli.md) — every command and flag.
- [Manifest reference](manifest.md) — `main.json` schema and service URL syntax.
- [Encoding policy](encoding.md) — UTF-8 everywhere: input strictness, output labelling, file I/O, DB session.
- [Structured logging](logging.md) — mandatory NDJSON-on-stdout via `tracing`; per-block / per-request fields.
- [Routes reference](routes.md) — `routes/*.json` schema and dispatch semantics.
- [Blocks reference](blocks/README.md) — the unit of logic inside `route.process`. One page per built-in under `docs/blocks/`.
- [Input reference](input.md) — typed `route.input` spec and the auto-generated validator it produces.
- [Models reference](models.md) — `models/*.json` schema and generated Rust structs.
- [Layouts reference](layouts.md) — `layouts/*.json` schema and inheritance wiring.
- [Migrations reference](migrations.md) — forward-only SQL generation from `models/*.json` diffs.
- [Expressions reference](expressions.md) — CEL syntax for guards, filters, validators.
- [Templates reference](templates.md) — Askama rendering for `kind: page` routes.
- [Deploy](deploy.md) — Dockerfile + docker-compose emitted under `dist/`.
- [Dev mode](dev-mode.md) — file watching, hot-reload, livereload protocol.
- [Agent integration](agents.md) — per-project files written by `build` for Claude, Codex (`AGENTS.md`), and Cursor.
- [OpenAPI generation](openapi.md) — auto-derived OpenAPI 3 spec for `kind: api` routes.
- [Project workflow](workflow.md) — branching, sandbox, push cadence.
- [Testing](testing.md) — inline unit tests and the `insta` snapshot layer for codegen.
- [Decisions](decisions.md) — log of design choices with rationale.

## Status

Pre-alpha. Implemented:

- `rublocks build [path]` — generates a Rust/Axum project under `<path>/dist`.
- `rublocks dev [path]` — same as build, plus a file watcher that rebuilds and restarts the child process on `*.json` / `*.html` changes, and serves a browser livereload snippet.
- `main.json` parsing: `name` + mandatory SemVer `version` + optional `services.{postgres,redis}` with `env:VAR` URL references.
- `routes/*.json` discovery + dispatch with full request-time handlers: typed `input` parsing/validation, process blocks (`db.find_many`, `db.find_one`, `db.insert`, `guard`, `time.now`, `error`), `view` / `output` mapping, and `redirect`.
- `models/*.json` → typed Rust structs in `dist/src/main.rs` under `mod models`, plus table-level `indexes`/`foreign_keys`/`checks` with field-level shorthand resolution.
- `layouts/*.json` parsing + `templates/*.html` Askama rendering for `kind: page` GET routes, with literal `view` baking and dev-mode livereload injection.
- Forward-only migration generation on every build (Postgres DDL today; multi-backend via sea-query is issue #9). See [migrations.md](migrations.md).
- Per-agent integration files written on every `build`: Claude skill, `AGENTS.md` (Codex), Cursor rule. See [agents.md](agents.md).

Not yet implemented:

- `rublocks new <name>` (scaffolding)
- `rublocks run [path]` (build-then-run without watching)
- Background jobs

## Updating these docs

When you add or change a capability, update the matching page in the same commit. New design decisions go to [decisions.md](decisions.md).

## Doc examples are parsed by the build

Canonical JSON blocks in these pages are validated by `cargo test` against the parser the binary actually accepts. The convention is one HTML comment immediately above the fence:

````markdown
<!-- rb:manifest -->
```json
{ "name": "myapp", "version": "0.1.0", "description": "A blog with public posts and admin moderation.", "language": "en-US", "encoding": "utf-8", "logging": { "level": "info" } }
```
````

Recognised kinds: `manifest`, `model`, `route`, `layout`. The test source is `src/docs_tests.rs`; the per-kind validators live next to each parser (`manifest::validate_doc_example`, etc.). Annotate the canonical example for a capability — leave illustrative fragments unannotated and they are silently skipped.

