# rublocks

Declarative JSON language compiling to Rust/Axum web applications. Authored by coding agents.

## Workflow (project-specific, overrides global)

- Work directly on `main`. No worktrees, no feature branches.
- No CI pipeline.
- Commit early, push to `origin/main` immediately to archive progress.

## Code generation

- Emit Rust via `quote!` + `prettyplease`. No string templates.
