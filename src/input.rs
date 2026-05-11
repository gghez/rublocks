//! Typed `route.input` specification + load-time validation of every
//! per-field constraint.
//!
//! The route's `input` section describes the JSON fields the handler
//! receives (path / query / body). Because the spec is *typed*, the
//! validator that runs at request time is **derived automatically** from
//! the declaration — there is no separate `validate.input` block to
//! sprinkle into `process`. The block layer downstream consumes
//! `$input.X.X` references already validated.
//!
//! What we validate at load time:
//!
//! - Every `type` is one of the supported [`FieldKind`] values.
//! - Every `default` is type-compatible with its declared field kind.
//! - Every `pattern` is a valid Rust `regex` — caught now so the user
//!   does not have to rebuild the dist crate to discover the typo.
//! - Every `validate` CEL string parses (same syntactic check as the
//!   `guard` block's `if` and `field.validate` in models).
//! - `min`/`max` only on numeric kinds, `min_length`/`max_length`/`pattern`
//!   only on string-shaped kinds.
//!
//! What we do NOT do here:
//!
//! - Emit Rust code for the auto-validator. That lives in `codegen.rs`,
//!   which queries this module's parsed types to know what to generate.
//! - Decide between `Json<T>` and `Form<T>` extractors. The decision sits
//!   in `BodySpec::form` and is consumed by codegen.
//!
//! See `docs/input.md`.

use indexmap::IndexMap;
use regex::Regex;
use schemars::{JsonSchema, schema::RootSchema, schema_for};
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;

use crate::expressions;
use crate::manifest::ManifestError;

/// Logical field types accepted in `input.*`. Mirrors the model field
/// types (so an `email` validated at input is the same shape as an
/// `email` column), with the addition that scalar Rust types govern
/// extraction here, not SQL types.
#[derive(Debug, Deserialize, JsonSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FieldKind {
    String,
    Text,
    Int,
    Bigint,
    Bool,
    Uuid,
    Email,
    Timestamptz,
}

impl FieldKind {
    /// True for kinds whose runtime Rust type is `String` (and therefore
    /// accept `min_length` / `max_length` / `pattern`).
    pub fn is_string_shaped(self) -> bool {
        matches!(self, Self::String | Self::Text | Self::Email)
    }

    /// True for numeric kinds that accept `min` / `max`.
    pub fn is_numeric(self) -> bool {
        matches!(self, Self::Int | Self::Bigint)
    }
}

/// A compiled regex pattern. The source string is kept so codegen can
/// embed it verbatim into the dist crate; the compiled form is used
/// at load time to reject syntactically invalid patterns.
#[derive(Debug, Clone)]
pub struct Pattern {
    pub source: String,
}

impl Pattern {
    fn compile(source: &str) -> Result<Self, regex::Error> {
        Regex::new(source).map(|_| Pattern {
            source: source.to_string(),
        })
    }
}

/// One declared input field, fully resolved and validated.
#[derive(Debug, Clone)]
pub struct FieldSpec {
    pub ty: FieldKind,
    pub required: bool,
    pub default: Option<Value>,
    pub min: Option<i64>,
    pub max: Option<i64>,
    pub min_length: Option<u32>,
    pub max_length: Option<u32>,
    pub pattern: Option<Pattern>,
    /// CEL expression. The runtime evaluator binds the field's parsed value
    /// to the field name (e.g. `title.size() > 3`).
    pub validate: Option<String>,
}

/// Body section of the input spec — either implicit JSON (a flat map of
/// fields) or an explicit `{ form: bool, fields: {...} }` wrapper.
#[derive(Debug, Clone)]
pub struct BodySpec {
    /// `true` ⇒ `application/x-www-form-urlencoded` (`Form<T>`).
    /// `false` ⇒ `application/json` (`Json<T>`).
    pub form: bool,
    pub fields: IndexMap<String, FieldSpec>,
}

/// Top-level shape of `route.input`. All three sections are optional —
/// a missing section means "no input extracted from this Axum source".
#[derive(Debug, Default, Clone)]
pub struct InputSpec {
    pub path: IndexMap<String, FieldSpec>,
    pub query: IndexMap<String, FieldSpec>,
    pub body: Option<BodySpec>,
}

impl InputSpec {
    /// `true` when the spec carries no fields at all — codegen skips
    /// emitting extractors and the validation chain in that case.
    pub fn is_empty(&self) -> bool {
        self.path.is_empty() && self.query.is_empty() && self.body.is_none()
    }

    /// Parse a raw `Value` against the input shape. Strict: unknown
    /// sections or unknown per-field knobs are rejected with a path
    /// that pinpoints the offending key (e.g. `input.query.limit.maxx`).
    pub fn parse(value: &Value, file: &Path) -> Result<Self, ManifestError> {
        let raw: RawInput = serde_json::from_value(value.clone()).map_err(|e| {
            ManifestError::validation(file, format!("input: {e}"))
        })?;
        let mut out = InputSpec::default();
        if let Some(map) = raw.path {
            for (name, raw_field) in map {
                let f = resolve_field(&raw_field, file, &format!("input.path.{name}"))?;
                out.path.insert(name, f);
            }
        }
        if let Some(map) = raw.query {
            for (name, raw_field) in map {
                let f = resolve_field(&raw_field, file, &format!("input.query.{name}"))?;
                out.query.insert(name, f);
            }
        }
        if let Some(raw_body) = raw.body {
            let body = resolve_body(&raw_body, file)?;
            out.body = Some(body);
        }
        Ok(out)
    }
}

// -- raw deserialization shapes --------------------------------------------

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "input spec")]
struct RawInput {
    #[serde(default)]
    path: Option<IndexMap<String, RawField>>,
    #[serde(default)]
    query: Option<IndexMap<String, RawField>>,
    #[serde(default)]
    body: Option<RawBody>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RawField {
    #[serde(rename = "type")]
    ty: FieldKind,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    default: Option<Value>,
    #[serde(default)]
    min: Option<i64>,
    #[serde(default)]
    max: Option<i64>,
    #[serde(default)]
    min_length: Option<u32>,
    #[serde(default)]
    max_length: Option<u32>,
    #[serde(default)]
    pattern: Option<String>,
    #[serde(default)]
    validate: Option<String>,
}

/// Either the implicit flat-map form or the `{ form, fields }` wrapper.
/// `untagged` lets serde pick the variant by structural match.
#[derive(Deserialize, JsonSchema)]
#[serde(untagged)]
enum RawBody {
    Wrapped {
        #[serde(default)]
        form: bool,
        fields: IndexMap<String, RawField>,
    },
    Flat(IndexMap<String, RawField>),
}

fn resolve_body(raw: &RawBody, file: &Path) -> Result<BodySpec, ManifestError> {
    let (form, fields) = match raw {
        RawBody::Wrapped { form, fields } => (*form, fields),
        RawBody::Flat(fields) => (false, fields),
    };
    let mut out = IndexMap::with_capacity(fields.len());
    for (name, raw_field) in fields {
        let f = resolve_field(raw_field, file, &format!("input.body.{name}"))?;
        out.insert(name.clone(), f);
    }
    Ok(BodySpec { form, fields: out })
}

fn resolve_field(
    raw: &RawField,
    file: &Path,
    label: &str,
) -> Result<FieldSpec, ManifestError> {
    // Numeric-only knobs.
    if (raw.min.is_some() || raw.max.is_some()) && !raw.ty.is_numeric() {
        return Err(ManifestError::validation(
            file,
            format!("{label}: `min` / `max` only apply to numeric kinds (`int`, `bigint`)"),
        ));
    }
    // String-only knobs.
    let has_string_knob =
        raw.min_length.is_some() || raw.max_length.is_some() || raw.pattern.is_some();
    if has_string_knob && !raw.ty.is_string_shaped() {
        return Err(ManifestError::validation(
            file,
            format!(
                "{label}: `min_length` / `max_length` / `pattern` only apply to string-shaped kinds (`string`, `text`, `email`)"
            ),
        ));
    }
    // Compile the regex at load time — typos surface immediately, before
    // the dist crate compiles.
    let pattern = match raw.pattern.as_deref() {
        Some(src) => Some(Pattern::compile(src).map_err(|e| {
            ManifestError::validation(file, format!("{label}.pattern: invalid regex: {e}"))
        })?),
        None => None,
    };
    // Validate the default's type matches the declared kind so the dist
    // crate cannot end up with a `default = "twenty"` for an `int`.
    if let Some(d) = raw.default.as_ref() {
        check_default(d, raw.ty, file, label)?;
    }
    // CEL validator — same syntactic check as `field.validate` in models.
    if let Some(expr) = raw.validate.as_deref() {
        expressions::validate(expr, file, &format!("{label}.validate"))?;
    }
    Ok(FieldSpec {
        ty: raw.ty,
        required: raw.required,
        default: raw.default.clone(),
        min: raw.min,
        max: raw.max,
        min_length: raw.min_length,
        max_length: raw.max_length,
        pattern,
        validate: raw.validate.clone(),
    })
}

fn check_default(
    value: &Value,
    kind: FieldKind,
    file: &Path,
    label: &str,
) -> Result<(), ManifestError> {
    let ok = match kind {
        FieldKind::String | FieldKind::Text | FieldKind::Email | FieldKind::Uuid
        | FieldKind::Timestamptz => value.is_string(),
        FieldKind::Int | FieldKind::Bigint => value.is_i64() || value.is_u64(),
        FieldKind::Bool => value.is_boolean(),
    };
    if !ok {
        return Err(ManifestError::validation(
            file,
            format!(
                "{label}.default: value type does not match declared field type `{kind:?}`"
            ),
        ));
    }
    Ok(())
}

/// JSON Schema describing the on-disk shape of `route.input`. Exposed for
/// the agent installers so each project's `AGENTS.md` carries the input
/// contract alongside the route + block schemas.
pub fn json_schema() -> RootSchema {
    schema_for!(RawInput)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fake() -> PathBuf {
        PathBuf::from("/fake/route.json")
    }

    fn parse(json: &str) -> Result<InputSpec, ManifestError> {
        let v: Value = serde_json::from_str(json).unwrap();
        InputSpec::parse(&v, &fake())
    }

    #[test]
    fn parse_returns_empty_for_empty_object() {
        let s = parse("{}").unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn parse_typed_query_field() {
        let s = parse(
            r#"{ "query": { "limit": { "type": "int", "default": 20, "max": 100 } } }"#,
        )
        .unwrap();
        let f = s.query.get("limit").unwrap();
        assert_eq!(f.ty, FieldKind::Int);
        assert_eq!(f.max, Some(100));
        assert_eq!(f.default, Some(Value::from(20)));
    }

    #[test]
    fn parse_flat_body() {
        let s = parse(
            r#"{ "body": { "title": { "type": "string", "required": true } } }"#,
        )
        .unwrap();
        let body = s.body.as_ref().unwrap();
        assert!(!body.form);
        assert!(body.fields["title"].required);
    }

    #[test]
    fn parse_wrapped_form_body() {
        let s = parse(
            r#"{ "body": { "form": true, "fields": { "name": { "type": "string", "required": true } } } }"#,
        )
        .unwrap();
        let body = s.body.as_ref().unwrap();
        assert!(body.form);
        assert!(body.fields["name"].required);
    }

    #[test]
    fn rejects_unknown_section() {
        let err = parse(r#"{ "headers": {} }"#).unwrap_err();
        assert!(err.message.contains("headers"), "got: {}", err.message);
    }

    #[test]
    fn rejects_unknown_field_knob() {
        let err = parse(
            r#"{ "query": { "limit": { "type": "int", "maxx": 10 } } }"#,
        )
        .unwrap_err();
        assert!(err.message.contains("maxx"), "got: {}", err.message);
    }

    #[test]
    fn rejects_min_on_string_field() {
        let err = parse(
            r#"{ "query": { "q": { "type": "string", "min": 1 } } }"#,
        )
        .unwrap_err();
        assert!(err.message.contains("numeric"), "got: {}", err.message);
    }

    #[test]
    fn rejects_max_length_on_int_field() {
        let err = parse(
            r#"{ "query": { "n": { "type": "int", "max_length": 5 } } }"#,
        )
        .unwrap_err();
        assert!(
            err.message.contains("string-shaped"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_invalid_regex_at_load_time() {
        let err = parse(
            r#"{ "path": { "slug": { "type": "string", "pattern": "[a-z" } } }"#,
        )
        .unwrap_err();
        assert!(
            err.message.contains("input.path.slug.pattern")
                && err.message.contains("invalid regex"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_invalid_cel_validate_at_load_time() {
        let err = parse(
            r#"{ "body": { "title": { "type": "string", "validate": "title.size() >=" } } }"#,
        )
        .unwrap_err();
        assert!(
            err.message.contains("input.body.title.validate")
                && err.message.contains("invalid CEL"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn rejects_default_with_wrong_type() {
        let err = parse(
            r#"{ "query": { "limit": { "type": "int", "default": "twenty" } } }"#,
        )
        .unwrap_err();
        assert!(err.message.contains("default"), "got: {}", err.message);
    }

    #[test]
    fn accepts_well_formed_regex_and_cel() {
        let s = parse(
            r#"{
                "body": {
                    "title": {
                        "type": "string",
                        "min_length": 3,
                        "max_length": 200,
                        "pattern": "^[A-Za-z0-9 ]+$",
                        "validate": "title.size() > 3"
                    }
                }
            }"#,
        )
        .unwrap();
        let f = &s.body.unwrap().fields["title"];
        assert_eq!(f.min_length, Some(3));
        assert_eq!(f.max_length, Some(200));
        assert_eq!(
            f.pattern.as_ref().map(|p| p.source.as_str()),
            Some("^[A-Za-z0-9 ]+$")
        );
        assert_eq!(f.validate.as_deref(), Some("title.size() > 3"));
    }
}
