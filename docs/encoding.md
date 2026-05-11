# Encoding policy

rublocks adopts **UTF-8 everywhere, strict on input, explicit on output**
as the single character-encoding contract for every project. The policy is
*declared, not implicit* — `main.json` carries an explicit `encoding`
field, and every codegen and runtime site reads from it.

## The `encoding` field

`main.json` must declare a top-level `encoding`. The value is required;
omitting it is a build-time error visible in the dev-mode browser overlay.
Only `"utf-8"` is accepted today (case-insensitive). Any other value is
rejected at load time with a message naming the offending value.

```json
{
  "name": "myapp",
  "encoding": "utf-8"
}
```

The field exists so the contract is explicit in every project and so a
future second value (e.g. another canonical normalization form) can land
without a silent default flip.

## Compile-time enforcement (inputs)

Every project file the compiler reads goes through a single decoder
(`manifest::read_text_utf8`). The decoder:

- Rejects UTF-16 LE/BE and UTF-32 LE/BE byte order marks at build time
  with a message naming the file. Re-save the file as UTF-8 and rebuild.
  UTF-32 is detected before UTF-16 because the four-byte signatures share
  their first two bytes (`FF FE` is the prefix of both UTF-16 LE and
  UTF-32 LE).
- Strips a leading UTF-8 BOM (`EF BB BF`) — some editors on Windows write
  one by default. The file is otherwise unchanged.
- Reports byte offsets for non-UTF-8 sequences so the editor can jump
  straight to the corruption.

Files covered: `main.json`, `routes/*.json`, `models/*.json`,
`layouts/*.json`, `migrations/.state.json`.

## Compile-time enforcement (outputs)

Every file rublocks writes — `dist/Cargo.toml`, `dist/src/main.rs`,
`Dockerfile`, `docker-compose.yml`, `.dockerignore`, migration `.sql`
files, `migrations/.state.json`, and the three agent integration files
(`SKILL.md`, `AGENTS.md`, cursor rule) — goes through
`manifest::write_text_utf8`:

- UTF-8 bytes, no BOM. Generated files never advertise their encoding
  through a BOM; the explicit HTTP `Content-Type` header is the canonical
  channel.
- LF line endings regardless of host OS. CRLF in the input is folded to
  LF first; lone CR (legacy Mac) is folded next. Writing a Windows-style
  snippet doesn't smuggle `\r\n` into the dist project.

## Runtime enforcement (HTTP)

The generated app carries a per-project `_rb_encoding` module wired as
the outermost router layer. It runs before any handler on the request
side and after every handler on the response side.

### Inbound: strict

- JSON, form-urlencoded, and `text/*` requests whose `Content-Type`
  carries a non-UTF-8 `charset=` parameter are rejected with
  `415 Unsupported Media Type`. The response body names the offending
  value.
- A missing `charset=` parameter is taken to mean "the project default
  applies" and accepted.
- Binary types (`image/*`, `application/octet-stream`, …) are passed
  through untouched — `charset` is not meaningful on those bodies.

### Outbound: explicit

- `application/json` and `text/*` responses that don't already carry a
  `charset=` parameter are re-labelled with `; charset=utf-8` before
  they leave the router. HTTP clients never have to infer.
- Responses that already declare a charset are left alone — the handler
  was explicit on purpose.

## Database

- **Postgres.** The generated `main.rs` augments the resolved connection
  URL with `client_encoding=UTF8` so the SQL session always matches the
  project-wide contract, regardless of the cluster's locale setting.
- **MySQL / MariaDB.** Encoding is negotiated through a different URL
  parameter (`charset=utf8mb4`). Left to user opt-in today.
- **MSSQL.** No equivalent client-side knob.
- **SQLite.** UTF-8 by default; out of scope.

## Why declared and not implicit

Rust's `String` is already UTF-8, Axum's default content types are
already UTF-8, and most JSON consumers assume UTF-8. The implicit
behavior works — until it doesn't:

- A `POST /api/posts` with a `Content-Type: application/json; charset=iso-8859-1`
  body fails late with a cryptic `serde_json` parse error instead of a
  clean 415.
- A manifest file accidentally saved as UTF-16 by a Windows editor fails
  with `invalid character` instead of "this file is UTF-16, re-save as
  UTF-8".
- A generated `Cargo.toml` written with CRLF on Windows confuses Linux
  tooling that doesn't expect CRLF in its TOML.

A declared encoding turns each of these into a single, browser-visible
error that names the file and the fix. The trade-off — one extra line in
every `main.json` — is cheap compared to the diagnostic gain.

## See also

- [Decisions log](decisions.md) for the design rationale.
- [Manifest reference](manifest.md) for the full `main.json` schema.
- [Dev mode](dev-mode.md) for how encoding errors surface in the browser.
