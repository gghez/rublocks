# Structured logging

Every rublocks project emits **NDJSON on stdout** — one compact JSON
object per event, no indentation, no internal newline — via
[`tracing`](https://docs.rs/tracing) and
[`tracing-subscriber`](https://docs.rs/tracing-subscriber). The pipeline
is *declared, not implicit*: `main.json` carries a mandatory `logging`
block and every codegen and runtime site reads from it.

## The `logging` field

`main.json` must declare a top-level `logging`. The value is required;
omitting it is a build-time error visible in the dev-mode browser
overlay.

<!-- rb:manifest -->
```json
{
  "name": "myblog",
  "version": "0.1.0",
  "description": "A blog with public posts and admin moderation.",
  "language": "en-US",
  "encoding": "utf-8",
  "logging": {
    "level": "info",
    "include": {
      "service": "myblog",
      "env": "env:RUST_ENV"
    }
  }
}
```

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `logging` | object | **yes** | Top-level block. Missing → load-time error pointing at `main.json`. |
| `logging.level` | string | **yes** | One of `trace` / `debug` / `info` / `warn` / `error`. No default — the project author commits to one explicitly. |
| `logging.include` | object | no | Key/value pairs injected on every event. Values are either literal strings or the `env:VAR_NAME` form (resolved at startup via `std::env::var`, missing var → empty string). |

Unknown fields under `logging` are rejected (`deny_unknown_fields`), as
they are elsewhere in the manifest.

## What you get

A request through a generated project emits a line per block, plus the
per-request tracing span set up by `tower-http::trace::TraceLayer`:

```
{"timestamp":"2026-05-11T18:34:21.842Z","level":"INFO","fields":{"message":"ok","duration_us":4123},"target":"rublocks::block","span":{"block":"db.find_many","table":"posts","name":"block"},"spans":[{"request_id":"01HXY...","method":"GET","path":"/api/posts","route":"/api/posts","name":"http_request"},{"block":"db.find_many","table":"posts","name":"block"}]}
```

One line = one event. Lines are valid JSON (`jq -c` round-trips them)
and any standard log aggregator picks them up.

### Common fields on every event

| Field | Source | Notes |
|-------|--------|-------|
| `timestamp` | `tracing-subscriber` | RFC 3339, millisecond resolution. |
| `level` | `tracing-subscriber` | `TRACE` / `DEBUG` / `INFO` / `WARN` / `ERROR`. |
| `target` | event metadata | `rublocks::block` for block events, `rublocks::request` for the request span, `rublocks::app` for the root span. |
| `block` | block span | The block's kind id (`db.find_many`, `guard`, …). Carried on every block event. |
| `request_id` | request span | Reads `x-request-id` if the inbound request carried one; otherwise a per-process monotonic id (timestamp ⊕ counter). |
| `route` | request span | The matched axum route pattern (e.g. `/api/posts/:slug`). |
| `duration_us` | event field | Time spent in the block, in microseconds. Always present on block events. |
| `message` (`"ok"` / `"block failed"`) | event field | The fixed payload string. |

### Block-specific static fields

Each block declares its own metadata via `BlockInstance::log_fields`. The
trait method has **no default impl** — adding a new block without
declaring its fields fails to compile, so the structured-log contract
can never silently regress on new kinds.

| Block | Static fields |
|-------|---------------|
| `db.find_many` / `db.find_one` / `db.insert` | `table` |
| `guard` | `predicate` (the CEL source) |
| `error` | `status`, `code` |
| `time.now` | `format` (when set) |

### Errors

A block-failure event carries the full error chain:

- `error` — `Display` of the head error (e.g. the sqlx message for a
  failed query).
- `error.chain` — the array of `source()` strings (sqlx `Database` →
  `pool acquire` → …).
- `backtrace` — present only when the process was launched with
  `RUST_BACKTRACE=1` (or `=full`). Stringified through `Display`.

## Subscriber configuration

The generated `main.rs` initialises the subscriber once at startup:

```rust
let subscriber = tracing_subscriber::fmt()
    .json()
    .flatten_event(true)
    .with_current_span(true)
    .with_span_list(true)
    .with_target(true)
    .with_max_level(tracing::Level::INFO) // from logging.level
    .finish();
tracing::subscriber::set_global_default(subscriber).ok();
```

`flatten_event(true)` writes event fields at the top of every line
(no `"fields":{...}` wrapper). Span fields stay nested under
`"span"` (the current span) and `"spans"` (the whole parent chain) so
`request_id` and `block` are both available on a single line — at a
slightly different path than the issue's flat example. A v2 custom
formatter may flatten the spans too; see issue #17 comments.

The `include` map is resolved at startup and entered as a root span
named `app`, so its fields appear on every line under `spans[0]`.

## Adding a new block

When you ship a new `BlockInstance`, you **must** implement
`log_fields()`. Returning `vec![]` is fine when the block has nothing
distinctive to surface; the kind id (`block = "..."`) is already wired
by the codegen wrapper. The compiler rejects missing implementations
with a hard error — there is no default impl.

You should also wire `tracing::error!` events before any `return` from
within the block body. The runtime helpers under
[`src/blocks/runtime.rs`](../src/blocks/runtime.rs) make this two lines
of `quote!`:

```rust
let log_err = runtime::log_block_error(ctx.index, quote! { e });
quote! {
    // ...
    if let Err(e) = something() {
        #log_err
        return crate::_rb_runtime::db_error(e);
    }
}
```

See `docs/blocks/README.md` for the full new-block checklist.

## Out of scope (v1)

- Sinks other than `stdout` (file, syslog, OTLP). Separate issue.
- Pretty / colored format. Separate issue if a use case emerges.
- Sampling, sensitive-field redaction. Separate issue.
- Metrics (Prometheus, OTLP metrics). Distinct from logging.
- Distributed tracing (W3C trace context). Distinct.
