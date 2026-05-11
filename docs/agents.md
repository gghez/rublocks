# Agent integration

rublocks is designed to be authored by coding agents (Claude, Codex, Cursor, ...) writing JSON, not by humans writing Rust. To make that practical, every `rublocks build` writes a self-contained per-agent integration file into the project, so any agent that opens the repository immediately knows the rublocks JSON shapes and conventions — no separate install step.

## Files written by `build`

| Path                                            | Consumer                  | Format                                |
|-------------------------------------------------|---------------------------|---------------------------------------|
| `.claude/skills/rublocks/SKILL.md`              | Claude / Claude Code      | Markdown with skill frontmatter       |
| `AGENTS.md`                                     | Codex, generic agents     | Markdown, rublocks-managed block      |
| `.cursor/rules/rublocks.mdc`                    | Cursor                    | Markdown with Cursor rule frontmatter |
| `.rublocks/schemas/*.schema.json`               | Editors (VS Code, Zed, …) | Draft-07 JSON Schema files            |
| `.vscode/settings.json`                         | VS Code                   | JSON, `json.schemas[]` mapping merged |

The first three are written by `src/agents.rs`, the last two by `src/schema_files.rs`. All are project-level and meant to be committed: a fresh `git clone` of a rublocks project is immediately agent-ready and editor-aware, exactly aligned with the binary version that produced the last build.

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

Each `rublocks build` emits the schemas in two complementary places:

- **Embedded** inside the agent artifacts (`SKILL.md`, `AGENTS.md`, `.cursor/rules/rublocks.mdc`) so an agent reading any one of them has the full surface in its context, with zero extra reads.
- **On disk** under `<project>/.rublocks/schemas/`, one file per surface (`main.schema.json`, `model.schema.json`, `route.schema.json`, `layout.schema.json`, `input.schema.json`, and `blocks/<kind>.schema.json` for each registered block). This is the editor channel — VS Code, Zed, and other JSON-aware editors pick the schemas up via the `.vscode/settings.json` mapping written alongside.

Both paths come from the same `schema::all()` call, so the embedded copy and the on-disk copy cannot drift. Hosting the schemas under a stable URL (e.g. `schemastore.org`) is a future step once the language stabilises.

## VS Code wiring

`rublocks build` writes (or merges) `<project>/.vscode/settings.json` with a `json.schemas` array:

```json
{
  "json.schemas": [
    { "fileMatch": ["main.json"],          "url": "./.rublocks/schemas/main.schema.json" },
    { "fileMatch": ["models/*.json"],      "url": "./.rublocks/schemas/model.schema.json" },
    { "fileMatch": ["routes/**/*.json"],   "url": "./.rublocks/schemas/route.schema.json" },
    { "fileMatch": ["layouts/*.json"],     "url": "./.rublocks/schemas/layout.schema.json" }
  ]
}
```

Unrelated settings keys in an existing `settings.json` are preserved — only the `json.schemas` array is owned by rublocks. The merge logic lives in `src/schema_files.rs::merge_vscode_settings`.

Editors that read `$schema` directly (Zed, JSON-LSP setups) can rely on the on-disk files alone without VS Code-specific configuration.

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
- No `dist/schemas/` directory. `dist/` is wiped on every build and gitignored — schemas need a stable home, so they live under `.rublocks/schemas/` instead.
- No mutation of user-authored `*.json` files. Editors discover the schemas via the `.vscode/settings.json` mapping (and `$schema` URLs that editors and humans can add by hand).
- No agents-specific subcommand: the writers are invoked by `build` (and therefore also by `dev`, which calls `build` on every change). There is no `rublocks agents init` to remember.
