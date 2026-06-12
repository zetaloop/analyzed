use std::{
    io::{self, Read, Write},
    path::PathBuf,
    process, thread,
};

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
    let mut daemon_reader = analyzed_daemon::connect_lsp_session(paths, PathBuf::from("."))?;
    let mut daemon_writer = daemon_reader.try_clone()?;
    thread::spawn(move || {
        let stdin = io::stdin();
        let mut stdin = stdin.lock();
        _ = io::copy(&mut stdin, &mut daemon_writer);
        process::exit(0);
    });

    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let mut buffer = [0; 8192];
    loop {
        let count = daemon_reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }

        stdout.write_all(&buffer[..count])?;
        stdout.flush()?;
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
