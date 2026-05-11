# Models (`models/*.json`)

Each file under `<project>/models/` declares one entity. The compiler emits a Rust struct per model into `dist/src/main.rs` (inside `pub mod models { ... }`), preserving the field order of the JSON source.

Field types map to Rust types and (when postgres is wired) to sqlx-compatible columns via `#[derive(sqlx::FromRow)]`.

## Schema

```json
{
  "name": "Post",
  "table": "posts",
  "fields": {
    "id":           { "type": "uuid",        "primary_key": true, "default": "gen_random_uuid()" },
    "slug":         { "type": "string",      "max_length": 200, "unique": true },
    "title":        { "type": "string",      "max_length": 200 },
    "body":         { "type": "text" },
    "author_id":    { "type": "uuid",        "references": { "model": "Author", "field": "id", "on_delete": "restrict" } },
    "published_at": { "type": "timestamptz", "nullable": true },
    "created_at":   { "type": "timestamptz", "default": "now()" },
    "updated_at":   { "type": "timestamptz", "default": "now()" }
  },
  "indexes": [
    { "fields": ["published_at"] },
    { "fields": ["author_id"] }
  ]
}
```

### Top-level fields

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `name` | string | yes | PascalCase ASCII. Becomes the Rust struct name. |
| `table` | string | yes | SQL table name. Consumed by migration generation. |
| `fields` | object | yes | Map of `column_name → field def`. Order preserved. |
| `indexes` | array | no | Table-level indexes (`fields`, `unique`, optional `name`). |
| `foreign_keys` | array | no | Table-level FKs (`field`, `references`, optional `on_delete`). |
| `checks` | array | no | Table-level check constraints (`expr`, optional `name`). |

### Field types

| `type` | Rust type | Postgres column |
|--------|-----------|-----------------|
| `uuid` | `uuid::Uuid` | `UUID` |
| `string` | `String` | `VARCHAR` |
| `text` | `String` | `TEXT` |
| `email` | `String` | `VARCHAR` (validation deferred to input parsing) |
| `int` | `i32` | `INTEGER` |
| `bigint` | `i64` | `BIGINT` |
| `bool` | `bool` | `BOOLEAN` |
| `timestamptz` | `chrono::DateTime<chrono::Utc>` | `TIMESTAMPTZ` |

`"nullable": true` wraps the Rust type in `Option<T>`. The remaining declarative attributes (`primary_key`, `default`, `unique`, `references`, `max_length`) describe the SQL column — they are parsed today and consumed by the migration generator.

### Field-level shorthand

Single-column constraints can be declared inline on the field instead of as a table-level entry. Both forms produce the same resolved model; mixing them on the same column is a parse error.

| Field-level | Equivalent table-level |
|-------------|------------------------|
| `"unique": true` on column `slug` | `{ "fields": ["slug"], "unique": true }` in `indexes` |
| `"references": "Author.id"` on `author_id` | `{ "field": "author_id", "references": "Author.id" }` in `foreign_keys` |

The object form of `references` is still accepted:

```json
"author_id": {
  "type": "uuid",
  "references": { "model": "Author", "field": "id", "on_delete": "cascade" }
}
```

`on_delete` accepts `restrict`, `cascade`, `set_null`, `no_action` (snake-cased). When omitted, the migration generator defaults to `restrict`.

### Validation at load time

The parser refuses a model file when:

- An `indexes` or `foreign_keys` entry points at an unknown column.
- A foreign key target (`Model.field`) does not resolve against the loaded model set.
- The same column carries both `"unique": true` and an explicit single-column unique index.
- The same column carries both field-level `references` and a matching entry in `foreign_keys`.

## Generated dependencies

If any model is declared:

- `serde` with `derive` is always added (for `Serialize`).
- `uuid` is added iff any model uses `type: "uuid"`.
- `chrono` is added iff any model uses `type: "timestamptz"`.
- The `sqlx` dependency (already present when `services.postgres` is set) gains the matching feature flags (`derive`, `uuid`, `chrono`).

Projects without postgres still get serializable structs; only the `FromRow` derive is gated on a postgres pool being present.

## Slice status

- **Slice 2 (current)** — struct generation only.
- **Next** — migration generation from model declarations.
- **Then** — wiring `process: db.*` blocks against these structs.
