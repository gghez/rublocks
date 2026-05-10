# rublocks documentation

Living reference for the rublocks language and compiler. The codebase changes fast — this directory is the source of truth for what the project currently does and why.

## Index

- [Vision](vision.md) — what rublocks is and who it is for.
- [Architecture](architecture.md) — compiler modules and data flow.
- [CLI reference](cli.md) — every command and flag.
- [Manifest reference](manifest.md) — `main.json` schema and service URL syntax.
- [Dev mode](dev-mode.md) — file watching, hot-reload, livereload protocol.
- [Project workflow](workflow.md) — branching, sandbox, push cadence.
- [Decisions](decisions.md) — log of design choices with rationale.

## Status

Pre-alpha. Implemented:

- `rublocks build [path]` — generates a Rust/Axum project under `<path>/dist`.
- `rublocks dev [path]` — same as build, plus a file watcher that rebuilds and restarts the child process on `*.json` changes, and serves a browser livereload snippet.
- `main.json` parsing: `name` + optional `services.{postgres,redis}` with `env:VAR` URL references.

Not yet implemented:

- `rublocks new <name>` (scaffolding)
- `rublocks run [path]` (build-then-run without watching)
- Route declarations
- Model / migration declarations
- Background jobs

## Updating these docs

When you add or change a capability, update the matching page in the same commit. New design decisions go to [decisions.md](decisions.md).
