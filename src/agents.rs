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
//! Each artifact embeds the schemas in full so that the corresponding agent
//! (Claude, Codex/AGENTS.md consumers, Cursor) sees a self-contained context
//! without depending on the other artifacts.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

use crate::schema;

/// Description string written into the Claude skill frontmatter. Front-loads
/// the trigger keywords so Claude routes rublocks-related prompts here.
const SKILL_DESCRIPTION: &str = "Authoring or editing a rublocks project \u{2014} declarative JSON files (main.json, models/*.json, routes/*.json) that compile to a Rust/Axum web app. Use whenever the user asks to add, modify, or debug rublocks models, routes, services, layouts, or templates, or whenever a main.json with a rublocks-style shape is present.";

/// Description written into the Cursor rule frontmatter.
const CURSOR_DESCRIPTION: &str =
    "rublocks (declarative JSON \u{2192} Rust/Axum) project conventions and JSON schemas";

/// Shared markdown body. Identical content goes into the Claude skill, the
/// rublocks-managed block of `AGENTS.md`, and the Cursor `.mdc` rule. The
/// per-version JSON Schemas are appended at the end by `render_body()`.
const SHARED_BODY: &str = r#"# rublocks

A declarative JSON language. The agent writes JSON files; `rublocks build` emits a Rust/Axum project under `dist/`. `rublocks dev` watches the JSON, rebuilds, and livereloads the browser after every save.

This file is rewritten by `rublocks build`; do not edit by hand — your changes will be overwritten on the next build.

## Project layout

- `main.json` — app name + version + services (postgres / redis). Required at the project root.
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
  "version": "0.1.0",
  "description": "A blog with public posts and admin moderation.",
  "language": "en-US",
  "services": {
    "db": { "kind": "postgres", "url": "env:DATABASE_URL" }
  }
}
```

`version` is mandatory (SemVer 2.0.0). It threads into the generated `Cargo.toml` `package.version`, OpenAPI `info.version`, the `X-App-Version` response header, and the dev-mode error page footer.

`description` is mandatory — a single-line synopsis (trimmed, max 280 chars, no newlines). It threads into the generated `Cargo.toml` `package.description`, the dev-mode landing subtitle + `<meta name="description">`, and the dev-mode error overlay subtitle.

`language` is required and must be a BCP 47 tag (e.g. `"en-US"`, `"fr-FR"`, `"pt-BR"`). It drives `<html lang="...">` on every generated page, the `Content-Language` HTTP header, and the dev-mode error overlay's localized strings.

`kind` accepts `postgres` (default), `mysql`, `mariadb`, `mssql`. The legacy `"postgres": { "url": ... }` shorthand still works for postgres projects.

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
    "author_id":    { "type": "uuid",        "references": "Author.id" },
    "published_at": { "type": "timestamptz", "nullable": true }
  },
  "indexes": [
    { "fields": ["author_id", "published_at"] }
  ],
  "checks": [
    { "name": "title_not_empty", "expr": "length(title) > 0" }
  ]
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
- Models support table-level `indexes`, `foreign_keys`, `checks` and per-field shorthand `unique` / `references` (`"Author.id"` or `{ "model": "...", "field": "...", "on_delete": "..." }`). Validation is performed at parse time.
- Unknown declarative attributes are accepted at parse time and reserved for future slices (`process:` blocks, ...).

## Workflow

1. Edit the JSON files.
2. Run `rublocks dev` — codegen + `cargo build` + supervised child + watcher + browser livereload.
3. Errors (manifest parse, codegen panic, `cargo build` failure, runtime crash) render in the browser with file path, line, and the offending snippet.
"#;

/// Marker delimiting the rublocks-managed region inside `AGENTS.md`.
const START_MARKER: &str = "<!-- rublocks:start -->";
const END_MARKER: &str = "<!-- rublocks:end -->";
/// Human-facing reminder placed immediately after the start marker.
const MANAGED_NOTICE: &str =
    "<!-- This block is managed by `rublocks build`. Do not edit by hand. -->";

/// Write every per-agent integration file. Called once per `rublocks build`.
pub fn write_all(project_dir: &Path) -> Result<()> {
    write_claude_skill(project_dir)?;
    write_agents_md(project_dir)?;
    write_cursor_rules(project_dir)?;
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

/// Write or merge `<project>/AGENTS.md`.
///
/// The rublocks-managed region is delimited by `START_MARKER` / `END_MARKER`
/// so user-authored content above and below the block is preserved across
/// rewrites.
pub fn write_agents_md(project_dir: &Path) -> Result<()> {
    let path = project_dir.join("AGENTS.md");
    let existing = match fs::read_to_string(&path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).context(format!("failed to read {}", path.display())),
    };
    fs::write(&path, merge_agents_md(existing.as_deref()))
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Write `<project>/.cursor/rules/rublocks.mdc`.
///
/// `alwaysApply: true` so Cursor always loads the rublocks context for any
/// edit performed inside a rublocks project.
pub fn write_cursor_rules(project_dir: &Path) -> Result<()> {
    let dir = project_dir.join(".cursor").join("rules");
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let path = dir.join("rublocks.mdc");
    fs::write(&path, render_cursor_rule())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Render the body shared by all three artifacts: the static markdown above
/// followed by the per-version JSON Schemas.
fn render_body() -> String {
    let mut buf = String::with_capacity(SHARED_BODY.len() + 8 * 1024);
    buf.push_str(SHARED_BODY);
    buf.push_str("\n---\n\n");
    buf.push_str("## Reference: full JSON schemas (Draft-07)\n\n");
    buf.push_str("Derived from the parsing types of the rublocks binary that wrote this file. Authoritative for the version of the language this project compiles against.\n\n");
    for s in schema::all() {
        buf.push_str("### ");
        buf.push_str(&s.title);
        buf.push_str("\n\n```json\n");
        buf.push_str(&s.pretty_json());
        buf.push_str("\n```\n\n");
    }
    buf
}

/// Render the full `SKILL.md` content: Claude frontmatter + shared body.
fn render_skill() -> String {
    format!(
        "---\nname: rublocks\ndescription: {SKILL_DESCRIPTION}\n---\n\n{}",
        render_body()
    )
}

/// Render the full `.cursor/rules/rublocks.mdc` content.
fn render_cursor_rule() -> String {
    format!(
        "---\ndescription: {CURSOR_DESCRIPTION}\nalwaysApply: true\n---\n\n{}",
        render_body()
    )
}

/// Render just the rublocks-managed block (markers + notice + body) destined
/// for `AGENTS.md`.
fn render_agents_block() -> String {
    format!(
        "{START_MARKER}\n{MANAGED_NOTICE}\n\n{}\n{END_MARKER}\n",
        render_body()
    )
}

/// Compute the new content of `AGENTS.md` given its current content (or `None`
/// when the file does not exist). Pure function for easy unit testing.
fn merge_agents_md(existing: Option<&str>) -> String {
    let block = render_agents_block();
    match existing {
        None => format!("# AGENTS\n\nProject conventions for coding agents.\n\n{block}"),
        Some(content) => {
            if let (Some(start), Some(end)) = (content.find(START_MARKER), content.find(END_MARKER))
                && end > start
            {
                let end_with_marker = end + END_MARKER.len();
                let tail = &content[end_with_marker..];
                let tail = tail.strip_prefix('\n').unwrap_or(tail);
                let mut out = String::with_capacity(content.len() + block.len());
                out.push_str(&content[..start]);
                out.push_str(&block);
                out.push_str(tail);
                return out;
            }
            // No usable markers: append our block at the end, separated by a
            // single blank line from prior content.
            let mut out = String::with_capacity(content.len() + block.len() + 2);
            out.push_str(content);
            if !content.is_empty() && !content.ends_with('\n') {
                out.push('\n');
            }
            if !content.is_empty() {
                out.push('\n');
            }
            out.push_str(&block);
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ---- Claude skill ----------------------------------------------------

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
        assert!(
            content.starts_with("---\nname: rublocks\n"),
            "frontmatter missing"
        );
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
    fn skill_embeds_all_four_schemas() {
        let dir = TempDir::new().unwrap();
        write_claude_skill(dir.path()).unwrap();
        let content = read_skill(dir.path());
        for title in [
            "main.json",
            "models/*.json",
            "routes/*.json",
            "layouts/*.json",
        ] {
            assert!(
                content.contains(&format!("### {title}")),
                "skill must embed schema section for {title}"
            );
        }
        assert!(content.contains("\"$schema\""), "missing JSON Schema body");
    }

    #[test]
    fn skill_write_is_idempotent() {
        let dir = TempDir::new().unwrap();
        write_claude_skill(dir.path()).unwrap();
        let first = read_skill(dir.path());
        write_claude_skill(dir.path()).unwrap();
        let second = read_skill(dir.path());
        assert_eq!(first, second);
    }

    // ---- AGENTS.md merge -------------------------------------------------

    #[test]
    fn fresh_agents_md_includes_header_and_block() {
        let out = merge_agents_md(None);
        assert!(out.starts_with("# AGENTS\n"), "missing default header");
        assert!(out.contains(START_MARKER));
        assert!(out.contains(END_MARKER));
        assert!(out.contains("rublocks"));
    }

    #[test]
    fn merge_preserves_user_content_above_and_below_block() {
        let user = "# My Project\n\nThe team-wide guidance.\n";
        let first = merge_agents_md(Some(user));
        // User content survives.
        assert!(first.contains("# My Project"));
        assert!(first.contains("The team-wide guidance."));
        // Block appended after user content.
        assert!(first.contains(START_MARKER));
        assert!(first.contains(END_MARKER));

        // Add some user content below the block too, then re-merge.
        let with_tail = format!("{first}\n## Other section\n\nMore prose.\n");
        let second = merge_agents_md(Some(&with_tail));
        // The pre-block content survives.
        assert!(second.contains("# My Project"));
        assert!(second.contains("The team-wide guidance."));
        // The post-block content survives.
        assert!(second.contains("## Other section"));
        assert!(second.contains("More prose."));
        // Block is still there exactly once.
        assert_eq!(second.matches(START_MARKER).count(), 1);
        assert_eq!(second.matches(END_MARKER).count(), 1);
    }

    #[test]
    fn merge_is_idempotent_when_block_already_present() {
        let user = "# My Project\n";
        let first = merge_agents_md(Some(user));
        let second = merge_agents_md(Some(&first));
        assert_eq!(first, second, "rewriting AGENTS.md must be a no-op");
    }

    #[test]
    fn write_agents_md_round_trips_idempotently_on_disk() {
        let dir = TempDir::new().unwrap();
        write_agents_md(dir.path()).unwrap();
        let first = fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        write_agents_md(dir.path()).unwrap();
        let second = fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn write_agents_md_preserves_pre_existing_user_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("AGENTS.md");
        fs::write(&path, "# Team rules\n\nBe kind. Write tests.\n").unwrap();
        write_agents_md(dir.path()).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("# Team rules"));
        assert!(content.contains("Be kind. Write tests."));
        assert!(content.contains(START_MARKER));
    }

    // ---- Cursor rule -----------------------------------------------------

    #[test]
    fn write_cursor_rules_creates_file_at_expected_path() {
        let dir = TempDir::new().unwrap();
        write_cursor_rules(dir.path()).unwrap();
        let path = dir
            .path()
            .join(".cursor")
            .join("rules")
            .join("rublocks.mdc");
        assert!(path.exists());
    }

    #[test]
    fn cursor_rule_has_always_apply_frontmatter() {
        let dir = TempDir::new().unwrap();
        write_cursor_rules(dir.path()).unwrap();
        let content = read_cursor(dir.path());
        assert!(content.starts_with("---\n"));
        assert!(content.contains("alwaysApply: true"));
        assert!(content.contains(CURSOR_DESCRIPTION));
        // Body is shared, sanity-check one schema landmark.
        assert!(content.contains("\"$schema\""));
    }

    #[test]
    fn cursor_rule_write_is_idempotent() {
        let dir = TempDir::new().unwrap();
        write_cursor_rules(dir.path()).unwrap();
        let first = read_cursor(dir.path());
        write_cursor_rules(dir.path()).unwrap();
        let second = read_cursor(dir.path());
        assert_eq!(first, second);
    }

    // ---- write_all -------------------------------------------------------

    #[test]
    fn write_all_lays_down_all_three_artifacts() {
        let dir = TempDir::new().unwrap();
        write_all(dir.path()).unwrap();
        assert!(dir.path().join(".claude/skills/rublocks/SKILL.md").exists());
        assert!(dir.path().join("AGENTS.md").exists());
        assert!(dir.path().join(".cursor/rules/rublocks.mdc").exists());
    }

    // ---- helpers ---------------------------------------------------------

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

    fn read_cursor(project_dir: &Path) -> String {
        fs::read_to_string(
            project_dir
                .join(".cursor")
                .join("rules")
                .join("rublocks.mdc"),
        )
        .unwrap()
    }
}
