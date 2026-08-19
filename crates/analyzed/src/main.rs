use std::{
    env,
    ffi::OsString,
    io::{self, BufRead, Read, Write},
    process::{self, ExitCode},
    thread,
};

use analyzed_ipc::{LSP_SESSION_FINISHED, RuntimePaths};
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
        flags::RustAnalyzerCmd::Diagnostics(cmd) => cmd.run(verbosity)?,
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
    _ = thread::spawn(move || {
        let stdin = io::stdin();
        let mut stdin = stdin.lock();
        loop {
            match forward_lsp_frame(&mut stdin, &mut daemon_writer) {
                Ok(true) => {}
                Ok(false) => {
                    _ = daemon_writer.write_all(&[LSP_SESSION_FINISHED, b'\n']);
                    return;
                }
                Err(error) => {
                    eprintln!("analyzed: {error}");
                    process::exit(1);
                }
            }
        }
    });

    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let mut buffer = [0; 8192];
    loop {
        let count = daemon_reader.read(&mut buffer)?;
        if count == 0 {
            anyhow::bail!("shared daemon disconnected while the LSP session was active");
        }

        let finished = buffer[count - 1] == LSP_SESSION_FINISHED;
        stdout.write_all(&buffer[..count - usize::from(finished)])?;
        stdout.flush()?;
        if finished {
            return Ok(());
        }
    }
}

fn forward_lsp_frame(reader: &mut impl BufRead, writer: &mut impl Write) -> anyhow::Result<bool> {
    let mut header = Vec::new();
    loop {
        let start = header.len();
        if reader.read_until(b'\n', &mut header)? == 0 {
            anyhow::ensure!(header.is_empty(), "LSP header ended unexpectedly");
            return Ok(false);
        }
        anyhow::ensure!(header[start..].ends_with(b"\r\n"), "malformed LSP header");
        if header[start..] == *b"\r\n" {
            break;
        }
    }

    let content_length = std::str::from_utf8(&header)?
        .split("\r\n")
        .filter_map(|line| {
            let (name, value) = line.split_once(": ")?;
            name.eq_ignore_ascii_case("Content-Length").then_some(value)
        })
        .last()
        .ok_or_else(|| anyhow::anyhow!("missing Content-Length"))?
        .parse::<u64>()?;
    writer.write_all(&header)?;
    let copied = io::copy(&mut (&mut *reader).take(content_length), writer)?;
    anyhow::ensure!(copied == content_length, "LSP body ended unexpectedly");
    writer.flush()?;
    Ok(true)
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
        if let Err(error) = driver::setup_logging(None) {
            eprintln!("Failed to setup logging: {error:#}");
        }
        analyzed_daemon::run_foreground(paths, startup_lock_owned)?;
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&analyzed_daemon::ensure_daemon(paths)?)?
        );
    }

    Ok(())
}
