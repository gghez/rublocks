# Blocks

A **block** is the atomic unit of logic inside a route. The project name
itself is the etymology: `rublocks` = *"rust blocks"*. Every route's
behaviour is a *composition of blocks* — small declarative steps with a
standardised input/output contract.

Blocks appear inside the `process` array of routes (and layouts):

```json
"process": [
  { "name": "posts", "block": "db.find_many", "table": "posts" }
]
```

## Contract

Every block:

- Is identified by a `block` discriminator of the form `namespace.action`
  (e.g. `db.find_many`, `time.now`, `error`).
- Declares a **typed input contract**: the JSON fields it accepts. Unknown
  fields are rejected at load time with a pointer to the offending file.
- Declares a **typed output contract**: the Rust type bound to `$<name>`
  (when the block sets a `name`). Other blocks, `view`, and `output` can
  reference that binding via `$<name>` / `$<name>.<field>`.

Blocks come in three flavours:

| Flavour | Binds `$<name>` | Examples |
|---------|-----------------|----------|
| **read-side** | yes | `db.find_many`, `db.find_one`, `time.now` |
| **write-side** | no | `db.insert`, `error` |
| **assertion** | no | `guard` |

Blocks can also be **composable**: a block field can itself hold a
sub-block. The canonical case today is `db.find_one.on_missing`, which
typically points at an `error` block to short-circuit the handler when a
lookup returns no row.

Validation that is *not* a block:

- **Input parsing and validation** — derived automatically from the
  typed `input` spec on the route. See [input.md](../input.md). The block
  layer assumes its `$input.X.X` references are already validated.

Authorization, on the other hand, *is* a block — see [`guard`](guard.md).
There is no `route.guard` field; placing authorization inside `process`
makes its scope explicit (it can only reference what prior blocks have
already bound) and lets a single guard sit anywhere in the pipeline.

## Catalogue

Built-ins shipped today:

- [`db.find_many`](db.find_many.md) — fetch a list of rows.
- [`db.find_one`](db.find_one.md) — fetch a single row, with optional
  `on_missing` sub-block.
- [`db.insert`](db.insert.md) — insert a single row.
- [`error`](error.md) — terminate the handler with an HTTP error.
- [`guard`](guard.md) — authorize the request against a CEL predicate
  (403 on failure).
- [`sftp.list`](sftp.list.md) — list entries under a remote SFTP directory.
- [`sftp.read`](sftp.read.md) — download a remote SFTP file into memory.
- [`time.now`](time.now.md) — bind the current wall-clock time to `$<name>`.

Foundation pages (shared contracts consumed by the catalogue above):

- [`sftp`](sftp.md) — shared `service` / `connection` contract for every
  `sftp.*` block. The service declaration lives in
  [`manifest.md#sftp-services`](../manifest.md#sftp-services).

Each page documents that block's exact JSON shape, output contract, and a
canonical example. The full JSON Schema is also embedded into the
per-project agent files (`AGENTS.md`, `.claude/skills/rublocks/SKILL.md`,
`.cursor/rules/rublocks.mdc`) so any coding agent that opens the project
sees the contract without leaving the repo.

## Logging contract

Every block emits structured log events on every execution path
(success + failure). The runtime wires this for you — see
[`logging.md`](../logging.md) for the full event shape:

- Codegen wraps each block call in a `tracing::info_span!` carrying the
  block's static metadata (`block = "<id>"`, plus the fields declared by
  `log_fields()`). The span makes its fields available to every event
  emitted from within the block.
- On the success path, codegen appends a `tracing::info!(... msg="ok",
  duration_us=...)` event at the end of the block body.
- On every error path inside the block body, the block itself emits a
  `tracing::error!(... msg="block failed", duration_us=..., error=...,
  error.chain=..., backtrace=...)` event before the `return`.

`BlockInstance::log_fields` has **no default impl** — adding a new
block without implementing it fails to compile, so the structured-log
contract cannot silently regress.

## Adding a new block

1. Create `src/blocks/<id>.rs` (use one of the built-ins as a template).
   Define a `Spec` struct (`#[serde(deny_unknown_fields)]`) so unknown
   fields are rejected. Implement `BlockKind` (with `parse`) and
   `BlockInstance` (with `output_type` **and** `log_fields`).
2. If the block has any `return ...` inside its body, emit a
   `tracing::error!` event first — use
   `crate::blocks::runtime::log_block_error` for blocks with a real
   `std::error::Error` value (e.g. `sqlx::Error`), or
   `log_block_error_message` for synthetic messages (`guard` denied,
   `error` block, field validation). See [`logging.md`](../logging.md).
3. If the block always returns (terminal blocks like `error`), override
   `has_success_path()` to `false` so codegen skips the trailing
   success `info!` (otherwise the post-body event is unreachable code).
4. Register the new kind in `BUILTIN_KINDS` in `src/blocks/mod.rs`.
5. Add `docs/blocks/<id>.md` — an integration test enforces the presence
   of this file so the catalogue cannot drift from the registry.

The agent integration files pick up the new block automatically via
`schema::all()`.
