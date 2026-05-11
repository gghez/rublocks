# Project workflow

Conventions specific to the rublocks repository. Most also live in `CLAUDE.md` so an agent loads them automatically.

## Branching

- All work lands directly on `main`.
- No feature branches, no worktrees, no PRs.
- Commit small and often; push to `origin/main` to archive progress.

This is intentional for the early-construction phase. It will be revisited once the project has external contributors or a release process.

## CI / pipelines

GitHub Actions runs `.github/workflows/ci.yml` on every push to `main` and every pull request. The `test` job runs `cargo build --locked --all-targets` followed by `cargo test --locked --all-targets`, with `RUSTFLAGS=-Dwarnings` so a warning fails the build.

A follow-on `playground` job then exercises the full codegen → compile pipeline against the in-repo sandbox: it builds the `rublocks` binary, runs `rublocks build playground/`, then `cargo build` inside `playground/dist/`. This is the cheapest end-to-end check we have — if the generated project doesn't compile, the compiler is broken regardless of how green the unit tests are. The dist step drops `--locked` because rublocks regenerates `playground/dist/` from scratch on every run and emits only `Cargo.toml` + `src/` (no `Cargo.lock`); the cache action still pins the registry between runs.

Tests live next to the code they cover (`#[cfg(test)] mod tests` at the bottom of each `src/*.rs` file). Behaviors worth keeping must be locked by a test — otherwise they will silently regress.

Codegen carries an additional snapshot layer powered by [`insta`](https://insta.rs). The `.snap` files under `src/snapshots/` freeze the exact post-`prettyplease` output for a curated set of fixtures, so any cross-cutting regression (a tweak to one helper silently rewriting every emitted handler) shows up as a snapshot diff. See [testing.md](testing.md) for the convention. After an intentional codegen change, run `cargo insta review` (install once with `cargo install cargo-insta`) to inspect and accept the diff; CI runs `cargo test --locked --all-targets` and fails if any snapshot drifts.

## Sandbox: `playground/`

- `playground/` is a real rublocks project, tracked in git, used to exercise the compiler against new generation patterns.
- Current running example: a blog. Models, routes, and templates are added as the language gains the capabilities they need.
- `playground/dist/` remains gitignored via the global `**/dist/` rule.
- The agent may **only** modify files under `playground/` with explicit prior approval from the user. Otherwise, leave it alone — it represents the user's current experiment.

## Where to write new files

- Code → `src/`
- Documentation → `docs/`
- Tests → inline `#[cfg(test)] mod tests` blocks inside each `src/*.rs` file (preferred). Top-level `tests/` is reserved for integration scenarios that need to drive the binary or spin up real services.
- Scratch / experiments → `playground/` (user-controlled only — see above)

Avoid creating new top-level directories without a clear reason.

## Generated `dist/` directories

Every rublocks project produces a `dist/` directory next to its `main.json`. These are gitignored globally via `.gitignore` (`**/dist/`).
