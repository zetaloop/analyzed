use std::{
    io::{self, BufRead, BufReader, Write},
    net::Shutdown,
    path::PathBuf,
    thread,
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
    let mut daemon_writer = analyzed_daemon::connect_lsp_session(paths, PathBuf::from("."))?;
    let daemon_reader = daemon_writer.try_clone()?;
    let _stdin = thread::spawn(move || -> anyhow::Result<()> {
        let stdin = io::stdin();
        let mut stdin = BufReader::new(stdin.lock());
        while let Some(frame) = read_lsp_frame(&mut stdin)? {
            daemon_writer.write_all(&frame.bytes)?;
            daemon_writer.flush()?;
            if frame.is_exit_notification() {
                break;
            }
        }
        _ = daemon_writer.shutdown(Shutdown::Write);
        Ok(())
    });

    let mut daemon_reader = BufReader::new(daemon_reader);
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    while let Some(frame) = read_lsp_frame(&mut daemon_reader)? {
        stdout.write_all(&frame.bytes)?;
        stdout.flush()?;
    }

    Ok(())
}

struct LspFrame {
    bytes: Vec<u8>,
    body_offset: usize,
}

impl LspFrame {
    fn is_exit_notification(&self) -> bool {
        let body = &self.bytes[self.body_offset..];
        serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|value| {
                value
                    .get("method")
                    .and_then(serde_json::Value::as_str)
                    .map(|method| method == "exit" && value.get("id").is_none())
            })
            .unwrap_or(false)
    }
}

fn read_lsp_frame(reader: &mut impl BufRead) -> anyhow::Result<Option<LspFrame>> {
    let mut header = Vec::new();
    let mut content_length = None;
    loop {
        let mut line = Vec::new();
        if reader.read_until(b'\n', &mut line)? == 0 {
            if header.is_empty() {
                return Ok(None);
            }
            anyhow::bail!("lsp stream closed mid-header");
        }
        header.extend_from_slice(&line);
        if line == b"\r\n" || line == b"\n" {
            break;
        }
        if let Some(value) = line
            .strip_prefix(b"Content-Length:")
            .or_else(|| line.strip_prefix(b"content-length:"))
        {
            let value = std::str::from_utf8(value)?.trim();
            content_length = Some(value.parse::<usize>()?);
        }
    }

    let content_length =
        content_length.ok_or_else(|| anyhow::anyhow!("lsp frame missing Content-Length"))?;
    let body_offset = header.len();
    let mut bytes = header;
    bytes.resize(body_offset + content_length, 0);
    reader.read_exact(&mut bytes[body_offset..])?;

    Ok(Some(LspFrame { bytes, body_offset }))
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
