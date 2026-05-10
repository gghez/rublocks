//! rublocks CLI entry point.
//!
//! Subcommands route to module-level functions: `build` (codegen only),
//! `dev` (codegen + cargo + supervised child + file watcher).
//! See `docs/cli.md` for the full reference.

mod codegen;
mod dev;
mod dev_error;
mod dev_services;
mod manifest;
mod models;
mod routes;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "rublocks", version, about = "Declarative JSON language compiling to Rust/Axum web applications")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scaffold a new rublocks project in the given directory.
    New {
        /// Project name (also used as the directory).
        name: String,
    },
    /// Compile the rublocks project to a Rust/Axum project under `<path>/dist`.
    Build {
        /// Project directory containing `main.json` (defaults to current directory).
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Build then run the generated project.
    Run {
        /// Project directory containing `main.json` (defaults to current directory).
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Run the project in dev mode: watch JSON files, rebuild + restart on change, with browser livereload.
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
    codegen::emit(&manifest, &dist_dir)?;
    println!(
        "rublocks: built `{}` -> {}",
        manifest.name,
        dist_dir.display()
    );
    Ok(())
}
