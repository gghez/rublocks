# Migrations

`rublocks build` generates forward-only SQL migrations whenever the resolved
`models/*.json` schema diverges from the previous build's snapshot, and the
generated dist binary applies them via `sqlx::migrate!`.

## Workflow

1. `rublocks build` (or `rublocks dev`) reads the project, parses every model,
   and resolves the table-level `indexes`, `foreign_keys` and `checks`
   (including merged field-level shorthand — see [`models.md`](models.md)).
2. The resolved schema is compared with the snapshot persisted in
   `migrations/.state.json` after the previous build.
3. When the snapshots differ, the diff is rendered as Postgres DDL and
   appended as `migrations/NNNN_<timestamp>_<slug>.sql`. The state file is
   then refreshed.
4. `dist/migrations/` is wiped and rewritten with the full project migration
   set on every build — the dist project ships every file the user has on
   disk, in numeric order.

## First build

If `migrations/.state.json` does not exist, the current schema is adopted as
the baseline: the state file is created but **no migration is emitted**.
Anything already in `migrations/*.sql` is treated as the v1 init script.
This is what the playground does today (`migrations/0001_init.sql` is
hand-authored and predates the generator).

## Renames

Renames are detected as **drop + add** for now. The issue tracker keeps the
heuristic-match plan open; until then, expect a `DROP COLUMN` (commented
out) plus an `ADD COLUMN` instead of an `ALTER COLUMN ... RENAME`.

## Destructive operations

`DROP TABLE` and `DROP COLUMN` are emitted **commented out** by default so a
typo in JSON cannot delete user data on the next deploy. Uncomment the line
after reviewing the migration.

`DROP INDEX`, `DROP CONSTRAINT` and `ALTER COLUMN TYPE` are emitted live —
they are reversible on the data side (no rows lost) and refusing to emit
them would break the contract of "the JSON is the source of truth".

## What gets emitted

| JSON change | SQL |
|-------------|-----|
| New model | `CREATE TABLE <table> (...)` + per-table indexes / FKs / checks |
| Removed model | `-- DROP TABLE <table>;` (commented) |
| New column | `ALTER TABLE <table> ADD COLUMN <col> <type> ...` |
| Removed column | `-- ALTER TABLE <table> DROP COLUMN <col>;` (commented) |
| Column type / nullable / default change | `ALTER TABLE ... ALTER COLUMN ... TYPE ...` + nullability + default |
| New index | `CREATE [UNIQUE] INDEX <name> ON <table> (...)` |
| Removed index | `DROP INDEX IF EXISTS <name>;` |
| New foreign key | `ALTER TABLE ... ADD CONSTRAINT <name> FOREIGN KEY (...) REFERENCES ...` |
| Removed foreign key | `ALTER TABLE ... DROP CONSTRAINT IF EXISTS <name>;` |
| New / removed check | `ALTER TABLE ... ADD CONSTRAINT <name> CHECK (...)` / `DROP CONSTRAINT IF EXISTS <name>;` |

## Naming

- Migration file: `NNNN_<unix_ts>_<slug>.sql`. `NNNN` is the next sequential
  number after every existing `migrations/*.sql`.
- Index: `<table>_<columns>_idx` (or `_uniq` when unique). Override with
  `"name": "..."` on the index entry.
- Foreign key: `<table>_<field>_fkey`.
- Check: `<table>_check_<n>` when unnamed. Override with `"name": "..."`.

## Running migrations

The dist binary is generated with two entry points when the project
declares postgres **and** has at least one migration file:

```
./<app>                # serve HTTP (default)
./<app> migrate        # apply pending migrations and exit 0
./<app> migrate --list # list every migration with applied/pending state
```

In dev mode (`RUBLOCKS_DEV=1`, which `rublocks dev` sets automatically),
the binary applies pending migrations on startup before binding the
listener — the browser-driven authoring loop never needs a manual step.

In production, the recommended sequence (and the one the generated
`docker-compose.yml` uses, see issue #5) is:

```bash
./<app> migrate   # one-shot, exits when done
./<app>           # boots the HTTP server
```

The runner is `sqlx::migrate!` — no custom tracking table, no advisory
locks, no re-invented runner.

## Backends

The generator currently emits Postgres DDL only. Issue #9 will route this
through `sea-query` so the same `models/*.json` produces correct SQL for
MySQL / MariaDB / MSSQL.
