# Dev mode

`rublocks dev [path]` runs the project with file watching, automatic rebuild + restart, and browser livereload.

## File watching

- Backed by [`notify-debouncer-full`](https://crates.io/crates/notify-debouncer-full) with a 300ms debounce window.
- Watches `[path]` recursively but ignores anything under `[path]/dist/`.
- Only `*.json` files outside `dist/` trigger a rebuild.

## Content-hash dedup

Inotify events on WSL2 (and some other filesystems) fire repeatedly for a single file write — sometimes spread across multiple debounce windows. Without dedup, the supervisor falls into an infinite rebuild loop after a single edit.

Mitigation: after every file event, the supervisor recomputes a hash of every relevant `*.json` file in the project (excluding `dist/`) and only rebuilds if the hash changed since the last build.

This is intentionally content-based, not mtime-based, so a re-save with identical content does nothing.

## Rebuild cycle

1. File event arrives via the debounced channel.
2. `relevant_change` checks: at least one path is `*.json` outside `dist/`.
3. Hash of project JSON files recomputed; compared with last hash.
4. If different: kill child, re-run codegen, `cargo build`, respawn child with `RUBLOCKS_DEV=1`.

A no-op rebuild typically takes ~0.4s on a warm cargo cache.

## Browser livereload

When `RUBLOCKS_DEV=1` is set, the generated app mounts three additional routes:

| Route | Purpose |
|-------|---------|
| `GET /` | Minimal HTML demo page that loads the livereload snippet. Serves only as a placeholder until user-defined routes exist. |
| `GET /__rublocks/livereload.js` | A small EventSource client. |
| `GET /__rublocks/events` | SSE stream kept alive by axum's `KeepAlive`. |

### Reload protocol

The client snippet maintains a single `EventSource` connection. On the first connection, it records "ever connected = true" but does nothing. When the connection drops (because the supervisor restarted the server), the snippet retries with a 500ms backoff. Once the connection succeeds again, the snippet calls `location.reload()`.

Net effect: editing `main.json` → ~1-2s later the browser tab refreshes with the new build.

The SSE stream itself never sends payload events — the connect/disconnect cycle alone is the signal.

## Ctrl+C

A `ctrlc` handler kills the child process and exits cleanly. The dist binary is killed via `Child::kill` then waited on to avoid zombies.
