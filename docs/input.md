# Route input

A route's `input` section declares the JSON fields the handler receives.
The declaration is *typed*, and that fact alone is what wires validation:
**rublocks generates the validator automatically** from the spec. There
is no `validate.input` [block](blocks/README.md) to add — validation is
intrinsic to the input declaration.

## Schema

```json
"input": {
  "path":  { "slug":  { "type": "string", "required": true, "max_length": 200 } },
  "query": { "limit": { "type": "int",    "default": 20,    "min": 0, "max": 100 } },
  "body":  {
    "form": true,
    "fields": {
      "title": { "type": "string", "required": true, "min_length": 3, "pattern": "^[A-Za-z0-9 ]+$" }
    }
  }
}
```

Three optional sections:

| Section | Extractor | Notes |
|---------|-----------|-------|
| `path`  | `axum::extract::Path<T>` | Captures from `:param` segments in `route.path`. |
| `query` | `axum::extract::Query<T>` | URL query parameters. |
| `body`  | `axum::extract::Json<T>` (default) or `axum::extract::Form<T>` (when `form: true`) | Request body. |

`body` accepts two shapes interchangeably:

- **Flat** — `body: { "field1": {...}, "field2": {...} }` (implicit JSON body).
- **Wrapped** — `body: { "form": true|false, "fields": {...} }`. Use this
  to opt into `application/x-www-form-urlencoded` parsing for HTML form
  submissions.

## Field types

| `type`        | Rust type                       |
|---------------|---------------------------------|
| `string`      | `String`                        |
| `text`        | `String`                        |
| `email`       | `String`                        |
| `int`         | `i32`                           |
| `bigint`      | `i64`                           |
| `bool`        | `bool`                          |
| `uuid`        | `uuid::Uuid`                    |
| `timestamptz` | `chrono::DateTime<chrono::Utc>` |

## Constraints

Every constraint runs **automatically** at request time — declaring it is
the only thing the author has to do.

| Constraint | Applies to | Effect |
|------------|------------|--------|
| `required`     | any | If `true`, the field must be present; otherwise its absence triggers a 400. |
| `default`      | any | When the field is absent, the value is substituted in. Type-checked against `type` at load time. |
| `min` / `max`  | `int`, `bigint` | Numeric bounds. |
| `min_length` / `max_length` | `string`, `text`, `email` | String-length bounds. |
| `pattern`      | `string`, `text`, `email` | Regex match. **Compiled at load time** — invalid regexes are rejected before the dist crate builds. |
| `validate`     | any | CEL expression evaluated against the field's parsed value. Same syntactic check as the `guard` block's `if` and `field.validate` on models. |

Mixing rules:

- `min` / `max` are rejected for non-numeric kinds.
- `min_length` / `max_length` / `pattern` are rejected for non-string kinds.
- `default` value's JSON type must match the declared `type`.

All of the above surface as `ManifestError`s at `rublocks build` time,
with a path that pinpoints the offending key (e.g.
`input.body.title.pattern: invalid regex: unclosed character class`).

## Validation failure response

On any constraint failure the handler responds with **status 422
Unprocessable Content** ([RFC 9110 §15.5.21][rfc9110-422]) — the request
was syntactically valid (the body parsed, the path/query matched the
declared types) but does not meet the declared constraints. The 400 path
is reserved for syntactic failures, which the Axum extractors already
return on their own (e.g. malformed JSON body).

- `kind: api` route — `application/json` body:
  ```json
  { "errors": [{ "field": "query.limit", "code": "max", "message": "must be <= 100" }, ...] }
  ```
- `kind: page` route — re-renders the route's template with `status: 422`
  and exposes two extra context variables:
  - `$errors` — list of `{ field, code, message }` records.
  - `$input` — the values the user submitted (so the form can re-fill).

  Templates that don't read `$errors` simply render the page; templates
  that do can show field-level error messages without any extra wiring.

[rfc9110-422]: https://www.rfc-editor.org/rfc/rfc9110#status.422

## Referencing input values

Anywhere a `$<ref>` is accepted (block fields, `view`, `output`,
`redirect.to`), inputs are reachable via:

- `$input.path.<name>` — captured path parameter.
- `$input.query.<name>` — query parameter.
- `$input.body.<name>` — body field.

The reference resolver typechecks these against the input spec at
codegen time, so a `$input.query.limit` against an undeclared `limit`
fails the build with a clear message.

## Why input is not a block

Blocks consume named values and produce named values. The input is the
*entry point* of a handler — it doesn't fit the bind-and-reference shape
of a block, and forcing the author to write `{ "block":
"validate.input" }` on every route would defeat the declarative ergonomic.
By making validation intrinsic to the typed declaration, every route
that declares an input is, by construction, also a route that validates
it.
