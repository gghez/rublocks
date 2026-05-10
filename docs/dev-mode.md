# Dev mode

`rublocks dev [path]` runs the project with file watching, automatic rebuild + restart, and browser livereload.

## File watching

- Backed by [`notify-debouncer-full`](https://crates.io/crates/notify-debouncer-full) with a 300ms debounce window.
- Watches `[path]` recursively but ignores anything under `[path]/dist/`.
- Watched extensions: `*.json` (manifest, models, routes, layouts) and `*.html` (templates). Both feed codegen, so either changing must rebuild. Other files are ignored.
- A 1s fallback sweep recomputes the project hash even when no inotify event arrived. This catches the WSL2/inotify race where a file written into a freshly-created subdirectory delivers no event. The content-hash dedup keeps the sweep from rebuilding when nothing changed.

## Content-hash dedup

Inotify events on WSL2 (and some other filesystems) fire repeatedly for a single file write — sometimes spread across multiple debounce windows. Without dedup, the supervisor falls into an infinite rebuild loop after a single edit.

Mitigation: after every file event, the supervisor recomputes a hash of every watched source file in the project (excluding `dist/`) and only rebuilds if the hash changed since the last build.

This is intentionally content-based, not mtime-based, so a re-save with identical content does nothing.

## Rebuild cycle

1. File event arrives via the debounced channel, **or** the 1s fallback sweep ticks.
2. For an event, `relevant_change` checks: at least one path is a watched source file outside `dist/`. The fallback sweep skips this filter.
3. Hash of project source files recomputed; compared with last hash.
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

## Service fallback

When `main.json` declares a `postgres` or `redis` service with `url: "env:VAR"` and `VAR` is not exported in the caller's environment, `rublocks dev` spins up a Docker container for that service instead of crashing. The provisioned URL is injected into the dist child process as `VAR`.

Services whose URL is literal, or whose env var is already set, are left untouched.

### Container & volume layout

| Resource | Name | Notes |
|---|---|---|
| Postgres container | `rublocks-dev-<app>-postgres` | Image `postgres:16-alpine`, user/password `rublocks`/`rublocks`, db `<app>` (with `-` rewritten to `_`). |
| Postgres volume | `rublocks-dev-<app>-postgres-data` | Mounted at `/var/lib/postgresql/data`. Persists across dev sessions. |
| Redis container | `rublocks-dev-<app>-redis` | Image `redis:7-alpine`, AOF on. |
| Redis volume | `rublocks-dev-<app>-redis-data` | Mounted at `/data`. Persists across dev sessions. |

Containers carry the `rublocks-dev=1` label, the host port is allocated by Docker (no fixed port → no collision with anything else running locally), and the resulting URL is logged at startup.

### Reuse

On every `rublocks dev` run, each service goes through one of three paths:

- **Missing** → `docker run` with the volume mount.
- **Stopped** → `docker start` on the existing container; data is preserved.
- **Running** → leave it alone, just read its current host port.

The host port can change on each `docker start` since Docker reallocates — `rublocks dev` resolves the live port on every run, so the generated URL is always correct.

### Requirements

Docker must be installed and the daemon reachable. If not, `rublocks dev` aborts with a message asking the user to either start Docker or export the missing env vars manually.

## Ctrl+C

A `ctrlc` handler kills the child process, runs `docker stop` on every container the session touched (both newly started and ones already running), then exits cleanly. Volumes and container definitions are kept so the next `dev` invocation reuses the same data.
