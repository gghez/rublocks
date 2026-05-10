# rublocks documentation

Living reference for the rublocks language and compiler. The codebase changes fast — this directory is the source of truth for what the project currently does and why.

## Index

- [Vision](vision.md) — what rublocks is and who it is for.
- [Architecture](architecture.md) — compiler modules and data flow.
- [CLI reference](cli.md) — every command and flag.
- [Manifest reference](manifest.md) — `main.json` schema and service URL syntax.
- [Routes reference](routes.md) — `routes/*.json` schema and dispatch semantics.
- [Models reference](models.md) — `models/*.json` schema and generated Rust structs.
- [Dev mode](dev-mode.md) — file watching, hot-reload, livereload protocol.
- [OpenAPI generation](openapi.md) — auto-derived OpenAPI 3 spec for `kind: api` routes.
- [Project workflow](workflow.md) — branching, sandbox, push cadence.
- [Decisions](decisions.md) — log of design choices with rationale.

## Status

Pre-alpha. Implemented:

- `rublocks build [path]` — generates a Rust/Axum project under `<path>/dist`.
- `rublocks dev [path]` — same as build, plus a file watcher that rebuilds and restarts the child process on `*.json` / `*.html` changes, and serves a browser livereload snippet.
- `main.json` parsing: `name` + optional `services.{postgres,redis}` with `env:VAR` URL references.
- `routes/*.json` discovery + dispatch (slice 1: handler stubs, no template rendering or process execution yet).
- `models/*.json` → typed Rust structs in `dist/src/main.rs` under `mod models` (slice 2).

Not yet implemented:

- `rublocks new <name>` (scaffolding)
- `rublocks run [path]` (build-then-run without watching)
- Route bodies (templates, input parsing, process blocks, view/output mapping, redirects)
- Migration generation from model declarations
- Background jobs

## Updating these docs

When you add or change a capability, update the matching page in the same commit. New design decisions go to [decisions.md](decisions.md).
