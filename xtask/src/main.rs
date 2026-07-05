use std::path::PathBuf;

use clap::{Parser, Subcommand};
use xshell::Shell;

mod dist;

#[derive(Debug, Parser)]
#[command(about = "Build tasks for analyzed")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Build and package the release artifact for one target into dist/
    Dist {
        /// Checkout of the PGO training crate, required for targets that build with PGO
        #[arg(long, value_name = "DIR")]
        training_dir: Option<PathBuf>,
    },
    /// Print the release job matrix as JSON for the CI workflow
    Matrix,
}

fn main() -> anyhow::Result<()> {
    let sh = Shell::new()?;
    sh.change_dir(dist::project_root());
    match Cli::parse().command {
        Command::Dist { training_dir } => dist::run(&sh, training_dir),
        Command::Matrix => {
            dist::matrix();
            Ok(())
        }
    }
}
