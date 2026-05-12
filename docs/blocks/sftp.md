# SFTP blocks — shared connection contract

The `sftp.*` block family (`list`, `read`, `write`, `delete` — separate issues) all consume one of two connection forms. This page documents the shared contract once; individual block pages link back here.

## Two ways to point a block at a server

Every `sftp.*` block accepts **exactly one** of:

- `service: "<name>"` — name of a `services.<name>` entry in `main.json` with `kind: "sftp"`.
- `connection: {...}` — inline declaration whose leaves accept `$ref` so values can come from a prior block binding.

Setting both, or neither, is a build error pointing at the offending block.

### Static target — `service`

For a single backup server, vendor drop folder, or any SFTP target known at build time. Declare the service in `main.json` (see [`manifest.md#sftp-services`](../manifest.md#sftp-services)) and reference it by name:

```json
{ "block": "sftp.list", "name": "files", "service": "files", "path": "/incoming" }
```

`rublocks` wires one `Arc<SftpService>` field on `AppState` per declared service — every block referencing the same `service` shares that handle.

### Dynamic target — `connection`

For a multi-tenant SaaS storing per-customer SFTP endpoints in a database row, declare the connection inline and bind the values from a prior block:

```json
{
  "name": "tenant",
  "block": "db.find_one",
  "table": "tenants",
  "where": "id == $input.path.tenant_id"
}
```

```json
{
  "block": "sftp.list",
  "connection": {
    "host": "$tenant.sftp_host",
    "port": "$tenant.sftp_port",
    "user": "$tenant.sftp_user",
    "auth": { "password": "$tenant.sftp_password" },
    "host_key_fingerprint": "$tenant.sftp_fingerprint"
  },
  "path": "/incoming"
}
```

Every leaf inside `connection` accepts three forms:

| Form | Resolved at | Example |
|------|-------------|---------|
| Literal | build time | `"host": "sftp.example.com"` |
| `env:VAR` | startup (`std::env::var`) | `"host": "env:SFTP_HOST"` |
| `$path.to.binding` | request time (prior block scope) | `"host": "$tenant.sftp_host"` |

A `$ref` that does not resolve in scope is a build error naming the offender.

## Inline connection schema

The body of `connection` mirrors `services.<name>` minus the `kind` discriminator:

| Field | Required | Notes |
|-------|----------|-------|
| `host` | yes | Server hostname. |
| `port` | no | Defaults to `22` when omitted. |
| `user` | yes | SSH user. |
| `auth` | yes | Exactly one of `password`, `private_key`, `private_key_pem`. |
| `auth.password` / `private_key` / `private_key_pem` | one-of | See [`manifest.md#sftp-services`](../manifest.md#sftp-services). |
| `auth.passphrase` | no | Optional passphrase for the key forms. |
| `host_key_fingerprint` | no | Same dev/release rules as the service form. |

## Host key fingerprint

| Mode | Behaviour when fingerprint missing |
|------|------------------------------------|
| `rublocks dev` (RUBLOCKS_DEV=1) | Warns and trusts on first use. |
| Release build | Refuses to start; the dev overlay surfaces the error with the offending `services.<name>` (or block) name. |

Pin the fingerprint before shipping: dev TOFU is a convenience, not a default for production.

## Operation blocks

The four operation blocks below all consume this contract. They ship in follow-up issues.

- [`sftp.list`](sftp.list.md) — list files in a remote directory.
- [`sftp.read`](sftp.read.md) — download a remote file into memory.
- `sftp.write` — upload bytes to a remote path *(follow-up issue)*.
- `sftp.delete` — remove a remote file or directory *(follow-up issue)*.
