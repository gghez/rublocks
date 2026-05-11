//! Validation of CEL (Common Expression Language) snippets declared in
//! manifest JSON.
//!
//! Every CEL expression accepted by rublocks (the `guard` block's `if`,
//! `field.validate`, `process.<block>.where`, view conditionals…) is
//! parsed at build time via `cel-interpreter::Program::compile`. Bad
//! syntax becomes a `ManifestError` with the offending file path so the
//! dev overlay can point the user straight at the place to fix.
//!
//! Runtime evaluation (against a typed `user` / row / request context)
//! lands once process-block execution (slice 5) is wired — see issue #11
//! for the open questions on that side.

use cel_interpreter::Program;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use crate::manifest::ManifestError;
use crate::models::Model;
use crate::routes::Route;

/// True when the project declares at least one CEL site whose program
/// the generated crate must evaluate at runtime — drives the conditional
/// emission of `cel-interpreter` in `render_cargo_toml`.
///
/// Per-site detail lives where it's declared: each `BlockInstance`
/// reports whether it embeds runtime CEL (e.g. `guard`, the string form
/// of `db.find_*.where`), and the `validate` fields on inputs and model
/// columns are inspected directly here.
pub fn project_uses_cel(routes: &[Route], models: &[Model]) -> bool {
    if models
        .iter()
        .any(|m| m.fields.values().any(|f| f.validate.is_some()))
    {
        return true;
    }
    for r in routes {
        if let Some(input) = r.input.as_ref() {
            let any_validate = |m: &indexmap::IndexMap<String, crate::input::FieldSpec>| {
                m.values().any(|f| f.validate.is_some())
            };
            if any_validate(&input.path)
                || any_validate(&input.query)
                || input.body.as_ref().is_some_and(|b| any_validate(&b.fields))
            {
                return true;
            }
        }
        if r.process.iter().any(|b| b.embeds_runtime_cel()) {
            return true;
        }
    }
    false
}

/// Compile `expr` as CEL. Returns `Ok` only when the source parses;
/// callers that also need to enforce a scope should call
/// [`validate_with_scope`] instead.
///
/// `cel-interpreter 0.10` can panic on certain malformed inputs (the
/// underlying antlr-generated grammar reaches an `unreachable!()`), so
/// the compile call is wrapped in `catch_unwind`. Surfacing a panic as a
/// structured manifest error is strictly better than crashing the build.
pub fn validate(expr: &str, source: &Path, label: &str) -> Result<(), ManifestError> {
    compile(expr, source, label).map(|_| ())
}

/// Compile `expr` and additionally reject any identifier that is not in
/// `allowed`. Built-in CEL stdlib functions (`size`, `length`, `has`,
/// `matches`, type coercions…) live in the function namespace and are
/// not affected by this check; only top-level variable references are.
///
/// The error message lists the offending names so authors can fix typos
/// without running the dist crate.
pub fn validate_with_scope(
    expr: &str,
    allowed: &[&str],
    source: &Path,
    label: &str,
) -> Result<(), ManifestError> {
    let program = compile(expr, source, label)?;
    let refs = program.references();
    let mut undeclared: Vec<&str> = refs
        .variables()
        .into_iter()
        .filter(|v| !allowed.contains(v))
        .collect();
    if undeclared.is_empty() {
        return Ok(());
    }
    undeclared.sort();
    undeclared.dedup();
    let names = undeclared.join(", ");
    let scope = if allowed.is_empty() {
        "(empty)".to_string()
    } else {
        allowed.join(", ")
    };
    Err(ManifestError::validation(
        source,
        format!("unknown identifier(s) in {label}: {names} — in scope: {scope}"),
    ))
}

/// Post-load scope analysis over every route's `process`.
///
/// Walks each block in order. Maintains a set of in-scope names that
/// grows as each named block executes. At each CEL site, the expression
/// is re-validated against the local scope:
///
/// - `guard.if` — scope = route input top-level names ∪ prior `$<name>`.
/// - `db.find_*.where` (string form) — scope = the target table's
///   columns. The target model must exist; this is a separate error
///   (already validated at load time? No — only the column-list scope
///   is enforced here, and a missing table is reported as such).
///
/// Identifier collisions across input sections (path/query/body) are
/// detected here too: the field name becomes ambiguous at the CEL level
/// otherwise, so we surface the conflict at build time.
pub fn scope_check_routes(routes: &[Route], models: &[Model]) -> Result<(), ManifestError> {
    for r in routes {
        let mut scope: Vec<String> = Vec::new();
        if let Some(input) = r.input.as_ref() {
            collect_input_names(input, &r.source, &mut scope)?;
        }
        for (idx, block) in r.process.iter().enumerate() {
            let label = format!("process[{idx}]");
            if let Some(expr) = block.guard_if() {
                let allowed: Vec<&str> = scope.iter().map(String::as_str).collect();
                validate_with_scope(expr, &allowed, &r.source, &format!("{label}.if"))?;
            }
            if let Some(expr) = block.where_predicate() {
                let table = block.target_table().unwrap_or("");
                let model = models.iter().find(|m| m.table == table).ok_or_else(|| {
                    ManifestError::validation(
                        &r.source,
                        format!(
                            "{label}.where references table `{table}` but no model declares it"
                        ),
                    )
                })?;
                let allowed: Vec<&str> = model.fields.keys().map(String::as_str).collect();
                validate_with_scope(expr, &allowed, &r.source, &format!("{label}.where"))?;
                // The translator runs at build time so the user learns
                // about an unsupported feature (a `like`, a function
                // call) immediately, not when slice 5 wires execution.
                // The fragment itself is discarded — execution will
                // recompile when it runs the prepared statement.
                crate::sql_where::compile(expr, &allowed).map_err(|e| {
                    ManifestError::validation(&r.source, format!("{label}.where: {e}"))
                })?;
            }
            if let Some(name) = block.name() {
                scope.push(name.to_string());
            }
        }
    }
    Ok(())
}

/// Flatten the three input sections into a single top-level CEL scope.
/// Collisions across sections are rejected: a CEL expression cannot
/// disambiguate `path.slug` from `body.slug` since both bind as `slug`.
fn collect_input_names(
    input: &crate::input::InputSpec,
    source: &Path,
    scope: &mut Vec<String>,
) -> Result<(), ManifestError> {
    let mut push = |name: &str, section: &str| -> Result<(), ManifestError> {
        if scope.iter().any(|s| s == name) {
            return Err(ManifestError::validation(
                source,
                format!(
                    "input field `{name}` (in `{section}`) collides with another input section — \
                     pick distinct field names so CEL guards can reference them unambiguously"
                ),
            ));
        }
        scope.push(name.to_string());
        Ok(())
    };
    for name in input.path.keys() {
        push(name, "path")?;
    }
    for name in input.query.keys() {
        push(name, "query")?;
    }
    if let Some(body) = input.body.as_ref() {
        for name in body.fields.keys() {
            push(name, "body")?;
        }
    }
    Ok(())
}

/// Compile and return the `Program`, wrapping panics into a manifest
/// error. Internal helper for [`validate`] and [`validate_with_scope`].
fn compile(expr: &str, source: &Path, label: &str) -> Result<Program, ManifestError> {
    if expr.trim().is_empty() {
        return Err(ManifestError::validation(
            source,
            format!("{label} CEL expression must not be empty"),
        ));
    }
    let attempt = catch_unwind(AssertUnwindSafe(|| Program::compile(expr)));
    match attempt {
        Ok(Ok(p)) => Ok(p),
        Ok(Err(e)) => Err(ManifestError::validation(
            source,
            format!("invalid CEL expression in {label}: {e}"),
        )),
        Err(_) => Err(ManifestError::validation(
            source,
            format!("invalid CEL expression in {label}: parser panicked on malformed input"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fake() -> PathBuf {
        PathBuf::from("/fake/main.json")
    }

    #[test]
    fn accepts_simple_boolean_expression() {
        validate("user.is_admin", &fake(), "process[0].if").unwrap();
    }

    #[test]
    fn accepts_chained_membership() {
        validate(
            "length(title) >= 1 && length(title) <= 200",
            &fake(),
            "title.validate",
        )
        .unwrap();
    }

    #[test]
    fn rejects_empty_expression() {
        let err = validate("   ", &fake(), "guard").unwrap_err();
        assert!(err.message.contains("must not be empty"));
    }

    #[test]
    fn rejects_syntax_error() {
        let err = validate("user.is_admin &&", &fake(), "guard").unwrap_err();
        assert!(
            err.message.contains("invalid CEL expression"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn scope_check_accepts_known_identifier() {
        validate_with_scope("title.size() > 3", &["title"], &fake(), "title.validate").unwrap();
    }

    #[test]
    fn scope_check_rejects_unknown_identifier_with_offending_name() {
        let err =
            validate_with_scope("user.is_admin", &["limit"], &fake(), "process[0].if").unwrap_err();
        assert!(
            err.message.contains("unknown identifier"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.contains("user"),
            "must name the offender: {}",
            err.message
        );
        assert!(
            err.message.contains("limit"),
            "must list the in-scope names: {}",
            err.message
        );
    }

    #[test]
    fn scope_check_ignores_stdlib_functions() {
        // `length` is a CEL stdlib function — it lives in the function
        // namespace, not the variable namespace, so it never trips the
        // scope check.
        validate_with_scope("length(title) >= 1", &["title"], &fake(), "title.validate").unwrap();
    }
}
