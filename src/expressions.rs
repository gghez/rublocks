//! Validation of CEL (Common Expression Language) snippets declared in
//! manifest JSON.
//!
//! Every CEL expression accepted by rublocks (`route.guard`,
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

/// Compile `expr` as CEL. Returns `Ok` only when the source parses; the
/// resulting `Program` is dropped because slice-4 just needs syntactic
/// validation. The runtime layer will recompile against a typed context
/// (and cache the result) when process blocks execute.
///
/// `cel-interpreter 0.10` can panic on certain malformed inputs (the
/// underlying antlr-generated grammar reaches an `unreachable!()`), so
/// the compile call is wrapped in `catch_unwind`. Surfacing a panic as a
/// structured manifest error is strictly better than crashing the build.
pub fn validate(expr: &str, source: &Path, label: &str) -> Result<(), ManifestError> {
    if expr.trim().is_empty() {
        return Err(ManifestError::validation(
            source,
            format!("{label} CEL expression must not be empty"),
        ));
    }
    let attempt = catch_unwind(AssertUnwindSafe(|| Program::compile(expr)));
    match attempt {
        Ok(Ok(_)) => Ok(()),
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
        validate("user.is_admin", &fake(), "route.guard").unwrap();
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
}
