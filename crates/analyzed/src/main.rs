use std::{io, net::Shutdown, path::PathBuf, thread};

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
        #[arg(long, hide = true)]
        daemonize: bool,
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
            daemonize,
        }) => {
            run_daemon(foreground, workspace, startup_lock_owned, daemonize)?;
        }
        Some(Command::Stop) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&analyzed_daemon::stop(RuntimePaths::discover()?)?)?
            );
        }
        None => run_stdio()?,
    }

    Ok(())
}

fn run_stdio() -> anyhow::Result<()> {
    let paths = RuntimePaths::discover()?;
    let mut daemon_writer = analyzed_daemon::connect_lsp_session(paths, PathBuf::from("."))?;
    let mut daemon_reader = daemon_writer.try_clone()?;
    let _stdin = thread::spawn(move || -> anyhow::Result<()> {
        io::copy(&mut io::stdin(), &mut daemon_writer)?;
        _ = daemon_writer.shutdown(Shutdown::Write);
        Ok(())
    });

    io::copy(&mut daemon_reader, &mut io::stdout())?;

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
    daemonize: bool,
) -> anyhow::Result<()> {
    let paths = RuntimePaths::discover()?;

    if foreground {
        analyzed_daemon::run_foreground(paths, workspace, startup_lock_owned, daemonize)?;
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&analyzed_daemon::ensure_daemon(paths, workspace)?)?
        );
    }

    Ok(())
}
