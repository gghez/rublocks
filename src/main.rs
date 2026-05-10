mod codegen;
mod manifest;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "rublocks", version, about = "Declarative JSON language compiling to Rust/Axum web applications")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scaffold a new rublocks project in the given directory
    New {
        /// Project name (also used as the directory)
        name: String,
    },
    /// Compile the rublocks project in the current directory to a Rust/Axum project under ./dist
    Build,
    /// Build then run the generated project
    Run,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::New { name } => anyhow::bail!("`new {name}` not implemented yet"),
        Command::Build => build(),
        Command::Run => anyhow::bail!("`run` not implemented yet"),
    }
}

fn build() -> Result<()> {
    let project_dir = std::env::current_dir()?;
    let manifest = manifest::Manifest::load(&project_dir)?;
    let dist_dir: PathBuf = project_dir.join("dist");
    codegen::emit(&manifest, &dist_dir)?;
    println!(
        "rublocks: built `{}` -> {}",
        manifest.name,
        dist_dir.display()
    );
    Ok(())
}
