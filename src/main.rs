//! rublocks CLI entry point.
//!
//! Subcommands route to module-level functions: `build` (codegen only),
//! `dev` (codegen + cargo + supervised child + file watcher).
//! See `docs/cli.md` for the full reference.

// `DevError` carries cargo stderr output and is intentionally large; the dev
// supervisor's failure path is not a hot loop, so boxing every Result variant
// would add noise without measurable benefit.
#![allow(clippy::result_large_err)]

mod agents;
mod codegen;
mod dev;
mod dev_error;
mod dev_services;
mod docker;
mod expressions;
mod layouts;
mod manifest;
mod migrations;
mod models;
mod routes;
mod schema;

#[cfg(test)]
mod docs_tests;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "rublocks",
    version,
    about = "Declarative JSON language compiling to Rust/Axum web applications",
    long_about = "rublocks compiles a directory of declarative JSON files into a Rust/Axum \
project under `<path>/dist`. The generated project is idiomatic Rust — typed structs, wired \
services, an async main — and is meant to be authored by coding agents rather than humans \
writing JSON by hand.\n\
\n\
Project layout (all relative to <path>, all optional except main.json):\n  \
  main.json          application manifest (name, services, http middleware)\n  \
  models/*.json      one declarative entity per file (struct + table)\n  \
  routes/**/*.json   one HTTP endpoint per file (page or api)\n  \
  layouts/*.json     shared template wrappers referenced by routes\n  \
  templates/*.html   Askama HTML for `kind: page` routes\n  \
  migrations/        forward-only SQL, generated from model diffs on every build\n\
\n\
On every build, rublocks also refreshes per-agent integration files at the project root:\n  \
  .claude/skills/rublocks/SKILL.md      Claude Code skill\n  \
  AGENTS.md                              Codex / generic AGENTS.md block\n  \
  .cursor/rules/rublocks.mdc            Cursor rule\n\
\n\
See the full reference under docs/ in the repository — start with docs/README.md."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scaffold a new rublocks project in the given directory.
    #[command(
        long_about = "Create a starter rublocks project layout. NOT YET IMPLEMENTED — currently bails.",
        after_help = "Example:\n  rublocks new myblog"
    )]
    New {
        /// Project name (also used as the directory).
        name: String,
    },
    /// Compile the rublocks project to a Rust/Axum project under `<path>/dist`.
    #[command(
        long_about = "Codegen-only pass: read main.json, parse models/routes/layouts, generate \
migrations from model diffs (forward-only), wipe and rewrite `<path>/dist/` with a fresh Cargo \
project, and refresh the per-agent integration files at the project root.\n\
\n\
`build` does NOT invoke `cargo build` — run that yourself inside `dist/` to produce a binary, \
or use `rublocks dev` for the full iteration loop.",
        after_help = "Examples:\n  \
  rublocks build                # build the project in the current directory\n  \
  rublocks build ./playground   # build a specific project\n\
\n\
The generated binary recognises one extra invocation when migrations exist:\n  \
  ./<app>                # serve (default)\n  \
  ./<app> migrate        # apply pending migrations and exit 0\n  \
  ./<app> migrate --list # list every migration with state"
    )]
    Build {
        /// Project directory containing `main.json` (defaults to current directory).
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Build then run the generated project.
    #[command(
        long_about = "Codegen + `cargo build` + run the resulting binary, without file watching. \
NOT YET IMPLEMENTED — currently bails.",
        after_help = "Example:\n  rublocks run ./playground"
    )]
    Run {
        /// Project directory containing `main.json` (defaults to current directory).
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Run the project in dev mode: watch JSON files, rebuild + restart on change, with browser livereload.
    #[command(
        long_about = "Full iteration loop: build, `cargo build` in dist/, spawn the generated \
binary with `RUBLOCKS_DEV=1`, then watch `*.json` and `*.html` under the project (excluding \
dist/). On each detected change (debounced 300ms, deduplicated by content hash), kill the child, \
re-run codegen, `cargo build` incrementally, and respawn.\n\
\n\
Open browser tabs reconnect via SSE at `/__rublocks/events` and reload after every restart. \
Errors (codegen panics, manifest parse errors, `cargo build` failures) surface in the browser \
overlay with file, line, and the offending snippet — not just in the terminal.\n\
\n\
For services declared as `env:VAR` that aren't set, dev mode provisions a labelled Docker \
container (postgres / redis) with a persistent volume, injects the URL into the child, and \
`docker stop`s it cleanly on Ctrl+C.\n\
\n\
Stop with Ctrl+C; the supervisor kills the child cleanly before exiting.",
        after_help = "Example:\n  rublocks dev ./playground"
    )]
    Dev {
        /// Project directory containing `main.json` (defaults to current directory).
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::New { name } => anyhow::bail!("`new {name}` not implemented yet"),
        Command::Build { path } => build(&resolve(&path)?),
        Command::Run { path } => anyhow::bail!("`run {}` not implemented yet", path.display()),
        Command::Dev { path } => dev::run(&resolve(&path)?),
    }
}

/// Canonicalize a CLI path argument so downstream modules always work with absolute paths.
///
/// All path-handling code (codegen, file watcher, `dist_canon` filter) assumes
/// absolute paths; centralizing the canonicalization here keeps that invariant.
fn resolve(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("invalid path: {}", path.display()))
}

/// One-shot codegen pass — no `cargo build`, no child process.
///
/// `rublocks build` is intentionally a thin layer over `codegen::emit`; running
/// the binary is a separate step (`cargo build` in `dist/`, or `rublocks dev`).
fn build(project_dir: &Path) -> Result<()> {
    let manifest = manifest::Manifest::load(project_dir)?;
    let dist_dir = project_dir.join("dist");
    // Generate (or refresh) project migrations BEFORE codegen so the
    // generated dist binary can wire `sqlx::migrate!` against the final
    // migration set on disk. Mirroring to dist/migrations/ runs after
    // codegen because codegen wipes dist/.
    let db_kind = manifest
        .database
        .as_ref()
        .map(|d| d.kind)
        .unwrap_or_default();
    if let Some(emitted) = migrations::generate(project_dir, &manifest.models, db_kind)? {
        println!("rublocks: wrote migration {}", emitted.path.display());
    }
    codegen::emit(&manifest, project_dir, &dist_dir)?;
    migrations::mirror(project_dir, &dist_dir)?;
    docker::emit(&manifest, &dist_dir)?;
    agents::write_all(project_dir)?;
    println!(
        "rublocks: built `{}` -> {}",
        manifest.name,
        dist_dir.display()
    );
    Ok(())
}
