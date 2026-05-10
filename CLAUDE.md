# rublocks

Declarative JSON language compiling to Rust/Axum web applications. Authored by coding agents.

## Workflow (project-specific, overrides global)

- Work directly on `main`. No worktrees, no feature branches.
- No CI pipeline.
- Commit early, push to `origin/main` immediately to archive progress.

## Sandbox (`playground/`)

- The `playground/` directory is the user's testing ground for new generation biases.
- The agent must NOT modify `playground/` without explicit prior approval from the user.
- It is gitignored and may contain anything the user wants to experiment with.

## Code generation

- Emit Rust via `quote!` + `prettyplease`. No string templates.

## Code documentation

- Document every public item in `src/` with a rustdoc comment.
- Comments should focus on the WHY — especially when an item embodies a project design decision (e.g. `ServiceUrl::Env`, codegen invariants, dev-mode dedup).
- Keep comments short. One or two lines is the target; if longer is needed, link to the relevant `docs/` page instead.

## Documentation (`docs/`)

- `docs/` holds the living capability documentation.
- When a capability changes (new command, new manifest field, new generated endpoint, new decision) the matching `docs/` page must be updated in the same commit.
- See `docs/README.md` for the index.
