# Project workflow

Conventions specific to the rublocks repository. Most also live in `CLAUDE.md` so an agent loads them automatically.

## Branching

- All work lands directly on `main`.
- No feature branches, no worktrees, no PRs.
- Commit small and often; push to `origin/main` to archive progress.

This is intentional for the early-construction phase. It will be revisited once the project has external contributors or a release process.

## CI / pipelines

None. The user runs builds and tests locally.

## Sandbox: `playground/`

- `playground/` is gitignored.
- The user maintains it as a real rublocks project for testing the compiler against new generation patterns.
- The agent may **only** modify files under `playground/` with explicit prior approval from the user. Otherwise, leave it alone — it represents the user's current experiment.

## Where to write new files

- Code → `src/`
- Documentation → `docs/`
- Tests → `tests/` (not yet established)
- Scratch / experiments → `playground/` (user-controlled only — see above)

Avoid creating new top-level directories without a clear reason.

## Generated `dist/` directories

Every rublocks project produces a `dist/` directory next to its `main.json`. These are gitignored globally via `.gitignore` (`**/dist/`).
