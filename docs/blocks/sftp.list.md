# `sftp.list`

Read-side block. Lists the entries under a remote directory on an SFTP
server and binds the result to `$<name>` as
`Vec<crate::_rb_sftp::SftpEntry>`.

This is the first of four operation blocks in the `sftp.*` family
(`list`, `read`, `write`, `delete`). The connection contract — `service`
vs. inline `connection` — is documented once in [`sftp.md`](sftp.md);
every block in the family consumes the same shape.

## Schema

```json
{
  "name":      "files",
  "block":     "sftp.list",
  "service":   "files",
  "path":      "/incoming",
  "recursive": false,
  "pattern":   "*.csv",
  "on_missing": {
    "block":       "error",
    "status":      404,
    "code":        "remote_dir_not_found",
    "description": "Remote directory /incoming does not exist."
  }
}
```

Inline `connection` form, with leaves bound from a prior block:

```json
{
  "name":  "files",
  "block": "sftp.list",
  "connection": {
    "host": "$tenant.sftp_host",
    "port": "$tenant.sftp_port",
    "user": "$tenant.sftp_user",
    "auth": { "password": "$tenant.sftp_password" }
  },
  "path": "/incoming"
}
```

| Field | Required | Type | Notes |
|-------|----------|------|-------|
| `block` | yes | `"sftp.list"` | Discriminator. |
| `name` | yes | string | Binds `Vec<SftpEntry>` to `$<name>`. |
| `service` / `connection` | one-of | string / object | Connection contract — see [`sftp.md`](sftp.md). Setting both, or neither, is a build error. |
| `path` | yes | string \| `$ref` | Remote directory path. Literal paths must be absolute; `$ref` / `env:` forms are resolved at request time. |
| `recursive` | no | bool | Default `false`. When `true`, walks subdirectories breadth-first. |
| `pattern` | no | string | Glob pattern (`*.csv`, `**/2026/*.json`). Validated at load time; bad globs surface in the dev overlay. |
| `on_missing` | no | block | Sub-block executed when `path` does not exist on the remote (parsed recursively, same semantics as `db.find_one.on_missing`). Without `on_missing`, a remote `ENOENT` flows through the dev-mode 500 surface. |

## Output

`$<name>` resolves to `Vec<crate::_rb_sftp::SftpEntry>`. Each entry
carries:

| Field | Type | Notes |
|-------|------|-------|
| `name` | `String` | Path relative to the listed directory — basename for non-recursive, relative subpath for recursive walks. |
| `kind` | enum (`"file"` / `"dir"` / `"link"` / `"other"`) | Server-reported file type. Symlinks are returned as `"link"` with the target unresolved. |
| `size` | `u64` | Bytes. `0` for `"dir"` / `"link"`. |
| `modified_at` | `chrono::DateTime<Utc>` | Server-reported mtime (`0` epoch when the remote omitted it). |
| `mode` | `u32` | POSIX permission bits (e.g. `0o644`). |

Pattern matching is applied to each entry's path *relative* to the
listed directory, so `pattern: "*.csv"` matches `report.csv` and
`pattern: "**/2026/*.json"` matches `archive/2026/jan.json` under a
recursive walk.

## Runtime

The block opens one SFTP session per call (v1 — pooling is deferred).
`recursive: true` walks subdirectories breadth-first, returning each
entry's path *relative to the listed root*. The `pattern` glob filters
entries by their relative path; non-matching entries are skipped but a
directory is still descended into when `recursive: true`.

ENOENT on the originally listed `path` dispatches to `on_missing` (or
the default `404 not_found` response when `on_missing` is absent); every
other error (auth failure, protocol error, ENOENT on a deeper directory
during a recursive walk) flows through the dev-overlay 500 surface so
the user sees the cause in plain text.

## Out of scope (v1)

- Pagination / streaming for huge directories — the block returns the
  full vector. Future operation blocks may stream when a real workload
  asks for it.
- Symlink resolution — links are returned as `kind: "link"` with the
  target unresolved.
- Sorting — callers do it via a follow-up CEL filter / sort step.
