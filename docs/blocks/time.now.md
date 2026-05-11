# `time.now`

Read-side scalar block. Binds the current wall-clock time to `$<name>`
as a `String`, optionally formatted via a `chrono` strftime pattern.

## Schema

```json
{
  "name":   "year",
  "block":  "time.now",
  "format": "%Y"
}
```

| Field | Required | Type | Notes |
|-------|----------|------|-------|
| `block` | yes | `"time.now"` | Discriminator. |
| `name` | yes | string | Binding for `$<name>` references. |
| `format` | no | string | `chrono` strftime pattern (e.g. `"%Y"`, `"%Y-%m-%d %H:%M:%S"`). Defaults to RFC 3339 when omitted. |
| `timezone` | no | string | Currently only `"utc"` is supported — enforced at load time. |

## Output

- `$<name>` resolves to `String`. Templates render it via `Display`.

## Runtime

The block emits `chrono::Utc::now().format(<pattern>).to_string()` at
the start of the handler body and binds the result to `$<name>`.
