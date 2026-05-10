# OpenAPI generation

Every route file with `"kind": "api"` is added to a single OpenAPI 3 spec at build time. No manual schema authoring; everything is derived from the route JSON and the referenced model files.

This is **not implemented yet** — it is documented now to lock in the contract while the playground takes shape.

## Endpoints exposed by the generated app

| Path | What |
|------|------|
| `/openapi.json` | Full OpenAPI 3 spec, served as JSON. |
| `/docs`         | Swagger UI (interactive explorer). |

Both default-on. Override via a future `openapi` block in `main.json`.

## Fields in `routes/*.json` that drive the spec

| Field | OpenAPI position | Notes |
|-------|------------------|-------|
| `path` + `method` | Operation key | Path params (`/posts/:slug`) are rewritten to `{slug}`. |
| `summary` | `operation.summary` | One-liner shown in the Swagger UI list view. |
| `description` | `operation.description` | Longer prose, Markdown allowed. |
| `tags` | `operation.tags` | Stable group names for the UI sidebar. |
| `input.path.*` | Path parameters | Types resolved from each field declaration. |
| `input.query.*` | Query parameters | Same. |
| `input.body.fields` | Request body schema | Generated as a `ToSchema`-deriving Rust struct. |
| `view` | Default 2xx response body | Built recursively from `$bindings` + model field types. |
| `status` | Default 2xx status code | Defaults: 200 for GET, 201 for POST/PUT, 204 for DELETE without body. |
| `process[block=error]` | Additional response codes | Each `error` block contributes one entry — `status` + `code` + optional `description`. |

## What is NOT exposed

- Routes with `kind: page`. Pages return HTML and don't belong in OpenAPI.
- `layouts/*.json`. Layouts are a rendering concern.
- Internal `process` steps. The pipeline is an implementation detail; only its observable I/O surfaces.

## Library

`utoipa` + `utoipa-axum` + `utoipa-swagger-ui`. The compiler emits:

- `#[derive(utoipa::ToSchema)]` on every model struct, on each `input.body` struct, and on the synthetic response struct derived from `view`.
- `#[utoipa::path(...)]` on every `kind: api` handler — including the responses table built from `error` blocks.
- An `OpenApiRouter` instead of plain `axum::Router` so registration happens by construction; no separate paths-registry to forget to update.

See [decisions.md](decisions.md#openapi-generation-automatic-via-utoipa) for the rationale.
