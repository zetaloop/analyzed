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
    },
    Status,
    Stop,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Status) => print_status()?,
        Some(Command::Daemon { foreground }) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&analyzed_daemon::pending_daemon_status(
                    RuntimePaths::discover(),
                    foreground,
                ))?
            );
        }
        Some(Command::Stop) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&analyzed_daemon::pending_stop_status(
                    RuntimePaths::discover(),
                ))?
            );
        }
        None => println!("analyzed {}", env!("CARGO_PKG_VERSION")),
    }

    Ok(())
}

fn print_status() -> anyhow::Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&analyzed_daemon::offline_status(RuntimePaths::discover()))?
    );

    Ok(())
}
