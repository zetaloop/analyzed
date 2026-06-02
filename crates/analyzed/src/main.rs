use std::path::PathBuf;

use analyzed_ipc::RuntimePaths;
use clap::{Parser, Subcommand};
use lsp_server::{Connection, ErrorCode, Message, Response};
use lsp_types::{
    InitializeParams, InitializeResult, ServerCapabilities, ServerInfo, TextDocumentSyncCapability,
    TextDocumentSyncKind,
};

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
    let hello = analyzed_daemon::ensure_daemon(paths, PathBuf::from("."))?;
    let _boundary = analyzed_ra::rust_analyzer_lsp_boundary();
    let (connection, io_threads) = Connection::stdio();
    let capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(
            TextDocumentSyncKind::INCREMENTAL,
        )),
        ..ServerCapabilities::default()
    };
    let initialize_result = InitializeResult {
        capabilities,
        server_info: Some(ServerInfo {
            name: "analyzed".to_owned(),
            version: Some(hello.rust_analyzer_version),
        }),
        offset_encoding: None,
    };
    let initialize_params = connection.initialize(serde_json::to_value(initialize_result)?)?;
    let _params: InitializeParams = serde_json::from_value(initialize_params)?;

    for message in &connection.receiver {
        match message {
            Message::Request(request) => {
                if connection.handle_shutdown(&request)? {
                    break;
                }

                connection.sender.send(Message::Response(Response::new_err(
                    request.id,
                    ErrorCode::MethodNotFound as i32,
                    "method is not implemented by analyzed shim yet".to_owned(),
                )))?;
            }
            Message::Response(_) | Message::Notification(_) => {}
        }
    }

    drop(connection);
    io_threads.join()?;

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
