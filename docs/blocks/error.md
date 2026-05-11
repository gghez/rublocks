# `error`

Terminal block. Short-circuits the handler with a structured HTTP error
response. Typically nested under another block's `on_missing`.

## Schema

```json
{
  "block":       "error",
  "status":      404,
  "code":        "post_not_found",
  "description": "No published post matches the given slug."
}
```

| Field | Required | Type | Notes |
|-------|----------|------|-------|
| `block` | yes | `"error"` | Discriminator. |
| `status` | yes | int | HTTP status code. Must be in the 4xx/5xx range — enforced at load time. |
| `code` | yes | string | Machine-readable error identifier. Surfaced in the response body. |
| `description` | no | string | Human-readable description. |

## Behaviour

- `kind: api` route — emits `application/json` with
  `{ "error": { "code": "...", "description": "..." } }` and the
  declared status.
- `kind: page` route — renders the route's template with the error
  context exposed (the exact template-side shape lands with the
  process-execution slice).

## Output

None. The block terminates the handler.

## Status

Parsing + validation only. The actual response emission lands with the
process-execution slice.
