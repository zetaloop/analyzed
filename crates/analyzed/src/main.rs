use std::path::PathBuf;

use analyzed_ipc::RuntimePaths;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(version, about = "Rust analysis daemon")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Daemon {
        #[arg(long)]
        foreground: bool,
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        #[arg(long, hide = true)]
        startup_lock_owned: bool,
    },
    Status,
    Stop,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Status) => print_status()?,
        Some(Command::Daemon {
            foreground,
            workspace,
            startup_lock_owned,
        }) => {
            run_daemon(foreground, workspace, startup_lock_owned)?;
        }
        Some(Command::Stop) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&analyzed_daemon::stop(RuntimePaths::discover()?)?)?
            );
        }
        None => println!("analyzed {}", env!("CARGO_PKG_VERSION")),
    }

    Ok(())
}

fn print_status() -> anyhow::Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&analyzed_daemon::status(RuntimePaths::discover()?))?
    );

    Ok(())
}

fn run_daemon(
    foreground: bool,
    workspace: PathBuf,
    startup_lock_owned: bool,
) -> anyhow::Result<()> {
    let paths = RuntimePaths::discover()?;

    if foreground {
        analyzed_daemon::run_foreground(paths, workspace, startup_lock_owned)?;
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&analyzed_daemon::ensure_daemon(paths, workspace)?)?
        );
    }

    Ok(())
}
