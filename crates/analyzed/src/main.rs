use std::{
    env,
    ffi::OsString,
    io::{self, Read, Write},
    process::{self, ExitCode},
    thread,
};

use analyzed_ipc::RuntimePaths;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use ra_ap_rust_analyzer::{cli::flags, config::Config, driver};

#[derive(Debug, Parser)]
#[command(about = "Rust analysis daemon")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Daemon {
        #[arg(long)]
        foreground: bool,
        #[arg(long, hide = true)]
        startup_lock_owned: bool,
    },
    Status,
    Stop,
    #[command(external_subcommand)]
    Upstream(Vec<OsString>),
}

fn main() -> anyhow::Result<ExitCode> {
    if env::var("RA_RUSTC_WRAPPER").is_ok() {
        return driver::main();
    }

    let matches = Cli::command()
        .version(analyzed_daemon::version())
        .get_matches();
    let cli = Cli::from_arg_matches(&matches)?;

    match cli.command {
        Some(Command::Status) => print_status()?,
        Some(Command::Daemon {
            foreground,
            startup_lock_owned,
        }) => {
            run_daemon(foreground, startup_lock_owned)?;
        }
        Some(Command::Stop) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&analyzed_daemon::stop(RuntimePaths::discover()?)?)?
            );
        }
        Some(Command::Upstream(args)) => return run_upstream_cli(args),
        None => run_stdio()?,
    }

    Ok(ExitCode::SUCCESS)
}

fn run_upstream_cli(args: Vec<OsString>) -> anyhow::Result<ExitCode> {
    let flags = match flags::RustAnalyzer::from_vec(args) {
        Ok(flags) => flags,
        Err(err) => err.exit(),
    };

    #[cfg(debug_assertions)]
    if flags.wait_dbg || env::var("RA_WAIT_DBG").is_ok() {
        driver::wait_for_debugger();
    }

    if let Err(e) = driver::setup_logging(flags.log_file.clone()) {
        eprintln!("Failed to setup logging: {e:#}");
    }

    let verbosity = flags.verbosity();

    match flags.subcommand {
        flags::RustAnalyzerCmd::LspServer(cmd) => 'lsp_server: {
            if cmd.print_config_schema {
                println!("{:#}", Config::json_schema());
                break 'lsp_server;
            }
            if cmd.version {
                println!("rust-analyzer {}", ra_ap_rust_analyzer::version());
                break 'lsp_server;
            }
            run_stdio()?;
        }
        flags::RustAnalyzerCmd::Parse(cmd) => cmd.run()?,
        flags::RustAnalyzerCmd::Symbols(cmd) => cmd.run()?,
        flags::RustAnalyzerCmd::Highlight(cmd) => cmd.run()?,
        flags::RustAnalyzerCmd::AnalysisStats(cmd) => cmd.run(verbosity)?,
        flags::RustAnalyzerCmd::Diagnostics(cmd) => cmd.run()?,
        flags::RustAnalyzerCmd::UnresolvedReferences(cmd) => cmd.run()?,
        flags::RustAnalyzerCmd::Ssr(cmd) => cmd.run()?,
        flags::RustAnalyzerCmd::Search(cmd) => cmd.run()?,
        flags::RustAnalyzerCmd::Lsif(cmd) => cmd.run(
            &mut std::io::stdout(),
            Some(project_model::RustLibSource::Discover),
        )?,
        flags::RustAnalyzerCmd::Scip(cmd) => cmd.run()?,
        flags::RustAnalyzerCmd::RunTests(cmd) => cmd.run()?,
        flags::RustAnalyzerCmd::RustcTests(cmd) => cmd.run()?,
        flags::RustAnalyzerCmd::PrimeCaches(cmd) => cmd.run()?,
    }

    Ok(ExitCode::SUCCESS)
}

fn run_stdio() -> anyhow::Result<()> {
    let paths = RuntimePaths::discover()?;
    let mut daemon_reader = analyzed_daemon::connect_lsp_session(paths)?;
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

fn run_daemon(foreground: bool, startup_lock_owned: bool) -> anyhow::Result<()> {
    let paths = RuntimePaths::discover()?;

    if foreground {
        analyzed_daemon::run_foreground(paths, startup_lock_owned)?;
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&analyzed_daemon::ensure_daemon(paths)?)?
        );
    }

    Ok(())
}
