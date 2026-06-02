use std::{
    fs,
    os::unix::net::UnixStream,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use analyzed_ipc::{
    ClientInfo, DaemonRequest, DaemonResponse, DaemonSnapshot, Hello, ProtocolError, RuntimePaths,
    StartupLock, Stop, bind_listener, read_json_line, write_json_line,
};
use analyzed_ra::AnalysisStore;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct DaemonStatus {
    running: bool,
    pid: Option<u32>,
    started_at_unix_seconds: Option<u64>,
    client_sessions: usize,
    workspaces: usize,
    paths: RuntimePaths,
    command: Option<PendingCommand>,
    hello: Option<Hello>,
    connection_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PendingCommand {
    name: &'static str,
    foreground: Option<bool>,
}

pub fn offline_status(paths: RuntimePaths) -> DaemonStatus {
    DaemonStatus {
        running: false,
        pid: None,
        started_at_unix_seconds: None,
        client_sessions: 0,
        workspaces: 0,
        paths,
        command: None,
        hello: None,
        connection_error: None,
    }
}

pub fn status(paths: RuntimePaths) -> DaemonStatus {
    match analyzed_ipc::connect_hello(&paths) {
        Ok(hello) => online_status(paths, hello),
        Err(error) => DaemonStatus {
            connection_error: Some(error.to_string()),
            ..offline_status(paths)
        },
    }
}

pub fn online_status(paths: RuntimePaths, hello: Hello) -> DaemonStatus {
    let snapshot = hello.state.as_ref();

    DaemonStatus {
        running: true,
        pid: Some(snapshot.map_or(hello.pid, |state| state.pid)),
        started_at_unix_seconds: snapshot.map(|state| state.started_at_unix_seconds),
        client_sessions: snapshot.map_or(0, |state| state.client_sessions),
        workspaces: snapshot.map_or(0, |state| state.workspaces),
        paths,
        command: None,
        hello: Some(hello),
        connection_error: None,
    }
}

pub fn pending_daemon_status(paths: RuntimePaths, foreground: bool) -> DaemonStatus {
    DaemonStatus {
        command: Some(PendingCommand {
            name: "daemon",
            foreground: Some(foreground),
        }),
        ..offline_status(paths)
    }
}

pub fn stop(paths: RuntimePaths) -> analyzed_ipc::Result<Stop> {
    analyzed_ipc::request_stop(&paths)
}

pub fn run_foreground(paths: RuntimePaths, workspace_root: impl AsRef<Path>) -> anyhow::Result<()> {
    let _lock = StartupLock::acquire(&paths)?;
    paths.ensure_state_dir()?;
    let listener = bind_listener(&paths)?;
    let workspace_root = workspace_root.as_ref();
    let mut state = ServiceState::load(&[workspace_root])?;
    let stopping = AtomicBool::new(false);

    write_state_file(&paths, &state.snapshot())?;

    for stream in listener.incoming() {
        let stream = stream?;
        state.client_sessions += 1;
        let result = handle_client(stream, &state, &stopping);
        state.client_sessions -= 1;

        if let Err(error) = result {
            eprintln!("{error}");
        }

        if stopping.load(Ordering::SeqCst) {
            break;
        }
    }

    Ok(())
}

struct ServiceState {
    pid: u32,
    started_at_unix_seconds: u64,
    client_sessions: usize,
    analysis: AnalysisStore,
}

impl ServiceState {
    fn load(workspace_roots: &[&Path]) -> anyhow::Result<Self> {
        let mut analysis = AnalysisStore::new();

        for root in workspace_roots {
            analysis.load_cargo_workspace(root)?;
        }

        Ok(Self {
            pid: std::process::id(),
            started_at_unix_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_secs()),
            client_sessions: 0,
            analysis,
        })
    }

    fn snapshot(&self) -> DaemonSnapshot {
        let workspace_loads: Vec<_> = self
            .analysis
            .workspace_summaries()
            .map(|summary| analyzed_ipc::WorkspaceSnapshot {
                root: summary.root.clone(),
                manifest: summary.manifest.clone(),
                packages: summary.packages,
                files: summary.files,
                proc_macro_server: summary.proc_macro_server,
            })
            .collect();

        DaemonSnapshot {
            pid: self.pid,
            started_at_unix_seconds: self.started_at_unix_seconds,
            client_sessions: self.client_sessions,
            workspaces: workspace_loads.len(),
            workspace_loads,
        }
    }
}

fn handle_client(
    mut stream: UnixStream,
    state: &ServiceState,
    stopping: &AtomicBool,
) -> analyzed_ipc::Result<()> {
    let request: DaemonRequest = read_json_line(&mut stream)?;
    let response = handle_request(request, state, stopping);
    write_json_line(&mut stream, &response)?;

    Ok(())
}

fn handle_request(
    request: DaemonRequest,
    state: &ServiceState,
    stopping: &AtomicBool,
) -> DaemonResponse {
    if let Some(error) = validate_client(request.client_info()) {
        return DaemonResponse::Error(error);
    }

    match request {
        DaemonRequest::Hello(_) => DaemonResponse::Hello(Hello::with_state(state.snapshot())),
        DaemonRequest::Stop(_) => {
            stopping.store(true, Ordering::SeqCst);
            DaemonResponse::Stop(Stop {
                accepted: true,
                pid: state.pid,
            })
        }
    }
}

fn validate_client(client: &ClientInfo) -> Option<ProtocolError> {
    (client.protocol_version != analyzed_ipc::PROTOCOL_VERSION).then(|| ProtocolError {
        message: format!(
            "unsupported protocol version {}, expected {}",
            client.protocol_version,
            analyzed_ipc::PROTOCOL_VERSION
        ),
    })
}

fn write_state_file(paths: &RuntimePaths, snapshot: &DaemonSnapshot) -> anyhow::Result<()> {
    fs::write(&paths.state_path, serde_json::to_vec_pretty(snapshot)?)?;
    Ok(())
}
