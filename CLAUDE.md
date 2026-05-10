# rublocks

Declarative JSON language compiling to Rust/Axum web applications. Authored by coding agents.

## Workflow (project-specific, overrides global)

- Work directly on `main`. No worktrees, no feature branches.
- CI lives in `.github/workflows/ci.yml`: `cargo build --locked --all-targets` + `cargo test --locked --all-targets` on push and PR. Every behavior worth keeping must be locked by a test.
- Commit early, push to `origin/main` immediately to archive progress.

## Sandbox (`playground/`)

- The `playground/` directory is the user's testing ground for new generation biases.
- Current running example: a blog — models, routes, and templates are added as the language gains the capabilities they need.
- Tracked in git; `playground/dist/` stays gitignored via the global `**/dist/` rule.
- The agent must NOT modify `playground/` without explicit prior approval from the user.

## Code generation

- Emit Rust via `quote!` + `prettyplease`. No string templates.

## Dev-mode error UX

- Every failure that happens in dev mode (codegen panic, manifest parse error, `cargo build` failure, runtime crash) must surface in the browser with extreme clarity — file path, line, the offending JSON snippet when relevant, and a one-sentence hint for the user. The browser is the primary loop where the user instructs the coding agent on what to fix; a silent terminal-only error wastes the loop.

## Code documentation

- Document every public item in `src/` with a rustdoc comment.
- Comments should focus on the WHY — especially when an item embodies a project design decision (e.g. `ServiceUrl::Env`, codegen invariants, dev-mode dedup).
- Keep comments short. One or two lines is the target; if longer is needed, link to the relevant `docs/` page instead.

## Documentation (`docs/`)

- `docs/` holds the living capability documentation.
- When a capability changes (new command, new manifest field, new generated endpoint, new decision) the matching `docs/` page must be updated in the same commit.
- See `docs/README.md` for the index.
