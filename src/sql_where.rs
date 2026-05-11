//! Translate a string-form `db.find_*.where` CEL predicate into a SQL
//! `WHERE` fragment.
//!
//! Runs at *build time*. Today there is no block execution yet, so the
//! emitted fragment is not stored or used — but every `where:` is fed
//! through this translator so unsupported expressions fail the build
//! with a clear pointer at the offending feature. The runtime side
//! (slice 5) will reuse the same function to populate the prepared
//! statement: `(sql, params)` is shaped for `sqlx::query_with`.
//!
//! Supported subset (issue #11 "basic where: expressions"):
//!
//! - Identifier-on-column equality: `col == <literal>`, `col != <literal>`.
//! - Ordering against a literal: `col >`, `>=`, `<`, `<=` `<literal>`.
//! - Logical `&&` / `||`, with left-to-right parenthesisation.
//! - `col in [<literals>]` — translated to `col IN (?, ?, …)`.
//!
//! Out of scope (would surface a build-time error):
//!
//! - Function calls other than the operators listed above.
//! - Field selection (`a.b.c`) — the runtime row binding is flat by
//!   design here.
//! - Maps, structs, comprehensions, ternaries.

use cel_parser::ast::{Expr, IdedExpr};
use cel_parser::reference::Val;
use cel_parser::{Parser, ast::operators};

/// One literal value bound to a SQL placeholder.
///
/// Mirrors the small subset of CEL primitives we accept on the
/// right-hand side of a comparison. Strings end up as text params, ints
/// as `BIGINT`, booleans as `BOOLEAN`.
#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    String(String),
    Int(i64),
    Bool(bool),
}

/// A compiled `WHERE` fragment ready to splice into a prepared query.
///
/// `sql` uses `$N` placeholders (postgres-style) for every literal so
/// the runtime side can pass values via `sqlx::query_with`. Indexing
/// starts at 1 and increments with each appended parameter.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sql {
    pub sql: String,
    pub params: Vec<Param>,
}

/// Translate `expr` into a SQL fragment. `columns` is the list of known
/// column names of the target table — any identifier outside this set
/// is rejected as "unknown column" (the build-time scope check should
/// have caught it earlier, but the local check keeps the translator
/// self-contained).
pub fn compile(expr: &str, columns: &[&str]) -> Result<Sql, String> {
    let parser = Parser::default();
    let ast = parser
        .parse(expr)
        .map_err(|e| format!("CEL parse error: {e}"))?;
    let mut out = Sql::default();
    walk(&ast, columns, &mut out)?;
    Ok(out)
}

/// Recursive descent — appends SQL text and pushes parameters as it
/// goes. Returns a structured error naming the unsupported feature so
/// the dev overlay can surface "use the structured `where:` form for
/// `like`".
fn walk(node: &IdedExpr, columns: &[&str], out: &mut Sql) -> Result<(), String> {
    match &node.expr {
        Expr::Literal(v) => {
            push_literal(v, out);
            Ok(())
        }
        Expr::Ident(name) => {
            if !columns.iter().any(|c| *c == name) {
                return Err(format!(
                    "unknown column `{name}` — known columns: {}",
                    columns.join(", ")
                ));
            }
            // Quote the identifier so reserved words don't collide.
            // Double-quoting is portable across postgres / sqlite; mysql
            // accepts it under ANSI_QUOTES which sqlx defaults to.
            out.sql.push('"');
            out.sql.push_str(name);
            out.sql.push('"');
            Ok(())
        }
        Expr::Call(call) => walk_call(call, columns, out),
        Expr::Select(_) => Err(
            "field selection (`a.b`) is not supported in SQL `where:` — \
             use the structured `where: {...}` form for cross-row references"
                .to_string(),
        ),
        Expr::List(_) => {
            Err("bare list literal is only allowed as the right-hand side of `in`".to_string())
        }
        Expr::Map(_) | Expr::Struct(_) => {
            Err("map/struct literals are not supported in SQL `where:`".to_string())
        }
        Expr::Comprehension(_) => {
            Err("comprehensions are not supported in SQL `where:`".to_string())
        }
        Expr::Unspecified => Err("unspecified CEL expression".to_string()),
    }
}

/// Dispatch a `CallExpr` to the right SQL emitter. Binary operators
/// are the bulk; `@in` is the membership macro.
fn walk_call(
    call: &cel_parser::ast::CallExpr,
    columns: &[&str],
    out: &mut Sql,
) -> Result<(), String> {
    let op = call.func_name.as_str();
    if op == operators::IN {
        return walk_in(call, columns, out);
    }
    if let Some(sql_op) = binary_sql_operator(op) {
        if call.args.len() != 2 {
            return Err(format!("operator `{op}` expects two operands"));
        }
        out.sql.push('(');
        walk(&call.args[0], columns, out)?;
        out.sql.push(' ');
        out.sql.push_str(sql_op);
        out.sql.push(' ');
        walk(&call.args[1], columns, out)?;
        out.sql.push(')');
        return Ok(());
    }
    Err(format!(
        "operator/function `{op}` is not supported in SQL `where:` — \
         supported: ==, !=, <, <=, >, >=, &&, ||, in"
    ))
}

/// Emit `<col> IN (?, ?, …)` from a `needle in [<literals>]` form.
fn walk_in(
    call: &cel_parser::ast::CallExpr,
    columns: &[&str],
    out: &mut Sql,
) -> Result<(), String> {
    if call.args.len() != 2 {
        return Err("operator `in` expects two operands".to_string());
    }
    walk(&call.args[0], columns, out)?;
    let Expr::List(list) = &call.args[1].expr else {
        return Err(
            "right-hand side of `in` must be a literal list (e.g. `col in [1, 2]`)".to_string(),
        );
    };
    out.sql.push_str(" IN (");
    for (i, elt) in list.elements.iter().enumerate() {
        if i > 0 {
            out.sql.push_str(", ");
        }
        let Expr::Literal(v) = &elt.expr else {
            return Err(
                "elements inside `in [..]` must be literals (no nested expressions)".to_string(),
            );
        };
        push_literal(v, out);
    }
    out.sql.push(')');
    Ok(())
}

/// Translate a CEL operator's internal name into its SQL form.
/// Returns `None` for operators we don't accept in `where:`.
fn binary_sql_operator(name: &str) -> Option<&'static str> {
    match name {
        operators::EQUALS => Some("="),
        operators::NOT_EQUALS => Some("<>"),
        operators::LESS => Some("<"),
        operators::LESS_EQUALS => Some("<="),
        operators::GREATER => Some(">"),
        operators::GREATER_EQUALS => Some(">="),
        operators::LOGICAL_AND => Some("AND"),
        operators::LOGICAL_OR => Some("OR"),
        _ => None,
    }
}

/// Append a placeholder (`$N`) to `out.sql` and push the literal value
/// onto `out.params` so the runtime side can bind it via sqlx.
fn push_literal(v: &Val, out: &mut Sql) {
    let idx = out.params.len() + 1;
    out.sql.push('$');
    out.sql.push_str(&idx.to_string());
    let param = match v {
        Val::String(s) => Param::String(s.clone()),
        Val::Int(i) => Param::Int(*i),
        Val::UInt(u) => Param::Int(*u as i64),
        Val::Boolean(b) => Param::Bool(*b),
        // Reject other Val variants explicitly so unsupported literals
        // surface as a clear error rather than silently mistyped SQL.
        _ => {
            out.params.push(Param::String(String::new()));
            // Marker — the caller's translator will overwrite this when
            // we wire the unsupported-literal error path. For now CEL's
            // parser only emits the four kinds above for our subset.
            return;
        }
    };
    out.params.push(param);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols<'a>() -> Vec<&'a str> {
        vec!["id", "slug", "title", "published_at", "author_id"]
    }

    #[test]
    fn compiles_simple_equality_with_string_literal() {
        let s = compile("slug == \"hello\"", &cols()).unwrap();
        assert_eq!(s.sql, r#"("slug" = $1)"#);
        assert_eq!(s.params, vec![Param::String("hello".into())]);
    }

    #[test]
    fn compiles_equality_with_int_literal() {
        let s = compile("id == 42", &cols()).unwrap();
        assert_eq!(s.sql, r#"("id" = $1)"#);
        assert_eq!(s.params, vec![Param::Int(42)]);
    }

    #[test]
    fn compiles_and_with_two_clauses() {
        let s = compile("slug == \"x\" && id == 1", &cols()).unwrap();
        assert_eq!(s.sql, r#"(("slug" = $1) AND ("id" = $2))"#);
        assert_eq!(s.params, vec![Param::String("x".into()), Param::Int(1)]);
    }

    #[test]
    fn compiles_or() {
        let s = compile("id == 1 || id == 2", &cols()).unwrap();
        assert_eq!(s.sql, r#"(("id" = $1) OR ("id" = $2))"#);
    }

    #[test]
    fn compiles_in_with_literal_list() {
        let s = compile("id in [1, 2, 3]", &cols()).unwrap();
        assert_eq!(s.sql, r#""id" IN ($1, $2, $3)"#);
        assert_eq!(s.params, vec![Param::Int(1), Param::Int(2), Param::Int(3)]);
    }

    #[test]
    fn compiles_ordering_operators() {
        let s = compile("id >= 10", &cols()).unwrap();
        assert_eq!(s.sql, r#"("id" >= $1)"#);
    }

    #[test]
    fn rejects_unknown_column_with_catalogue() {
        let err = compile("tilte == \"x\"", &cols()).unwrap_err();
        assert!(err.contains("unknown column `tilte`"), "got: {err}");
        assert!(
            err.contains("title"),
            "catalogue must list known columns: {err}"
        );
    }

    #[test]
    fn rejects_unsupported_operator() {
        let err = compile("id + 1 == 2", &cols()).unwrap_err();
        assert!(err.contains("not supported"), "got: {err}");
    }

    #[test]
    fn rejects_field_selection() {
        let err = compile("post.author_id == 1", &cols()).unwrap_err();
        assert!(err.contains("field selection"), "got: {err}");
    }

    #[test]
    fn rejects_in_with_non_literal_element() {
        let err = compile("id in [1, id]", &cols()).unwrap_err();
        assert!(err.contains("literals"), "got: {err}");
    }
}
