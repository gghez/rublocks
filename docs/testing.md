# Testing

rublocks ships two layers of automated tests.

## Inline unit tests

Every `src/*.rs` file ends with a `#[cfg(test)] mod tests` block. Tests are colocated with the code they cover and run on every `cargo test --locked --all-targets`. Behaviors worth keeping must be locked by a test — see [workflow.md](workflow.md).

## Snapshot tests (`insta`)

Snapshot tests freeze the exact Rust output that codegen produces for a curated set of minimal projects. They complement the assertion-based tests by catching cross-cutting regressions a targeted `assert!(main_rs.contains(...))` would miss — a refactor of one helper that silently rewrites every emitted handler shows up here as a snapshot diff in review.

### Where snapshots live

- Snapshot files are committed under `src/snapshots/`, next to the source file that hosts the test (`insta`'s default layout).
- File naming: `<crate>__<module_path>__<test_name>.snap`. For tests inside `src/codegen.rs`'s `mod tests::snapshot_tests`, that gives `src/snapshots/rublocks__codegen__tests__snapshot_tests__<name>.snap`.
- Snapshots are plain text and **must** be committed: CI runs `cargo test` in non-update mode, so a missing or stale snapshot fails the build.

### What is covered

The current snapshot suite (in `src/codegen.rs`) locks:

- `codegen::emit` for a minimal manifest (no routes, no models).
- `render_cargo_toml` for the minimal manifest and for the postgres + migrations variant.
- Each `blocks::*` kind — one snapshot per built-in (`db.find_many`, `db.find_one`, `db.insert`, `error`, `guard`, `time.now`).
- Layout inheritance, page route, api route, and a model with mixed field types.

Adding a new block kind or codegen module is the moment to add a matching snapshot.

### Accepting changes

After an intentional codegen change, the affected snapshots will differ. The workflow is:

1. Run the tests once. `insta` writes `*.snap.new` files next to the existing `*.snap` and the tests fail.
2. Inspect the diff with `cargo insta review` (install once: `cargo install cargo-insta`). Accept each diff you intend, reject the rest.
3. Commit the updated `*.snap` files alongside the codegen change.

`cargo insta accept --no-pager` bulk-accepts every pending snapshot — handy when the change is mechanical and you have already audited the diff.
