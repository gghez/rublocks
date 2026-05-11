//! Structured form of `db.find_*.where` and the small grammar of
//! per-column operators it accepts.
//!
//! Three accepted shapes inside a route file:
//!
//! 1. A bare CEL string — already covered by [`crate::sql_where`].
//! 2. A flat object map of `column -> value-or-operator`:
//!
//!    ```json
//!    "where": { "slug": "$input.path.slug", "published_at": { "is_not_null": true } }
//!    ```
//!
//! 3. Mixed: any value at column-level may be a literal, a `$ref`, or one
//!    of the operator objects below.
//!
//! Operator objects on the right-hand side:
//!
//! - `{ "is_null": true }` / `{ "is_not_null": true }` — null-aware
//! - `{ "eq": <v> }`, `{ "ne": <v> }`, `{ "lt": <v> }`, `{ "le": <v> }`,
//!   `{ "gt": <v> }`, `{ "ge": <v> }` — comparisons
//! - `{ "in": [<v>, ...] }` — membership (each element a `$ref` or literal)
//!
//! A literal RHS is sugar for `{ "eq": <v> }`. Multiple top-level keys
//! are joined with AND.

use serde_json::Value;

use crate::value_ref::ValueRef;

/// Parsed `where:` for one block. Either string-form CEL (already
/// handled by [`crate::sql_where::compile`]) or the structured form
/// captured below.
#[derive(Debug, Clone)]
pub enum WhereSpec {
    Cel(String),
    Structured(Vec<ColumnClause>),
}

/// One column-scoped predicate of a structured `where:`. Multiple clauses
/// are AND-joined at SQL emission time.
#[derive(Debug, Clone)]
pub struct ColumnClause {
    pub column: String,
    pub op: WhereOp,
}

/// One operator applied to a column.
#[derive(Debug, Clone)]
pub enum WhereOp {
    IsNull,
    IsNotNull,
    Eq(ValueRef),
    Ne(ValueRef),
    Lt(ValueRef),
    Le(ValueRef),
    Gt(ValueRef),
    Ge(ValueRef),
    In(Vec<ValueRef>),
}

impl WhereSpec {
    /// Parse a raw `where:` JSON value into a typed [`WhereSpec`].
    ///
    /// Each path returned in errors is prefixed by `label` so a manifest
    /// error can locate the offending block file + JSON pointer.
    pub fn parse(value: &Value, label: &str) -> Result<Self, String> {
        match value {
            Value::String(s) => Ok(Self::Cel(s.clone())),
            Value::Object(_) => parse_structured(value, label).map(Self::Structured),
            other => Err(format!(
                "{label}: `where:` must be a CEL string or an object map (got {other:?})"
            )),
        }
    }

    /// Every value-ref reachable from this predicate. The build-time
    /// scope check uses this to make sure no `$<name>` resolves to an
    /// unbound block.
    pub fn refs(&self) -> Vec<&ValueRef> {
        let mut out = Vec::new();
        if let Self::Structured(clauses) = self {
            for c in clauses {
                match &c.op {
                    WhereOp::Eq(v)
                    | WhereOp::Ne(v)
                    | WhereOp::Lt(v)
                    | WhereOp::Le(v)
                    | WhereOp::Gt(v)
                    | WhereOp::Ge(v) => out.push(v),
                    WhereOp::In(list) => out.extend(list.iter()),
                    WhereOp::IsNull | WhereOp::IsNotNull => {}
                }
            }
        }
        out
    }
}

fn parse_structured(value: &Value, label: &str) -> Result<Vec<ColumnClause>, String> {
    let Value::Object(map) = value else {
        return Err(format!("{label}: expected object"));
    };
    let mut clauses = Vec::with_capacity(map.len());
    for (col, rhs) in map {
        let op = parse_op(rhs, &format!("{label}.{col}"))?;
        clauses.push(ColumnClause {
            column: col.clone(),
            op,
        });
    }
    Ok(clauses)
}

fn parse_op(value: &Value, label: &str) -> Result<WhereOp, String> {
    if let Value::Object(map) = value {
        return parse_op_object(map, label);
    }
    // Bare RHS — sugar for `eq`.
    Ok(WhereOp::Eq(parse_ref(value, label)?))
}

fn parse_op_object(map: &serde_json::Map<String, Value>, label: &str) -> Result<WhereOp, String> {
    if map.len() != 1 {
        return Err(format!(
            "{label}: operator object must hold exactly one key (got {})",
            map.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    let (op, val) = map.iter().next().expect("len == 1");
    match op.as_str() {
        "is_null" => expect_bool_true(val, label, "is_null").map(|_| WhereOp::IsNull),
        "is_not_null" => expect_bool_true(val, label, "is_not_null").map(|_| WhereOp::IsNotNull),
        "eq" => parse_ref(val, label).map(WhereOp::Eq),
        "ne" => parse_ref(val, label).map(WhereOp::Ne),
        "lt" => parse_ref(val, label).map(WhereOp::Lt),
        "le" => parse_ref(val, label).map(WhereOp::Le),
        "gt" => parse_ref(val, label).map(WhereOp::Gt),
        "ge" => parse_ref(val, label).map(WhereOp::Ge),
        "in" => {
            let Value::Array(items) = val else {
                return Err(format!("{label}.in: expected an array of values"));
            };
            let mut refs = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                refs.push(parse_ref(item, &format!("{label}.in[{i}]"))?);
            }
            Ok(WhereOp::In(refs))
        }
        other => Err(format!(
            "{label}: unknown operator `{other}` — supported: is_null, is_not_null, eq, ne, lt, le, gt, ge, in"
        )),
    }
}

fn expect_bool_true(value: &Value, label: &str, op: &str) -> Result<(), String> {
    if value == &Value::Bool(true) {
        Ok(())
    } else {
        Err(format!("{label}.{op}: expected `true` (got {value:?})"))
    }
}

fn parse_ref(value: &Value, label: &str) -> Result<ValueRef, String> {
    ValueRef::parse(value).map_err(|e| format!("{label}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_string_form_as_cel() {
        let s = WhereSpec::parse(&json!("id == 1"), "where").unwrap();
        assert!(matches!(s, WhereSpec::Cel(_)));
    }

    #[test]
    fn parses_structured_with_bare_eq() {
        let s = WhereSpec::parse(&json!({ "slug": "hello" }), "where").unwrap();
        match s {
            WhereSpec::Structured(c) => {
                assert_eq!(c.len(), 1);
                assert_eq!(c[0].column, "slug");
                assert!(matches!(c[0].op, WhereOp::Eq(_)));
            }
            other => panic!("expected Structured, got {other:?}"),
        }
    }

    #[test]
    fn parses_is_not_null_operator() {
        let s =
            WhereSpec::parse(&json!({ "published_at": { "is_not_null": true } }), "where").unwrap();
        let WhereSpec::Structured(c) = s else {
            panic!();
        };
        assert!(matches!(c[0].op, WhereOp::IsNotNull));
    }

    #[test]
    fn parses_in_operator_with_literals() {
        let s = WhereSpec::parse(&json!({ "id": { "in": [1, 2, 3] } }), "where").unwrap();
        let WhereSpec::Structured(c) = s else {
            panic!();
        };
        match &c[0].op {
            WhereOp::In(refs) => assert_eq!(refs.len(), 3),
            other => panic!("expected In, got {other:?}"),
        }
    }

    #[test]
    fn parses_ref_on_rhs() {
        let s = WhereSpec::parse(&json!({ "slug": "$input.path.slug" }), "where").unwrap();
        let WhereSpec::Structured(c) = s else {
            panic!();
        };
        match &c[0].op {
            WhereOp::Eq(ValueRef::Input { .. }) => {}
            other => panic!("expected eq(Input), got {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_operator() {
        let err = WhereSpec::parse(&json!({ "x": { "between": [1, 2] } }), "where").unwrap_err();
        assert!(err.contains("unknown operator"), "{err}");
    }

    #[test]
    fn rejects_multi_key_operator_object() {
        let err = WhereSpec::parse(&json!({ "x": { "eq": 1, "ne": 2 } }), "where").unwrap_err();
        assert!(err.contains("exactly one key"), "{err}");
    }
}
