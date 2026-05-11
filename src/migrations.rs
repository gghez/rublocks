//! Forward-only SQL migration generation from `models/*.json` diffs.
//!
//! On every `rublocks build`, the current resolved schema is compared
//! against `migrations/.state.json` (the snapshot from the previous build).
//! When the snapshots differ, a new file is written under `migrations/` and
//! the state file is refreshed. The output is Postgres DDL — issue #9 will
//! route this through `sea-query` for multi-dialect support.
//!
//! "Forward-only" means we never re-edit or re-order an existing file. Each
//! call appends one new migration with the next sequential number. Existing
//! `migrations/*.sql` files (e.g. a hand-authored `0001_init.sql`) are
//! treated as the baseline when no `.state.json` is present.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::manifest::DbKind;
use crate::models::{Check, FieldDef, FieldType, ForeignKey, Index, Model, OnDelete};

const STATE_FILE: &str = ".state.json";

/// Persisted shape of the schema, written verbatim to `.state.json`.
///
/// Kept distinct from `Model` so we control on-disk stability (field order,
/// names, defaulting) independently of the in-memory parser types. Any
/// change here is a format break that requires regenerating `.state.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snapshot {
    pub version: u32,
    pub tables: BTreeMap<String, TableSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TableSnapshot {
    pub model_name: String,
    pub columns: Vec<ColumnSnapshot>,
    pub indexes: Vec<IndexSnapshot>,
    pub foreign_keys: Vec<ForeignKeySnapshot>,
    pub checks: Vec<CheckSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ColumnSnapshot {
    pub name: String,
    pub ty: String,
    pub nullable: bool,
    pub primary_key: bool,
    pub default: Option<String>,
    pub max_length: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexSnapshot {
    pub name: String,
    pub fields: Vec<String>,
    pub unique: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForeignKeySnapshot {
    pub name: String,
    pub field: String,
    pub target_table: String,
    pub target_field: String,
    pub on_delete: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckSnapshot {
    pub name: String,
    pub expr: String,
}

/// Outcome of `generate`: a brand-new migration path (when the diff was
/// non-empty), or `None` when the resolved schema matched the state file.
pub struct GeneratedMigration {
    pub path: PathBuf,
}

/// Compute the current snapshot from the loaded model set.
///
/// `kind` selects the SQL dialect for column types. `Snapshot` carries the
/// dialect-rendered type strings inline, so the on-disk state file looks
/// identical to the migrations it produced and the diff stays stable
/// across runs of the same backend.
pub fn snapshot_from_models(models: &[Model], kind: DbKind) -> Snapshot {
    let mut tables = BTreeMap::new();
    for model in models {
        tables.insert(model.table.clone(), table_snapshot(model, kind));
    }
    Snapshot { version: 1, tables }
}

fn table_snapshot(model: &Model, kind: DbKind) -> TableSnapshot {
    let columns = model
        .fields
        .iter()
        .map(|(name, def)| ColumnSnapshot {
            name: name.clone(),
            ty: column_type(def, kind),
            nullable: def.nullable,
            primary_key: def.primary_key,
            default: def.default.clone(),
            max_length: def.max_length,
        })
        .collect();
    let indexes = model
        .indexes
        .iter()
        .map(|i| IndexSnapshot {
            name: index_name(&model.table, i),
            fields: i.fields.clone(),
            unique: i.unique,
        })
        .collect();
    let foreign_keys = model
        .foreign_keys
        .iter()
        .map(|fk| {
            let (target_model, target_field) = split_dotted(&fk.references);
            // Resolve the model name to the table name via the surrounding
            // loop's `models` slice. The function receives only the one
            // model, so we encode the *model name* here; render time will
            // map model → table via the snapshot it has.
            ForeignKeySnapshot {
                name: foreign_key_name(&model.table, fk),
                field: fk.field.clone(),
                target_table: target_model, // resolved by caller below
                target_field,
                on_delete: on_delete_label(fk.on_delete.unwrap_or(OnDelete::Restrict)),
            }
        })
        .collect();
    let checks = model
        .checks
        .iter()
        .map(|c| CheckSnapshot {
            name: check_name(&model.table, c),
            expr: c.expr.clone(),
        })
        .collect();

    TableSnapshot {
        model_name: model.name.clone(),
        columns,
        indexes,
        foreign_keys,
        checks,
    }
}

/// Resolve `target_table` from the model name embedded by `table_snapshot`.
///
/// FK references in models point at a `<Model>.<field>` pair, but the
/// migration emitter needs the SQL table name. We rewrite the snapshot
/// post-build now that every model is known.
fn resolve_fk_tables(snapshot: &mut Snapshot, models: &[Model]) {
    let name_to_table: BTreeMap<&str, &str> = models
        .iter()
        .map(|m| (m.name.as_str(), m.table.as_str()))
        .collect();
    for table in snapshot.tables.values_mut() {
        for fk in &mut table.foreign_keys {
            if let Some(real) = name_to_table.get(fk.target_table.as_str()) {
                fk.target_table = (*real).to_string();
            }
        }
    }
}

/// Read the persisted state file. Returns `None` when absent — the caller
/// treats that as "first build, baseline whatever is on disk".
pub fn load_state(project_dir: &Path) -> Result<Option<Snapshot>> {
    let path = state_path(project_dir);
    // Probe existence first so the "no state file yet" case keeps its
    // `None` shortcut. The UTF-8 decoder otherwise turns a missing file
    // into a `ManifestError::read` we'd need to special-case here.
    if !path.exists() {
        return Ok(None);
    }
    let content = crate::manifest::read_text_utf8(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let snap: Snapshot = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(snap))
}

/// Write (or overwrite) the persisted state file with the given snapshot.
fn write_state(project_dir: &Path, snapshot: &Snapshot) -> Result<()> {
    let migrations_dir = project_dir.join("migrations");
    fs::create_dir_all(&migrations_dir)
        .with_context(|| format!("failed to create {}", migrations_dir.display()))?;
    let path = state_path(project_dir);
    let body = serde_json::to_string_pretty(snapshot)?;
    fs::write(&path, body).with_context(|| format!("failed to write {}", path.display()))
}

fn state_path(project_dir: &Path) -> PathBuf {
    project_dir.join("migrations").join(STATE_FILE)
}

/// Top-level entry. Compute the current snapshot, diff it against the
/// persisted one, emit a new migration if needed, and refresh the state
/// file.
///
/// Returns `Some(GeneratedMigration)` when a new file was written, `None`
/// when the schema already matched the state file (or this is the first
/// build and existing migrations are treated as the baseline).
///
/// Mirroring to `dist/migrations/` is a separate step (`mirror`) so the
/// caller can run codegen between writing the SQL and copying it over —
/// codegen needs the project's migrations directory in its final state to
/// decide whether to wire `sqlx::migrate!`.
pub fn generate(
    project_dir: &Path,
    models: &[Model],
    kind: DbKind,
) -> Result<Option<GeneratedMigration>> {
    let mut current = snapshot_from_models(models, kind);
    resolve_fk_tables(&mut current, models);

    match load_state(project_dir)? {
        None => {
            // First build with the generator: adopt the current schema as
            // the baseline without producing a new file. Existing
            // hand-authored migrations stay as the v1 init script.
            write_state(project_dir, &current)?;
            Ok(None)
        }
        Some(prev) if prev == current => Ok(None),
        Some(prev) => {
            let changes = diff(&prev, &current);
            if changes.is_empty() {
                Ok(None)
            } else {
                let path = write_migration(project_dir, &current, &changes)?;
                write_state(project_dir, &current)?;
                Ok(Some(GeneratedMigration { path }))
            }
        }
    }
}

/// Copy `<project>/migrations/*.sql` into `<dist>/migrations/`. Wipes the
/// destination first so a removed source file does not leave a stale copy
/// in the dist project. The state file is intentionally not mirrored — it
/// is an authoring artefact, not a runtime input.
pub fn mirror(project_dir: &Path, dist_dir: &Path) -> Result<()> {
    mirror_to_dist(project_dir, dist_dir)
}

/// Does the project contain at least one migration SQL file? Used by
/// codegen to decide whether to wire `sqlx::migrate!` into the dist binary.
pub fn has_migration_files(project_dir: &Path) -> bool {
    let dir = project_dir.join("migrations");
    let Ok(rd) = fs::read_dir(&dir) else {
        return false;
    };
    rd.flatten().any(|entry| {
        entry
            .path()
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|ext| ext == "sql")
    })
}

/// Mirror `<project>/migrations/*.sql` into `<dist>/migrations/`.
///
/// Always wipes the destination first — leaving stale migrations could
/// cause `sqlx::migrate!` to refuse to start in dev.
fn mirror_to_dist(project_dir: &Path, dist_dir: &Path) -> Result<()> {
    let src = project_dir.join("migrations");
    let dst = dist_dir.join("migrations");
    if dst.exists() {
        fs::remove_dir_all(&dst).with_context(|| format!("failed to clean {}", dst.display()))?;
    }
    if !src.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(&dst).with_context(|| format!("failed to create {}", dst.display()))?;
    for entry in fs::read_dir(&src)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("sql") {
            continue;
        }
        let target = dst.join(entry.file_name());
        fs::copy(&path, &target).with_context(|| format!("failed to copy {}", path.display()))?;
    }
    Ok(())
}

/// Persist the rendered SQL under `migrations/NNNN_<ts>_<slug>.sql`.
fn write_migration(project_dir: &Path, snapshot: &Snapshot, changes: &[Change]) -> Result<PathBuf> {
    let migrations_dir = project_dir.join("migrations");
    fs::create_dir_all(&migrations_dir)?;
    let next = next_number(&migrations_dir)?;
    let slug = slug_for(changes);
    let ts = timestamp();
    let filename = format!("{next:04}_{ts}_{slug}.sql");
    let path = migrations_dir.join(filename);
    let body = render_sql(snapshot, changes);
    fs::write(&path, body).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

/// Next sequential migration number. Scans `migrations/*.sql` for files
/// whose stem starts with four digits, takes the max + 1.
fn next_number(migrations_dir: &Path) -> Result<u32> {
    let mut max = 0u32;
    for entry in fs::read_dir(migrations_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("sql") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|x| x.to_str()) else {
            continue;
        };
        let digits: String = stem.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = digits.parse::<u32>()
            && n > max
        {
            max = n;
        }
    }
    Ok(max + 1)
}

fn timestamp() -> String {
    // Plain UTC seconds-since-epoch — keeps filenames sortable without
    // pulling in chrono just to format a timestamp.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}

/// Pick a short slug for the migration filename, based on the first
/// meaningful change. Keeps filenames readable in `ls` output.
fn slug_for(changes: &[Change]) -> String {
    let raw = match changes.first() {
        Some(Change::CreateTable(name)) => format!("create_{name}"),
        Some(Change::DropTable(name)) => format!("drop_{name}"),
        Some(Change::AddColumn { table, column, .. }) => format!("add_{table}_{column}"),
        Some(Change::DropColumn { table, column, .. }) => format!("drop_{table}_{column}"),
        Some(Change::AlterColumn { table, column, .. }) => format!("alter_{table}_{column}"),
        Some(Change::AddIndex { table, .. }) => format!("add_{table}_index"),
        Some(Change::DropIndex { table, .. }) => format!("drop_{table}_index"),
        Some(Change::AddForeignKey { table, .. }) => format!("add_{table}_fk"),
        Some(Change::DropForeignKey { table, .. }) => format!("drop_{table}_fk"),
        Some(Change::AddCheck { table, .. }) => format!("add_{table}_check"),
        Some(Change::DropCheck { table, .. }) => format!("drop_{table}_check"),
        None => "noop".to_string(),
    };
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// The set of changes between two snapshots. Ordered so that referenced
/// tables come before referencing ones (CREATE TABLE before ADD FK, etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    CreateTable(String),
    DropTable(String),
    AddColumn {
        table: String,
        column: String,
        spec: ColumnSnapshot,
    },
    DropColumn {
        table: String,
        column: String,
    },
    AlterColumn {
        table: String,
        column: String,
        from: ColumnSnapshot,
        to: ColumnSnapshot,
    },
    AddIndex {
        table: String,
        index: IndexSnapshot,
    },
    DropIndex {
        table: String,
        index: IndexSnapshot,
    },
    AddForeignKey {
        table: String,
        fk: ForeignKeySnapshot,
    },
    DropForeignKey {
        table: String,
        fk: ForeignKeySnapshot,
    },
    AddCheck {
        table: String,
        check: CheckSnapshot,
    },
    DropCheck {
        table: String,
        check: CheckSnapshot,
    },
}

/// Produce the ordered list of changes between `prev` and `current`.
pub fn diff(prev: &Snapshot, current: &Snapshot) -> Vec<Change> {
    let mut changes = Vec::new();

    // Drop tables that disappeared (rendered as commented-out DDL).
    for table in prev.tables.keys() {
        if !current.tables.contains_key(table) {
            changes.push(Change::DropTable(table.clone()));
        }
    }

    // Create new tables first so subsequent FK adds can resolve.
    for (table, snap) in &current.tables {
        if !prev.tables.contains_key(table) {
            changes.push(Change::CreateTable(table.clone()));
            for idx in &snap.indexes {
                changes.push(Change::AddIndex {
                    table: table.clone(),
                    index: idx.clone(),
                });
            }
            for fk in &snap.foreign_keys {
                changes.push(Change::AddForeignKey {
                    table: table.clone(),
                    fk: fk.clone(),
                });
            }
            for c in &snap.checks {
                changes.push(Change::AddCheck {
                    table: table.clone(),
                    check: c.clone(),
                });
            }
        }
    }

    // Per-table column / index / fk / check diffs.
    for (table, cur_snap) in &current.tables {
        let Some(prev_snap) = prev.tables.get(table) else {
            continue;
        };
        diff_columns(table, prev_snap, cur_snap, &mut changes);
        diff_named(
            table,
            &prev_snap.indexes,
            &cur_snap.indexes,
            &mut changes,
            |i| i.name.clone(),
        );
        diff_named(
            table,
            &prev_snap.foreign_keys,
            &cur_snap.foreign_keys,
            &mut changes,
            |f| f.name.clone(),
        );
        diff_checks(table, &prev_snap.checks, &cur_snap.checks, &mut changes);
    }

    changes
}

fn diff_columns(table: &str, prev: &TableSnapshot, cur: &TableSnapshot, out: &mut Vec<Change>) {
    let prev_cols: BTreeMap<&str, &ColumnSnapshot> =
        prev.columns.iter().map(|c| (c.name.as_str(), c)).collect();
    let cur_cols: BTreeMap<&str, &ColumnSnapshot> =
        cur.columns.iter().map(|c| (c.name.as_str(), c)).collect();
    for (name, col) in &cur_cols {
        match prev_cols.get(name) {
            None => out.push(Change::AddColumn {
                table: table.to_string(),
                column: (*name).to_string(),
                spec: (*col).clone(),
            }),
            Some(prev_col) if prev_col != col => out.push(Change::AlterColumn {
                table: table.to_string(),
                column: (*name).to_string(),
                from: (*prev_col).clone(),
                to: (*col).clone(),
            }),
            _ => {}
        }
    }
    for name in prev_cols.keys() {
        if !cur_cols.contains_key(name) {
            out.push(Change::DropColumn {
                table: table.to_string(),
                column: (*name).to_string(),
            });
        }
    }
}

/// Generic add/drop diff for collections keyed by a stable name.
fn diff_named<T>(
    table: &str,
    prev: &[T],
    cur: &[T],
    out: &mut Vec<Change>,
    key: impl Fn(&T) -> String,
) where
    T: Clone + Eq + NamedChange,
{
    let prev_by_name: BTreeMap<String, &T> = prev.iter().map(|t| (key(t), t)).collect();
    let cur_by_name: BTreeMap<String, &T> = cur.iter().map(|t| (key(t), t)).collect();
    for (name, item) in &cur_by_name {
        match prev_by_name.get(name) {
            None => out.push(T::add(table, (*item).clone())),
            Some(prev_item) if prev_item != item => {
                // Same name, different body: drop + add to keep the
                // emitter simple (issue #6 explicitly skips renames).
                out.push(T::drop(table, (*prev_item).clone()));
                out.push(T::add(table, (*item).clone()));
            }
            _ => {}
        }
    }
    for (name, item) in &prev_by_name {
        if !cur_by_name.contains_key(name) {
            out.push(T::drop(table, (*item).clone()));
        }
    }
}

/// Maps a snapshot type to its add/drop Change variants. Lets `diff_named`
/// stay generic over `Index`, `ForeignKey` (and any future addition).
trait NamedChange: Sized {
    fn add(table: &str, value: Self) -> Change;
    fn drop(table: &str, value: Self) -> Change;
}

impl NamedChange for IndexSnapshot {
    fn add(table: &str, value: Self) -> Change {
        Change::AddIndex {
            table: table.to_string(),
            index: value,
        }
    }
    fn drop(table: &str, value: Self) -> Change {
        Change::DropIndex {
            table: table.to_string(),
            index: value,
        }
    }
}

impl NamedChange for ForeignKeySnapshot {
    fn add(table: &str, value: Self) -> Change {
        Change::AddForeignKey {
            table: table.to_string(),
            fk: value,
        }
    }
    fn drop(table: &str, value: Self) -> Change {
        Change::DropForeignKey {
            table: table.to_string(),
            fk: value,
        }
    }
}

fn diff_checks(table: &str, prev: &[CheckSnapshot], cur: &[CheckSnapshot], out: &mut Vec<Change>) {
    let prev_by_name: BTreeMap<&str, &CheckSnapshot> =
        prev.iter().map(|c| (c.name.as_str(), c)).collect();
    let cur_by_name: BTreeMap<&str, &CheckSnapshot> =
        cur.iter().map(|c| (c.name.as_str(), c)).collect();
    for (name, item) in &cur_by_name {
        match prev_by_name.get(name) {
            None => out.push(Change::AddCheck {
                table: table.to_string(),
                check: (*item).clone(),
            }),
            Some(prev_item) if prev_item != item => {
                out.push(Change::DropCheck {
                    table: table.to_string(),
                    check: (*prev_item).clone(),
                });
                out.push(Change::AddCheck {
                    table: table.to_string(),
                    check: (*item).clone(),
                });
            }
            _ => {}
        }
    }
    for (name, item) in &prev_by_name {
        if !cur_by_name.contains_key(name) {
            out.push(Change::DropCheck {
                table: table.to_string(),
                check: (*item).clone(),
            });
        }
    }
}

/// Render the migration body. Drops are emitted commented-out by default —
/// the user is expected to uncomment after reviewing the migration.
fn render_sql(snapshot: &Snapshot, changes: &[Change]) -> String {
    let mut out = String::new();
    out.push_str("-- Generated by rublocks. Do not edit by hand.\n");
    out.push_str("-- Destructive operations are commented out — review and uncomment them if intentional.\n\n");

    // Collect every table referenced by the changes so we can render
    // CREATE TABLE statements with their columns inline.
    let mut created: BTreeSet<&str> = BTreeSet::new();
    for change in changes {
        if let Change::CreateTable(name) = change {
            created.insert(name.as_str());
        }
    }

    for change in changes {
        match change {
            Change::CreateTable(name) => {
                if let Some(table) = snapshot.tables.get(name) {
                    out.push_str(&render_create_table(name, table));
                }
            }
            Change::DropTable(name) => {
                out.push_str(&format!("-- DROP TABLE {name};\n\n"));
            }
            Change::AddColumn {
                table,
                column,
                spec,
            } => {
                out.push_str(&format!(
                    "ALTER TABLE {table} ADD COLUMN {col};\n\n",
                    col = render_column(column, spec)
                ));
            }
            Change::DropColumn { table, column } => {
                out.push_str(&format!("-- ALTER TABLE {table} DROP COLUMN {column};\n\n"));
            }
            Change::AlterColumn {
                table, column, to, ..
            } => {
                out.push_str(&render_alter_column(table, column, to));
            }
            Change::AddIndex { table, index } => {
                if created.contains(table.as_str()) {
                    // CREATE INDEX is emitted alongside CREATE TABLE.
                    continue;
                }
                out.push_str(&render_create_index(table, index));
            }
            Change::DropIndex { index, .. } => {
                out.push_str(&format!("DROP INDEX IF EXISTS {};\n\n", index.name));
            }
            Change::AddForeignKey { table, fk } => {
                if created.contains(table.as_str()) {
                    continue;
                }
                out.push_str(&render_add_foreign_key(table, fk));
            }
            Change::DropForeignKey { table, fk } => {
                out.push_str(&format!(
                    "ALTER TABLE {table} DROP CONSTRAINT IF EXISTS {};\n\n",
                    fk.name
                ));
            }
            Change::AddCheck { table, check } => {
                if created.contains(table.as_str()) {
                    continue;
                }
                out.push_str(&format!(
                    "ALTER TABLE {table} ADD CONSTRAINT {name} CHECK ({expr});\n\n",
                    name = check.name,
                    expr = check.expr,
                ));
            }
            Change::DropCheck { table, check } => {
                out.push_str(&format!(
                    "ALTER TABLE {table} DROP CONSTRAINT IF EXISTS {};\n\n",
                    check.name
                ));
            }
        }
    }
    out
}

fn render_create_table(name: &str, table: &TableSnapshot) -> String {
    let mut out = String::new();
    out.push_str(&format!("CREATE TABLE {name} (\n"));
    let pks: Vec<&str> = table
        .columns
        .iter()
        .filter(|c| c.primary_key)
        .map(|c| c.name.as_str())
        .collect();
    let single_pk = pks.len() == 1;
    let mut lines: Vec<String> = Vec::new();
    for col in &table.columns {
        let mut line = format!("    {}", render_column(&col.name, col));
        if single_pk && col.primary_key {
            line.push_str(" PRIMARY KEY");
        }
        lines.push(line);
    }
    if pks.len() > 1 {
        lines.push(format!("    PRIMARY KEY ({})", pks.join(", ")));
    }
    for fk in &table.foreign_keys {
        lines.push(render_inline_foreign_key(fk));
    }
    for c in &table.checks {
        lines.push(format!(
            "    CONSTRAINT {name} CHECK ({expr})",
            name = c.name,
            expr = c.expr,
        ));
    }
    out.push_str(&lines.join(",\n"));
    out.push_str("\n);\n\n");
    for idx in &table.indexes {
        out.push_str(&render_create_index(name, idx));
    }
    out
}

fn render_inline_foreign_key(fk: &ForeignKeySnapshot) -> String {
    format!(
        "    CONSTRAINT {name} FOREIGN KEY ({field}) REFERENCES {target} ({target_field}) ON DELETE {on_delete}",
        name = fk.name,
        field = fk.field,
        target = fk.target_table,
        target_field = fk.target_field,
        on_delete = on_delete_sql(&fk.on_delete),
    )
}

fn render_add_foreign_key(table: &str, fk: &ForeignKeySnapshot) -> String {
    format!(
        "ALTER TABLE {table} ADD CONSTRAINT {name} FOREIGN KEY ({field}) REFERENCES {target} ({target_field}) ON DELETE {on_delete};\n\n",
        name = fk.name,
        field = fk.field,
        target = fk.target_table,
        target_field = fk.target_field,
        on_delete = on_delete_sql(&fk.on_delete),
    )
}

fn render_create_index(table: &str, idx: &IndexSnapshot) -> String {
    let unique = if idx.unique { "UNIQUE " } else { "" };
    format!(
        "CREATE {unique}INDEX {name} ON {table} ({cols});\n\n",
        name = idx.name,
        cols = idx.fields.join(", "),
    )
}

fn render_alter_column(table: &str, column: &str, to: &ColumnSnapshot) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "ALTER TABLE {table} ALTER COLUMN {column} TYPE {ty};\n",
        ty = sql_type(to)
    ));
    let nullability = if to.nullable {
        "DROP NOT NULL"
    } else {
        "SET NOT NULL"
    };
    out.push_str(&format!(
        "ALTER TABLE {table} ALTER COLUMN {column} {nullability};\n"
    ));
    match &to.default {
        Some(expr) => out.push_str(&format!(
            "ALTER TABLE {table} ALTER COLUMN {column} SET DEFAULT {expr};\n"
        )),
        None => out.push_str(&format!(
            "ALTER TABLE {table} ALTER COLUMN {column} DROP DEFAULT;\n"
        )),
    }
    out.push('\n');
    out
}

fn render_column(name: &str, col: &ColumnSnapshot) -> String {
    let mut parts = vec![name.to_string(), sql_type(col)];
    if !col.nullable {
        parts.push("NOT NULL".to_string());
    }
    if let Some(default) = &col.default {
        parts.push(format!("DEFAULT {default}"));
    }
    parts.join(" ")
}

fn sql_type(col: &ColumnSnapshot) -> String {
    // Field type strings produced by `column_type` carry an optional
    // `(N)` length suffix already; nothing else to do here.
    col.ty.clone()
}

fn column_type(def: &FieldDef, kind: DbKind) -> String {
    let varchar_default_len = 255u32;
    match (def.ty, kind) {
        (FieldType::Uuid, DbKind::Postgres) => "UUID".to_string(),
        (FieldType::Uuid, DbKind::Mysql | DbKind::Mariadb) => "BINARY(16)".to_string(),
        (FieldType::Uuid, DbKind::Mssql) => "UNIQUEIDENTIFIER".to_string(),

        (FieldType::String, _) => {
            let n = def.max_length.unwrap_or(varchar_default_len);
            match kind {
                DbKind::Mssql => format!("NVARCHAR({n})"),
                _ => format!("VARCHAR({n})"),
            }
        }

        (FieldType::Text, DbKind::Postgres) => "TEXT".to_string(),
        (FieldType::Text, DbKind::Mysql | DbKind::Mariadb) => "LONGTEXT".to_string(),
        (FieldType::Text, DbKind::Mssql) => "NVARCHAR(MAX)".to_string(),

        (FieldType::Email, _) => {
            let n = def.max_length.unwrap_or(varchar_default_len);
            match kind {
                DbKind::Mssql => format!("NVARCHAR({n})"),
                _ => format!("VARCHAR({n})"),
            }
        }

        (FieldType::Int, _) => "INTEGER".to_string(),
        (FieldType::Bigint, _) => "BIGINT".to_string(),

        (FieldType::Bool, DbKind::Postgres) => "BOOLEAN".to_string(),
        (FieldType::Bool, DbKind::Mysql | DbKind::Mariadb) => "TINYINT(1)".to_string(),
        (FieldType::Bool, DbKind::Mssql) => "BIT".to_string(),

        (FieldType::Timestamptz, DbKind::Postgres) => "TIMESTAMPTZ".to_string(),
        (FieldType::Timestamptz, DbKind::Mysql | DbKind::Mariadb) => "DATETIME".to_string(),
        (FieldType::Timestamptz, DbKind::Mssql) => "DATETIMEOFFSET".to_string(),
    }
}

fn split_dotted(s: &str) -> (String, String) {
    match s.split_once('.') {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => (s.to_string(), String::new()),
    }
}

fn on_delete_label(action: OnDelete) -> String {
    match action {
        OnDelete::Restrict => "restrict",
        OnDelete::Cascade => "cascade",
        OnDelete::SetNull => "set_null",
        OnDelete::NoAction => "no_action",
    }
    .to_string()
}

fn on_delete_sql(label: &str) -> &'static str {
    match label {
        "cascade" => "CASCADE",
        "set_null" => "SET NULL",
        "no_action" => "NO ACTION",
        _ => "RESTRICT",
    }
}

fn index_name(table: &str, idx: &Index) -> String {
    if let Some(explicit) = &idx.name {
        return explicit.clone();
    }
    let cols = idx.fields.join("_");
    let suffix = if idx.unique { "_uniq" } else { "_idx" };
    format!("{table}_{cols}{suffix}")
}

fn foreign_key_name(table: &str, fk: &ForeignKey) -> String {
    format!("{table}_{}_fkey", fk.field)
}

fn check_name(table: &str, check: &Check) -> String {
    check
        .name
        .clone()
        .unwrap_or_else(|| format!("{table}_check_{}", check.expr.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn col(name: &str, ty: &str) -> ColumnSnapshot {
        ColumnSnapshot {
            name: name.to_string(),
            ty: ty.to_string(),
            nullable: false,
            primary_key: false,
            default: None,
            max_length: None,
        }
    }

    fn snapshot_from(tables: Vec<(&str, TableSnapshot)>) -> Snapshot {
        Snapshot {
            version: 1,
            tables: tables
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        }
    }

    fn empty_table(name: &str, cols: Vec<ColumnSnapshot>) -> TableSnapshot {
        TableSnapshot {
            model_name: name.to_string(),
            columns: cols,
            indexes: vec![],
            foreign_keys: vec![],
            checks: vec![],
        }
    }

    #[test]
    fn first_build_writes_state_and_emits_no_migration() {
        let dir = tempfile::TempDir::new().unwrap();
        let project = dir.path();
        let dist = project.join("dist");
        fs::create_dir_all(&dist).unwrap();

        let models = vec![Model {
            name: "Post".to_string(),
            table: "posts".to_string(),
            fields: IndexMap::new(),
            indexes: vec![],
            foreign_keys: vec![],
            checks: vec![],
        }];
        let out = generate(project, &models, DbKind::Postgres).unwrap();
        mirror(project, &dist).unwrap();
        assert!(out.is_none(), "first build must not emit a new file");
        assert!(state_path(project).exists());
    }

    #[test]
    fn adding_a_new_table_emits_create_table() {
        let dir = tempfile::TempDir::new().unwrap();
        let project = dir.path();
        let dist = project.join("dist");
        fs::create_dir_all(&dist).unwrap();
        let initial = snapshot_from(vec![]);
        write_state(project, &initial).unwrap();

        let mut fields: IndexMap<String, FieldDef> = IndexMap::new();
        fields.insert(
            "id".to_string(),
            FieldDef {
                ty: FieldType::Uuid,
                nullable: false,
                primary_key: true,
                unique: false,
                default: Some("gen_random_uuid()".to_string()),
                max_length: None,
                references: None,
                validate: None,
            },
        );
        let models = vec![Model {
            name: "Post".to_string(),
            table: "posts".to_string(),
            fields,
            indexes: vec![],
            foreign_keys: vec![],
            checks: vec![],
        }];
        let out = generate(project, &models, DbKind::Postgres).unwrap();
        mirror(project, &dist).unwrap();
        let path = out.expect("new migration").path;
        assert!(path.exists());
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("CREATE TABLE posts"));
        assert!(body.contains("id UUID"));
        assert!(body.contains("PRIMARY KEY"));
        // Mirrored into dist/.
        let dist_file = dist.join("migrations").join(path.file_name().unwrap());
        assert!(dist_file.exists(), "{} should exist", dist_file.display());
    }

    #[test]
    fn adding_a_column_emits_alter_table_add_column() {
        let dir = tempfile::TempDir::new().unwrap();
        let project = dir.path();
        let dist = project.join("dist");
        fs::create_dir_all(&dist).unwrap();
        let initial = snapshot_from(vec![(
            "posts",
            empty_table("Post", vec![col("id", "UUID")]),
        )]);
        write_state(project, &initial).unwrap();

        let mut fields: IndexMap<String, FieldDef> = IndexMap::new();
        fields.insert(
            "id".to_string(),
            FieldDef {
                ty: FieldType::Uuid,
                nullable: false,
                primary_key: false,
                unique: false,
                default: None,
                max_length: None,
                references: None,
                validate: None,
            },
        );
        fields.insert(
            "title".to_string(),
            FieldDef {
                ty: FieldType::String,
                nullable: false,
                primary_key: false,
                unique: false,
                default: None,
                max_length: Some(200),
                references: None,
                validate: None,
            },
        );
        let models = vec![Model {
            name: "Post".to_string(),
            table: "posts".to_string(),
            fields,
            indexes: vec![],
            foreign_keys: vec![],
            checks: vec![],
        }];
        let path = generate(project, &models, DbKind::Postgres)
            .unwrap()
            .unwrap()
            .path;
        mirror(project, &dist).unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("ALTER TABLE posts ADD COLUMN title VARCHAR(200)"));
    }

    #[test]
    fn changing_a_column_type_emits_alter_column_type() {
        let dir = tempfile::TempDir::new().unwrap();
        let project = dir.path();
        let dist = project.join("dist");
        fs::create_dir_all(&dist).unwrap();
        let mut prev_col = col("body", "TEXT");
        prev_col.nullable = true;
        let initial = snapshot_from(vec![("posts", empty_table("Post", vec![prev_col]))]);
        write_state(project, &initial).unwrap();

        let mut fields: IndexMap<String, FieldDef> = IndexMap::new();
        fields.insert(
            "body".to_string(),
            FieldDef {
                ty: FieldType::String,
                nullable: false,
                primary_key: false,
                unique: false,
                default: None,
                max_length: Some(500),
                references: None,
                validate: None,
            },
        );
        let models = vec![Model {
            name: "Post".to_string(),
            table: "posts".to_string(),
            fields,
            indexes: vec![],
            foreign_keys: vec![],
            checks: vec![],
        }];
        let path = generate(project, &models, DbKind::Postgres)
            .unwrap()
            .unwrap()
            .path;
        mirror(project, &dist).unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("ALTER TABLE posts ALTER COLUMN body TYPE VARCHAR(500)"),
            "got: {body}"
        );
        assert!(body.contains("SET NOT NULL"));
    }

    #[test]
    fn dropping_a_column_is_commented_out() {
        let dir = tempfile::TempDir::new().unwrap();
        let project = dir.path();
        let dist = project.join("dist");
        fs::create_dir_all(&dist).unwrap();
        let initial = snapshot_from(vec![(
            "posts",
            empty_table("Post", vec![col("id", "UUID"), col("body", "TEXT")]),
        )]);
        write_state(project, &initial).unwrap();

        let mut fields: IndexMap<String, FieldDef> = IndexMap::new();
        fields.insert(
            "id".to_string(),
            FieldDef {
                ty: FieldType::Uuid,
                nullable: false,
                primary_key: false,
                unique: false,
                default: None,
                max_length: None,
                references: None,
                validate: None,
            },
        );
        let models = vec![Model {
            name: "Post".to_string(),
            table: "posts".to_string(),
            fields,
            indexes: vec![],
            foreign_keys: vec![],
            checks: vec![],
        }];
        let path = generate(project, &models, DbKind::Postgres)
            .unwrap()
            .unwrap()
            .path;
        mirror(project, &dist).unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("-- ALTER TABLE posts DROP COLUMN body;"),
            "got: {body}"
        );
    }

    #[test]
    fn next_number_skips_non_numeric_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let migrations = dir.path().join("migrations");
        fs::create_dir_all(&migrations).unwrap();
        fs::write(migrations.join("0001_init.sql"), "-- init").unwrap();
        fs::write(migrations.join("0007_add_thing.sql"), "-- thing").unwrap();
        fs::write(migrations.join("notes.txt"), "ignored").unwrap();
        assert_eq!(next_number(&migrations).unwrap(), 8);
    }

    #[test]
    fn mysql_dialect_maps_uuid_text_and_bool() {
        let def = FieldDef {
            ty: FieldType::Uuid,
            nullable: false,
            primary_key: false,
            unique: false,
            default: None,
            max_length: None,
            references: None,
            validate: None,
        };
        assert_eq!(column_type(&def, DbKind::Mysql), "BINARY(16)");
        assert_eq!(column_type(&def, DbKind::Mariadb), "BINARY(16)");
        let def = FieldDef {
            ty: FieldType::Text,
            ..def
        };
        assert_eq!(column_type(&def, DbKind::Mysql), "LONGTEXT");
        let def = FieldDef {
            ty: FieldType::Bool,
            ..def
        };
        assert_eq!(column_type(&def, DbKind::Mysql), "TINYINT(1)");
        let def = FieldDef {
            ty: FieldType::Timestamptz,
            ..def
        };
        assert_eq!(column_type(&def, DbKind::Mysql), "DATETIME");
    }

    #[test]
    fn mssql_dialect_maps_uuid_and_text() {
        let def = FieldDef {
            ty: FieldType::Uuid,
            nullable: false,
            primary_key: false,
            unique: false,
            default: None,
            max_length: None,
            references: None,
            validate: None,
        };
        assert_eq!(column_type(&def, DbKind::Mssql), "UNIQUEIDENTIFIER");
        let def = FieldDef {
            ty: FieldType::Text,
            ..def
        };
        assert_eq!(column_type(&def, DbKind::Mssql), "NVARCHAR(MAX)");
        let def = FieldDef {
            ty: FieldType::String,
            max_length: Some(50),
            ..def
        };
        assert_eq!(column_type(&def, DbKind::Mssql), "NVARCHAR(50)");
    }

    #[test]
    fn postgres_dialect_remains_unchanged() {
        let def = FieldDef {
            ty: FieldType::Uuid,
            nullable: false,
            primary_key: false,
            unique: false,
            default: None,
            max_length: None,
            references: None,
            validate: None,
        };
        assert_eq!(column_type(&def, DbKind::Postgres), "UUID");
        let def = FieldDef {
            ty: FieldType::Timestamptz,
            ..def
        };
        assert_eq!(column_type(&def, DbKind::Postgres), "TIMESTAMPTZ");
    }

    #[test]
    fn diff_is_empty_when_snapshots_match() {
        let a = snapshot_from(vec![(
            "posts",
            empty_table("Post", vec![col("id", "UUID")]),
        )]);
        assert!(diff(&a, &a).is_empty());
    }
}
