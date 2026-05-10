//! Per-project agent integration writers.
//!
//! Rublocks is agent-authored: every project committed to disk ships with the
//! per-agent integration files needed for any coding agent that opens the
//! project to immediately know the rublocks JSON shapes and conventions.
//! These files are rewritten at every `rublocks build` so they stay aligned
//! with the binary version that produced them.
//!
//! The schemas embedded inside these artifacts are invariant per binary
//! version — there is intentionally no per-project `dist/schemas/` copy.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

use crate::schema;

/// Description string written into the Claude skill frontmatter. Front-loads
/// the trigger keywords so Claude routes rublocks-related prompts here.
const SKILL_DESCRIPTION: &str = "Authoring or editing a rublocks project \u{2014} declarative JSON files (main.json, models/*.json, routes/*.json) that compile to a Rust/Axum web app. Use whenever the user asks to add, modify, or debug rublocks models, routes, services, layouts, or templates, or whenever a main.json with a rublocks-style shape is present.";

/// Body of the Claude skill, in markdown. Static across builds of the same
/// binary; the per-version JSON Schemas are appended at the end at render time.
const SKILL_BODY: &str = r#"# rublocks

A declarative JSON language. The agent writes JSON files; `rublocks build` emits a Rust/Axum project under `dist/`. `rublocks dev` watches the JSON, rebuilds, and livereloads the browser after every save.

This skill is rewritten by `rublocks build`; do not edit by hand — your changes will be overwritten on the next build.

## Project layout

- `main.json` — app name + services (postgres / redis). Required at the project root.
- `models/*.json` — one declared entity per file. Each emits a Rust struct.
- `routes/*.json` — one HTTP endpoint per file. Subdirectories allowed.
- `templates/*.html` — Askama-style templates referenced by `kind: page` routes.
- `layouts/*.json` — layout declarations.
- `migrations/*.sql` — hand-written SQL migrations.

## What works today

- `main.json` → AppState wiring + `/health` route.
- `models/*.json` → typed Rust structs (`serde::Serialize` + `sqlx::FromRow` when postgres is declared).
- `routes/*.json` → router entries with stub handlers — handler bodies are not yet generated.
- Templates, layouts, migrations: file discovery only, no rendering yet.

## Canonical examples

### main.json

```json
{
  "name": "myblog",
  "services": {
    "postgres": { "url": "env:DATABASE_URL" }
  }
}
```

### models/post.json

```json
{
  "name": "Post",
  "table": "posts",
  "fields": {
    "id":           { "type": "uuid",        "primary_key": true, "default": "gen_random_uuid()" },
    "slug":         { "type": "string",      "max_length": 200, "unique": true },
    "title":        { "type": "string",      "max_length": 200 },
    "body":         { "type": "text" },
    "published_at": { "type": "timestamptz", "nullable": true }
  }
}
```

### routes/posts-show.json

```json
{
  "path": "/posts/:slug",
  "method": "GET",
  "kind": "page",
  "template": "posts/show.html",
  "layout": "main"
}
```

## Field types

| `type`        | Rust type                       | Postgres column |
|---------------|---------------------------------|-----------------|
| `uuid`        | `uuid::Uuid`                    | `UUID`          |
| `string`      | `String`                        | `VARCHAR`       |
| `text`        | `String`                        | `TEXT`          |
| `int`         | `i32`                           | `INTEGER`       |
| `bigint`      | `i64`                           | `BIGINT`        |
| `bool`        | `bool`                          | `BOOLEAN`       |
| `timestamptz` | `chrono::DateTime<chrono::Utc>` | `TIMESTAMPTZ`   |
| `email`       | `String`                        | `VARCHAR`       |

`"nullable": true` wraps the Rust type in `Option<T>`.

## Conventions

- App name: lowercase ASCII letters / digits / `_` / `-`.
- Model `name`: PascalCase. Model `table`: snake_case.
- Route paths: leading `/`, `:param` for captures (rewritten to `{param}` for Axum at codegen time).
- `env:VAR_NAME` reads `std::env::var("VAR_NAME")` at startup; literal strings are embedded as-is.
- Unknown declarative attributes are accepted at parse time and reserved for future slices (migrations, `process:` blocks, ...).

## Workflow

1. Edit the JSON files.
2. Run `rublocks dev` — codegen + `cargo build` + supervised child + watcher + browser livereload.
3. Errors (manifest parse, codegen panic, `cargo build` failure, runtime crash) render in the browser with file path, line, and the offending snippet.
"#;

/// Write every per-agent integration file (Claude skill today; Codex `AGENTS.md`
/// and Cursor rules in the next slice). Called once per `rublocks build`.
pub fn write_all(project_dir: &Path) -> Result<()> {
    write_claude_skill(project_dir)?;
    Ok(())
}

/// Write `<project>/.claude/skills/rublocks/SKILL.md`.
///
/// Always overwrites: the file is fully owned by `rublocks build`. The
/// embedded schemas reflect the binary version that performed the write.
pub fn write_claude_skill(project_dir: &Path) -> Result<()> {
    let dir = project_dir.join(".claude").join("skills").join("rublocks");
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let path = dir.join("SKILL.md");
    fs::write(&path, render_skill())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Render the full `SKILL.md` content: frontmatter + body + appended schemas.
fn render_skill() -> String {
    let mut buf = String::with_capacity(SKILL_BODY.len() + 8 * 1024);
    buf.push_str("---\n");
    buf.push_str("name: rublocks\n");
    buf.push_str("description: ");
    buf.push_str(SKILL_DESCRIPTION);
    buf.push('\n');
    buf.push_str("---\n\n");
    // The body uses unicode escapes (\u{2014} etc.) so the markdown reads as
    // proper typography (em-dashes, arrows) after Rust string-literal decoding.
    buf.push_str(SKILL_BODY);
    buf.push_str("\n---\n\n");
    buf.push_str("## Reference: full JSON schemas (Draft-07)\n\n");
    buf.push_str("Derived from the parsing types of the rublocks binary that wrote this file. Authoritative for the version of the language this project compiles against.\n\n");
    for s in schema::all() {
        buf.push_str("### ");
        buf.push_str(s.title);
        buf.push_str("\n\n```json\n");
        buf.push_str(&s.pretty_json());
        buf.push_str("\n```\n\n");
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_claude_skill_creates_file_at_expected_path() {
        let dir = TempDir::new().unwrap();
        write_claude_skill(dir.path()).unwrap();
        let path = dir
            .path()
            .join(".claude")
            .join("skills")
            .join("rublocks")
            .join("SKILL.md");
        assert!(path.exists(), "expected {} to exist", path.display());
    }

    #[test]
    fn skill_contains_frontmatter_and_description() {
        let dir = TempDir::new().unwrap();
        write_claude_skill(dir.path()).unwrap();
        let content = read_skill(dir.path());
        assert!(content.starts_with("---\nname: rublocks\n"), "frontmatter missing");
        assert!(
            content.contains(SKILL_DESCRIPTION),
            "description not embedded"
        );
    }

    #[test]
    fn skill_lists_every_field_type() {
        let dir = TempDir::new().unwrap();
        write_claude_skill(dir.path()).unwrap();
        let content = read_skill(dir.path());
        for ty in [
            "uuid",
            "string",
            "text",
            "int",
            "bigint",
            "bool",
            "timestamptz",
            "email",
        ] {
            assert!(
                content.contains(&format!("`{ty}`")),
                "skill must reference field type `{ty}`"
            );
        }
    }

    #[test]
    fn skill_embeds_all_three_schemas() {
        let dir = TempDir::new().unwrap();
        write_claude_skill(dir.path()).unwrap();
        let content = read_skill(dir.path());
        for title in ["main.json", "models/*.json", "routes/*.json"] {
            assert!(
                content.contains(&format!("### {title}")),
                "skill must embed schema section for {title}"
            );
        }
        // Sanity-check that the actual schema payload is in there too.
        assert!(content.contains("\"$schema\""), "missing JSON Schema body");
    }

    #[test]
    fn write_is_idempotent() {
        let dir = TempDir::new().unwrap();
        write_claude_skill(dir.path()).unwrap();
        let first = read_skill(dir.path());
        write_claude_skill(dir.path()).unwrap();
        let second = read_skill(dir.path());
        assert_eq!(
            first, second,
            "successive writes must produce identical content"
        );
    }

    fn read_skill(project_dir: &Path) -> String {
        fs::read_to_string(
            project_dir
                .join(".claude")
                .join("skills")
                .join("rublocks")
                .join("SKILL.md"),
        )
        .unwrap()
    }
}
