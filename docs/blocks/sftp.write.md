# `sftp.write`

Write-side block. Uploads a byte buffer to a remote path on an SFTP
server and binds an ack to `$<name>` as
`crate::_rb_sftp::SftpWriteAck`.

Third of the four operation blocks in the `sftp.*` family (`list`,
`read`, `write`, `delete`). Per the project rule "exactly one way to
spell each thing", a single block covers create-or-overwrite — there is
no separate `sftp.create` / `sftp.update`. Idempotent overwrite is the
default; opt-in error on existing target via `if_exists: "error"`.

The connection contract — `service` vs. inline `connection` — is
documented once in [`sftp.md`](sftp.md); every block in the family
consumes the same shape.

## Schema

```json
{
  "name":          "uploaded",
  "block":         "sftp.write",
  "service":       "files",
  "path":          "/outbox/report-2026-05-11.xlsx",
  "body":          "$report",
  "mode":          "0o640",
  "mkdir_parents": true,
  "if_exists":     "overwrite"
}
```

Conflict-handling form with an `on_conflict` sub-block:

```json
{
  "name":      "uploaded",
  "block":     "sftp.write",
  "service":   "files",
  "path":      "/outbox/report.xlsx",
  "body":      "$report",
  "if_exists": "error",
  "on_conflict": {
    "block":       "error",
    "status":      409,
    "code":        "remote_exists",
    "description": "Refusing to overwrite an existing report."
  }
}
```

| Field | Required | Type | Notes |
|-------|----------|------|-------|
| `block` | yes | `"sftp.write"` | Discriminator. |
| `name` | yes | string | Binds `SftpWriteAck { path: String, size: u64 }` to `$<name>`. |
| `service` / `connection` | one-of | string / object | Connection contract — see [`sftp.md`](sftp.md). Setting both, or neither, is a build error. |
| `path` | yes | string \| `$ref` | Absolute destination path. Literal paths must be absolute; `$ref` / `env:` forms resolve at request time. |
| `body` | yes | `$ref` | Reference to a `Bytes` / `&[u8]` binding (typically from `csv.write`, `xlsx.write`, `pdf.render`, or `$input.body`). |
| `mode` | no | string (octal) | POSIX mode for the created file, e.g. `"0o640"`. Default `"0o644"`. Validated as `0o[0-7]{3,4}` at load time. |
| `mkdir_parents` | no | bool | Default `false`. When `true`, missing parent directories are created with mode `0o755`. |
| `if_exists` | no | enum | `"overwrite"` (default), `"error"`, `"skip"`. See below. |
| `on_conflict` | no | block | Sub-block executed when `if_exists: "error"` and the target exists. Same semantics as `db.find_one.on_missing`. Pairing it with a non-`error` policy is a build error (the branch would be unreachable). |

## Output

`$<name>` resolves to `crate::_rb_sftp::SftpWriteAck`:

| Field | Type | Notes |
|-------|------|-------|
| `path` | `String` | The final remote path. |
| `size` | `u64` | Bytes written. `0` when `if_exists: "skip"` no-ops on an existing target. |

## Conflict policy (`if_exists`)

| Value | Behaviour when target exists |
|-------|------------------------------|
| `"overwrite"` (default) | Atomic overwrite via temp-file + rename. Idempotent. |
| `"error"` | Surfaces `409` (or runs `on_conflict` if set). The default 409 response carries the existing file's size in dev mode. |
| `"skip"` | No-op. Ack has `size: 0` so callers can branch on "wrote nothing". |

## Runtime

The block opens one SFTP session per call (v1 — pooling is deferred).
Writes flow through a sibling `.rb-tmp-*` file, then rename atomically
into place so a partial transfer never leaves a truncated file at the
target. If the server refuses the rename (rare — some chrooted SFTP
servers reject cross-directory renames), the block falls back to a
direct overwrite at the target path and logs the fallback in dev mode.

`mkdir_parents: true` creates every missing component of the parent
chain with mode `0o755` (matches the historical `mkdir -p` default).
`EEXIST` races on the same directory are treated as success so
concurrent writers do not collide.

Every error other than `Conflict` flows through the dev-overlay 500
surface so the user sees the cause in plain text. `Conflict` dispatches
to `on_conflict` (or the default `409 conflict` response when
`on_conflict` is absent).

## Out of scope (v1)

- Resumable / chunked uploads — the full body is sent in one stream.
- Setting ownership (`chown`) — most SFTP servers refuse it anyway.
- Server-side checksum verification. v1 trusts the protocol's transport
  integrity.
