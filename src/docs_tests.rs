//! Validates that every annotated JSON block in `docs/*.md` still parses
//! against the rublocks schema the binary actually accepts.
//!
//! Convention: a fenced ```json (or ```jsonc) block prefixed with an HTML
//! comment of the form `<!-- rb:<kind> -->` is treated as an example and
//! validated against `<kind>` (one of `manifest`, `model`, `route`,
//! `layout`). Blocks without an annotation are illustrative fragments and
//! intentionally skipped — keep this in mind when writing new docs:
//! annotate the canonical example, leave fragments unannotated.
//!
//! Lives under `#[cfg(test)]` only; the validation helpers it calls are
//! compiled into the test binary via the same gate.

use std::path::PathBuf;

/// One annotated example found in a doc file.
struct Example {
    file: &'static str,
    /// 1-based line number of the opening code fence.
    line: usize,
    kind: String,
    /// JSON payload with `//` line comments stripped (for `jsonc` blocks).
    body: String,
}

/// Extract every `<!-- rb:<kind> -->` annotated code block from a markdown
/// document. The annotation must appear within five lines above the opening
/// fence (allowing a single blank line in between). Code fences are matched
/// by exact lines starting with ```` ``` ````; nested fences are not supported.
fn extract(file: &'static str, md: &str) -> Vec<Example> {
    let lines: Vec<&str> = md.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        if trimmed.starts_with("```json") || trimmed.starts_with("```jsonc") {
            // Look up to 5 lines back for an annotation, skipping blanks.
            let mut kind: Option<String> = None;
            let mut j = i;
            let mut blanks = 0;
            while j > 0 && blanks < 2 {
                j -= 1;
                let prev = lines[j].trim();
                if prev.is_empty() {
                    blanks += 1;
                    continue;
                }
                if let Some(k) = parse_annotation(prev) {
                    kind = Some(k);
                }
                break;
            }
            // Find the closing fence.
            let mut k = i + 1;
            while k < lines.len() && !lines[k].trim().starts_with("```") {
                k += 1;
            }
            if let Some(kind) = kind {
                let body: String = lines[i + 1..k]
                    .iter()
                    .map(|l| strip_line_comment(l))
                    .collect::<Vec<_>>()
                    .join("\n");
                out.push(Example {
                    file,
                    line: i + 1,
                    kind,
                    body,
                });
            }
            i = k + 1;
            continue;
        }
        i += 1;
    }
    out
}

/// Parse `<!-- rb:<kind> -->` (whitespace-tolerant) into the kind string.
fn parse_annotation(line: &str) -> Option<String> {
    let s = line.trim();
    let s = s.strip_prefix("<!--")?.trim_end_matches("-->").trim();
    let s = s.strip_prefix("rb:")?.trim();
    if s.is_empty() {
        return None;
    }
    Some(s.to_string())
}

/// Strip a `//` line comment from outside JSON string literals. The state
/// machine tracks string entry/exit with backslash escapes so a `//` inside
/// `"a // b"` is preserved.
fn strip_line_comment(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut in_str = false;
    let mut esc = false;
    let mut cut: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_str = true;
            i += 1;
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            cut = Some(i);
            break;
        }
        i += 1;
    }
    match cut {
        Some(at) => line[..at].trim_end().to_string(),
        None => line.to_string(),
    }
}

fn validate(ex: &Example) -> Result<(), String> {
    // Each per-kind validator returns a string-shaped error so it can carry
    // whichever flavour (serde / manifest / block-registry) makes sense; we
    // just decorate with the file + line so failures pinpoint the offending
    // markdown fence.
    let r: Result<(), String> = match ex.kind.as_str() {
        "manifest" => crate::manifest::validate_doc_example(&ex.body).map_err(|e| e.to_string()),
        "model" => crate::models::validate_doc_example(&ex.body).map_err(|e| e.to_string()),
        "route" => crate::routes::validate_doc_example(&ex.body),
        "layout" => crate::layouts::validate_doc_example(&ex.body),
        other => {
            return Err(format!(
                "{}:{} unknown annotation `<!-- rb:{} -->`",
                ex.file, ex.line, other
            ));
        }
    };
    r.map_err(|e| format!("{}:{} {}: {}", ex.file, ex.line, ex.kind, e))
}

/// Walk every `docs/*.md` and run the extractor. Reading from disk (rather
/// than `include_str!`) keeps the test reactive to renames or new files.
fn collect_all() -> Vec<Example> {
    let docs_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&docs_dir)
        .expect("docs/ should exist")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
        .collect();
    files.sort();
    let mut out = Vec::new();
    for path in files {
        let name: &'static str = Box::leak(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
                .into_boxed_str(),
        );
        let md = std::fs::read_to_string(&path).expect("readable md");
        out.extend(extract(name, &md));
    }
    out
}

#[test]
fn every_annotated_doc_example_parses() {
    let examples = collect_all();
    assert!(
        !examples.is_empty(),
        "no annotated examples found in docs/ — extractor regression?"
    );
    let mut failures = Vec::new();
    for ex in &examples {
        if let Err(msg) = validate(ex) {
            failures.push(msg);
        }
    }
    assert!(
        failures.is_empty(),
        "doc example validation failed:\n  {}",
        failures.join("\n  ")
    );
}

#[test]
fn at_least_one_example_per_capability() {
    let examples = collect_all();
    for kind in ["manifest", "model", "route", "layout"] {
        assert!(
            examples.iter().any(|e| e.kind == kind),
            "no `<!-- rb:{kind} -->` block found in docs/ — every capability should have at least one canonical example",
        );
    }
}

#[test]
fn strip_line_comment_keeps_url_slashes() {
    assert_eq!(
        strip_line_comment(r#"  "url": "postgres://x" // comment"#),
        r#"  "url": "postgres://x""#
    );
    assert_eq!(strip_line_comment("// whole line"), "");
    assert_eq!(strip_line_comment(r#""a // b""#), r#""a // b""#);
}
