# Agent integration

rublocks is designed to be authored by coding agents (Claude, Codex, Cursor, ...) writing JSON, not by humans writing Rust. To make that practical, every `rublocks build` writes a self-contained per-agent integration file into the project, so any agent that opens the repository immediately knows the rublocks JSON shapes and conventions — no separate install step.

## Files written by `build`

| Path                                            | Consumer                  | Format                                |
|-------------------------------------------------|---------------------------|---------------------------------------|
| `.claude/skills/rublocks/SKILL.md`              | Claude / Claude Code      | Markdown with skill frontmatter       |
| `AGENTS.md`                                     | Codex, generic agents     | Markdown, rublocks-managed block      |
| `.cursor/rules/rublocks.mdc`                    | Cursor                    | Markdown with Cursor rule frontmatter |

All three are written by `src/agents.rs` at the end of `build()`. They are project-level and meant to be committed: a fresh `git clone` of a rublocks project is immediately agent-ready, exactly aligned with the binary version that produced the last build.

## Content

Every artifact embeds the same body:

- One-paragraph orientation (what rublocks is, what `build` and `dev` do).
- Project layout (`main.json`, `models/*.json`, `routes/*.json`, `templates/`, `layouts/`, `migrations/`).
- Honest "what works today" section (capabilities of the binary that produced the artifact).
- Canonical examples for `main.json`, a model, and a route.
- Field-type reference table (`uuid` → `uuid::Uuid` → `UUID`, etc.).
- Conventions (PascalCase model names, lowercase app name, `env:VAR_NAME` URLs, ...).
- The dev workflow loop and the browser-first error policy.
- The full Draft-07 JSON schemas for `main.json`, `models/*.json`, `routes/*.json`, `layouts/*.json`, **plus one schema per registered [block](blocks/README.md)** (`block: db.find_many`, `block: db.find_one`, ...), all derived from the parsing types via `schemars`.

The shared body lives in `SHARED_BODY` in `src/agents.rs`; the per-version schemas are appended by `render_body()`. The per-block entries come from `blocks::registry()` so adding a new block automatically extends the agent artifacts.

## One schema set per binary

The JSON schemas are derived from the Rust parsing types (`RawManifest`, `RawModel`, `RawRoute`) via `schemars`. They are invariant for a given rublocks binary — every project that runs `rublocks <version>` sees the same shapes.

Consequence: the schemas live **only** inside the agent artifacts (each embeds its own copy). There is intentionally no `dist/schemas/` directory and no per-project schema files outside the agent artifacts. Live IDE schema validation through `"$schema"` references is deferred until there is a hosted schema store.

## AGENTS.md merge semantics

`AGENTS.md` is the only artifact a user is likely to author themselves. The rublocks-managed block is delimited by HTML comments:

```markdown
<!-- rublocks:start -->
<!-- This block is managed by `rublocks build`. Do not edit by hand. -->

...generated content...

<!-- rublocks:end -->
```

Merge rules on every build:

- **File absent** — write a default `# AGENTS` header + the block.
- **File present, no markers** — append the block at the end, separated by a single blank line.
- **File present with markers** — replace the content between markers in place. User-authored content above and below survives.

The merge is byte-idempotent: re-running `rublocks build` without source changes leaves `AGENTS.md` unchanged.

## Why per-project (not global)

Codex's `AGENTS.md` and Cursor's `.cursor/rules/*.mdc` are project-root files by design — there is no machine-wide install. Aligning Claude's skill on the same per-project model (rather than a global `~/.claude/skills/rublocks/`) keeps all three consistent and makes a single `git clone` the only setup step. The cost is a few markdown files per project; the benefit is no machine-level state to manage and exact version alignment with the binary that last touched the project.

## What is NOT written

- No global skill at `~/.claude/skills/...`. Per-project only.
- No `dist/schemas/` directory. The schemas live inside the agent artifacts.
- No agents-specific subcommand: the writers are invoked by `build` (and therefore also by `dev`, which calls `build` on every change). There is no `rublocks agents init` to remember.
