# `sftp.read`

Read-side block. Downloads a single remote file's contents into memory
and binds the result to `$<name>` as `bytes::Bytes`.

This is the second of four operation blocks in the `sftp.*` family
(`list`, `read`, `write`, `delete`). The connection contract — `service`
vs. inline `connection` — is documented once in [`sftp.md`](sftp.md);
every block in the family consumes the same shape.

## Schema

```json
{
  "name":      "raw",
  "block":     "sftp.read",
  "service":   "files",
  "path":      "/incoming/orders-2026-05-11.csv",
  "max_bytes": 10485760,
  "on_missing": {
    "block":       "error",
    "status":      404,
    "code":        "remote_file_not_found",
    "description": "The expected remote file is not present."
  }
}
```

Inline `connection` form, with leaves bound from a prior block:

```json
{
  "name":  "raw",
  "block": "sftp.read",
  "connection": {
    "host": "$tenant.sftp_host",
    "port": "$tenant.sftp_port",
    "user": "$tenant.sftp_user",
    "auth": { "password": "$tenant.sftp_password" }
  },
  "path": "/incoming/orders.csv"
}
```

| Field | Required | Type | Notes |
|-------|----------|------|-------|
| `block` | yes | `"sftp.read"` | Discriminator. |
| `name` | yes | string | Binds `bytes::Bytes` to `$<name>`. |
| `service` / `connection` | one-of | string / object | Connection contract — see [`sftp.md`](sftp.md). Setting both, or neither, is a build error. |
| `path` | yes | string \| `$ref` | Absolute remote file path. Literal paths must be absolute; `$ref` / `env:` forms resolve at request time. |
| `max_bytes` | no | int | Hard cap on the download. Default `10485760` (10 MiB). Exceeded → `413 Payload Too Large` with the actual remote size in the response body. |
| `on_missing` | no | block | Sub-block executed when `path` does not exist on the remote (parsed recursively, same semantics as `db.find_one.on_missing`). Without `on_missing`, a remote `ENOENT` flows through the default `404 not_found` response. |

## Output

`$<name>` resolves to `bytes::Bytes`. Downstream blocks
(`csv.read`, `xlsx.read`, the `pdf.render` consumers, … to come)
accept it as their `source` input — no re-serialization required.

## Runtime

The block opens one SFTP session per call (v1 — pooling is deferred).
The cap is enforced twice for safety:

1. A pre-flight `stat()` short-circuits oversized files before any byte
   of payload is transferred.
2. A streaming counter on the read loop catches the case where the
   server lies about the size or grows the file between `stat` and
   `read`.

Either path returns `413 Payload Too Large` with the actual remote
size in the response body — the dev-mode reader can raise `max_bytes`
without leaving the browser. `ENOENT` on `path` dispatches to
`on_missing` (or the default `404 not_found` when omitted); every other
error (auth failure, protocol error, …) flows through the dev-overlay
`500` surface so the user sees the cause in plain text.

## Out of scope (v1)

- Resumable / ranged downloads. The whole file is fetched in one pass.
- Direct streaming to the HTTP response without buffering. v1 buffers
  the entire body into `Bytes` first.
- On-the-fly decompression of `.gz` / `.zip`. Callers chain a dedicated
  block when those land.
