//! The "block" abstraction — the unit of logic inside a route's `process`.
//!
//! The project name itself is an etymology cue: rublocks = "rust blocks".
//! Every route's behaviour is a *composition of blocks*: small declarative
//! steps with a standardised input contract (the JSON fields a block reads)
//! and a standardised output contract (an optional named binding that other
//! blocks and `view` / `output` can reference via `$<name>`).
//!
//! ## Adding a new block
//!
//! 1. Create `src/blocks/<id>.rs` with a `Spec` struct (`deny_unknown_fields`),
//!    an `Instance` impl of `BlockInstance`, and a `Kind` impl of `BlockKind`.
//! 2. Register the kind in `BUILTIN_KINDS` below.
//! 3. Write the matching documentation page at `docs/blocks/<id>.md` — the
//!    presence of this file is enforced by an integration test so the
//!    catalogue cannot drift from the registry.
//!
//! See `docs/blocks/README.md` for the user-facing contract.

use indexmap::IndexMap;
use proc_macro2::TokenStream;
use schemars::schema::RootSchema;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::manifest::ManifestError;
use crate::models::Model;
use crate::value_ref::ValueScope;
use crate::where_clause::WhereSpec;

use self::runtime::BlockCodegenCtx;

pub mod db_find_many;
pub mod db_find_one;
pub mod db_insert;
pub mod error;
pub mod guard;
pub mod runtime;
pub mod time_now;

/// Raw, untyped form of one process block.
///
/// Captures the discriminator (`block`) and every other field as opaque JSON.
/// Each [`BlockKind`] takes a `RawBlock` and turns it into a typed
/// [`BlockInstance`], rejecting unknown fields and validating constraints.
#[derive(Debug, Clone)]
pub struct RawBlock {
    /// Discriminator value — matches one of the registered kind ids.
    pub block: String,
    /// Every other field of the JSON object, preserving source order so
    /// later codegen can reproduce author intent verbatim.
    pub fields: IndexMap<String, Value>,
    /// File the block came from. Carried through so validation errors point
    /// at the right place to edit.
    pub source: PathBuf,
    /// Human-readable position inside its parent file (e.g. `process[1]` or
    /// `process[1].on_missing`). Surfaces in error messages.
    pub label: String,
}

impl RawBlock {
    /// Convert a JSON value into a [`RawBlock`], extracting the `block`
    /// discriminator and keeping every other field as-is.
    pub fn from_value(value: &Value, source: &Path, label: &str) -> Result<Self, ManifestError> {
        let Value::Object(map) = value else {
            return Err(ManifestError::validation(
                source,
                format!("{label}: expected an object describing a block"),
            ));
        };
        let block = match map.get("block") {
            Some(Value::String(s)) => s.clone(),
            Some(_) => {
                return Err(ManifestError::validation(
                    source,
                    format!("{label}: `block` must be a string"),
                ));
            }
            None => {
                return Err(ManifestError::validation(
                    source,
                    format!("{label}: missing `block` discriminator"),
                ));
            }
        };
        let mut fields = IndexMap::with_capacity(map.len().saturating_sub(1));
        for (k, v) in map {
            if k != "block" {
                fields.insert(k.clone(), v.clone());
            }
        }
        Ok(RawBlock {
            block,
            fields,
            source: source.to_path_buf(),
            label: label.to_string(),
        })
    }

    /// Reassemble the original JSON object — `block` discriminator + every
    /// other field — for downstream `serde_json::from_value` consumption.
    pub(crate) fn as_full_object(&self) -> Value {
        let mut obj = Map::with_capacity(self.fields.len() + 1);
        obj.insert("block".to_string(), Value::String(self.block.clone()));
        for (k, v) in &self.fields {
            obj.insert(k.clone(), v.clone());
        }
        Value::Object(obj)
    }

    /// Wrap a `serde_json` error so it points at the file + label this block
    /// came from. Surface for [`BlockKind::parse`] implementations.
    pub(crate) fn parse_error(&self, err: serde_json::Error) -> ManifestError {
        ManifestError::validation(&self.source, format!("{}: {err}", self.label))
    }

    /// Build a structured validation error for this block.
    pub(crate) fn validation_error(&self, message: impl Into<String>) -> ManifestError {
        let m: String = message.into();
        ManifestError::validation(&self.source, format!("{}: {m}", self.label))
    }
}

/// Static description of one kind of block.
///
/// One implementor per `src/blocks/<id>.rs`. Registered in [`BUILTIN_KINDS`].
pub trait BlockKind: Send + Sync {
    /// Discriminator value the manifest uses (e.g. `"db.find_many"`).
    fn id(&self) -> &'static str;

    /// JSON Schema (Draft-07) of the block's accepted fields, including the
    /// `block` discriminator. Consumed by the agent installers so each
    /// per-block surface shows up explicitly in `AGENTS.md` / Cursor / Claude.
    fn json_schema(&self) -> RootSchema;

    /// Parse a raw block into a typed instance. Implementations MUST reject
    /// unknown fields and validate constraints (CEL syntax, references…).
    fn parse(&self, raw: &RawBlock) -> Result<Box<dyn BlockInstance>, ManifestError>;
}

/// One parsed, validated process block.
///
/// Each block decides its own output type from the loaded model set —
/// scalars (e.g. `time.now` → `String`), model lookups (`db.find_*` →
/// `Vec<Post>` / `Post`), and write-side blocks that bind nothing all flow
/// through the same trait.
pub trait BlockInstance: std::fmt::Debug + Send + Sync {
    /// Discriminator of the kind that produced this instance.
    ///
    /// Surface for tests and tools that need to identify the kind of a
    /// parsed block without pattern-matching on a concrete type.
    #[allow(dead_code)]
    fn kind_id(&self) -> &'static str;

    /// Binding name for `$<name>` references. `None` for write-side blocks.
    fn name(&self) -> Option<&str>;

    /// Rust type for `$<name>` references. `None` for write-side blocks and
    /// for blocks whose target model is unknown — codegen falls back to
    /// `String` in either case.
    fn output_type(&self, models: &[Model]) -> Option<TokenStream>;

    /// Emit the Rust tokens that execute this block at request time.
    ///
    /// Each implementor resolves its inputs against `scope`, emits the
    /// runtime call (sqlx query, CEL eval, error response, …) and — if
    /// the block has a `name` — registers a fresh [`crate::value_ref::ScopeBinding`]
    /// so downstream blocks and `view` / `output` can reference it.
    ///
    /// Returning `Err(_)` aborts `rublocks build` with a manifest error
    /// pointing at the offending block.
    fn emit_code(
        &self,
        ctx: &BlockCodegenCtx,
        scope: &mut ValueScope,
    ) -> Result<TokenStream, String>;

    /// True when this block embeds at least one CEL expression that the
    /// generated handler must evaluate at runtime. Drives the conditional
    /// emission of the `cel` dependency in the dist `Cargo.toml` (see
    /// `expressions::project_uses_cel`).
    fn embeds_runtime_cel(&self) -> bool {
        false
    }

    /// The CEL predicate this block evaluates before passing through, if
    /// any. Only the `guard` block returns `Some`; codegen treats `false`
    /// as a request to short-circuit with `403`.
    fn guard_if(&self) -> Option<&str> {
        None
    }

    /// Parsed `where:` clause. Both the CEL string form and the
    /// structured object form land here so build-time scope checking +
    /// runtime SQL emission share a single typed representation.
    fn where_spec(&self) -> Option<&WhereSpec> {
        None
    }

    /// The model table this block targets, if any. Paired with
    /// [`Self::where_spec`] so the scope-checker can resolve column names
    /// and runtime codegen can pick the right `FROM` table.
    fn target_table(&self) -> Option<&str> {
        None
    }

    /// Column → value map for `db.insert` blocks. The scope-checker walks
    /// these to make sure every `$<ref>` resolves in the block's scope.
    /// Default `None` for non-insert blocks.
    fn insert_values(&self) -> Option<&indexmap::IndexMap<String, crate::value_ref::ValueRef>> {
        None
    }
}

/// Resolve the model struct (if any) matching a table name. Helper exposed
/// for built-ins whose output type is `Vec<T>` or `T` for some declared
/// model — they all share the same lookup logic.
pub fn model_for_table<'a>(models: &'a [Model], table: &str) -> Option<&'a Model> {
    models.iter().find(|m| m.table == table)
}

/// Registry mapping block ids to their static [`BlockKind`].
pub struct BlockRegistry {
    kinds: Vec<&'static dyn BlockKind>,
}

impl BlockRegistry {
    fn new() -> Self {
        let mut kinds: Vec<&'static dyn BlockKind> = BUILTIN_KINDS.to_vec();
        // Stable order for readable error messages and predictable schemas.
        kinds.sort_by_key(|k| k.id());
        Self { kinds }
    }

    /// Look up a kind by its discriminator value.
    pub fn get(&self, id: &str) -> Option<&'static dyn BlockKind> {
        self.kinds.iter().copied().find(|k| k.id() == id)
    }

    /// Every registered block id, in stable order. Used in error messages
    /// (`unknown block X — known: A, B, C`).
    pub fn ids(&self) -> Vec<&'static str> {
        self.kinds.iter().map(|k| k.id()).collect()
    }

    /// Every registered kind, in stable order. Surface for agent installers
    /// that want to embed each block's schema.
    pub fn kinds(&self) -> &[&'static dyn BlockKind] {
        &self.kinds
    }
}

/// Built-in kinds. Adding a new block = one entry here + one `.rs` file +
/// one `docs/blocks/<id>.md` page (locked by the integration test).
const BUILTIN_KINDS: &[&'static dyn BlockKind] = &[
    &db_find_many::Kind,
    &db_find_one::Kind,
    &db_insert::Kind,
    &error::Kind,
    &guard::Kind,
    &time_now::Kind,
];

/// Lazily-built singleton registry. Lookups are O(N) on a tiny N — no need
/// for a hash map and the explicit sort keeps the iteration order stable.
pub fn registry() -> &'static BlockRegistry {
    static REG: OnceLock<BlockRegistry> = OnceLock::new();
    REG.get_or_init(BlockRegistry::new)
}

/// Parse a [`RawBlock`] against the registry: looks up the kind by id,
/// rejects unknown ids with a friendly catalogue, and delegates to the
/// kind's own [`BlockKind::parse`].
pub fn parse(raw: &RawBlock) -> Result<Box<dyn BlockInstance>, ManifestError> {
    let reg = registry();
    let Some(kind) = reg.get(&raw.block) else {
        let known = reg.ids().join(", ");
        return Err(raw.validation_error(format!("unknown block `{}` — known: {known}", raw.block)));
    };
    kind.parse(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fake_path() -> PathBuf {
        PathBuf::from("/fake/route.json")
    }

    #[test]
    fn registry_lists_builtins_in_stable_order() {
        let ids = registry().ids();
        assert_eq!(
            ids,
            vec![
                "db.find_many",
                "db.find_one",
                "db.insert",
                "error",
                "guard",
                "time.now",
            ]
        );
    }

    #[test]
    fn parse_rejects_unknown_block_with_catalogue() {
        let raw = RawBlock {
            block: "db.find_meny".to_string(), // typo
            fields: IndexMap::new(),
            source: fake_path(),
            label: "process[0]".to_string(),
        };
        let err = parse(&raw).unwrap_err();
        assert!(err.message.contains("unknown block `db.find_meny`"));
        assert!(
            err.message.contains("db.find_many"),
            "catalogue must list known blocks: {}",
            err.message
        );
    }

    #[test]
    fn raw_block_from_value_extracts_discriminator() {
        let v: Value = serde_json::from_str(
            r#"{ "block": "db.find_many", "name": "posts", "table": "posts" }"#,
        )
        .unwrap();
        let raw = RawBlock::from_value(&v, &fake_path(), "process[0]").unwrap();
        assert_eq!(raw.block, "db.find_many");
        assert!(raw.fields.contains_key("name"));
        assert!(raw.fields.contains_key("table"));
        assert!(!raw.fields.contains_key("block"));
    }

    #[test]
    fn raw_block_from_value_rejects_missing_block() {
        let v: Value = serde_json::from_str(r#"{ "name": "posts" }"#).unwrap();
        let err = RawBlock::from_value(&v, &fake_path(), "process[0]").unwrap_err();
        assert!(err.message.contains("missing `block`"));
    }

    #[test]
    fn raw_block_from_value_rejects_non_object() {
        let v: Value = serde_json::from_str("[1,2,3]").unwrap();
        let err = RawBlock::from_value(&v, &fake_path(), "process[0]").unwrap_err();
        assert!(err.message.contains("expected an object"));
    }

    #[test]
    fn every_registered_block_has_a_doc_page() {
        // Locks the "each new block ships its own doc page" contract from
        // CLAUDE.md. The presence of `docs/blocks/<id>.md` is the canonical
        // signal that the catalogue stays in sync with the registry.
        let docs_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("docs")
            .join("blocks");
        for kind in registry().kinds() {
            let page = docs_dir.join(format!("{}.md", kind.id()));
            assert!(
                page.is_file(),
                "missing doc page for block `{}` — expected at `{}`",
                kind.id(),
                page.display()
            );
        }
    }
}
