use clap::{Parser, Subcommand};

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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::New { name } => anyhow::bail!("`new {name}` not implemented yet"),
        Command::Build => anyhow::bail!("`build` not implemented yet"),
        Command::Run => anyhow::bail!("`run` not implemented yet"),
    }
}
