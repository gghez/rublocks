# CLI reference

```
rublocks <COMMAND> [ARGS]
```

All commands accepting `[path]` default to the current working directory and canonicalize the argument before use.

## `rublocks build [path]`

Compiles the rublocks project at `[path]` to a Rust/Axum project under `<path>/dist`.

Steps:
1. Read and validate `<path>/main.json`.
2. Wipe `<path>/dist/` (preserving `target/` for incremental builds).
3. Emit `<path>/dist/Cargo.toml` and `<path>/dist/src/main.rs`.

Does **not** invoke `cargo build`. Run `cargo build` yourself in `dist/` to produce a binary.

## `rublocks dev [path]`

Builds the project, runs it, and watches for changes.

Steps:
1. `build` (codegen).
2. `cargo build` in `dist/`.
3. Spawn the generated binary as a child process with `RUBLOCKS_DEV=1`.
4. Watch `*.json` files under `<path>` (recursive, excluding `<path>/dist/`).
5. On detected change (debounced 300ms, deduplicated by content hash):
   - Kill the child process.
   - Re-run codegen.
   - `cargo build` again (incremental).
   - Respawn the child.

The generated app, when started with `RUBLOCKS_DEV=1`, mounts dev-only routes — see [dev-mode.md](dev-mode.md).

Stop with `Ctrl+C`; the supervisor kills the child cleanly before exiting.

## `rublocks new <name>`

Not implemented yet. Will scaffold a new rublocks project directory with a starter `main.json`.

## `rublocks run [path]`

Not implemented yet. Will build then run the generated project without watching for changes.
