use std::{
    env, fs,
    os::unix::net::UnixStream,
    path::Path,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use analyzed_ipc::{
    ClientInfo, DaemonRequest, DaemonResponse, DaemonSnapshot, Hello, ProtocolError,
    RUST_ANALYZER_VERSION, RuntimePaths, StartupLock, Stop, bind_listener, read_json_line,
    write_json_line,
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
    hello: Option<Hello>,
    connection_error: Option<String>,
}

pub fn offline_status(paths: RuntimePaths) -> DaemonStatus {
    DaemonStatus {
        running: false,
        pid: None,
        started_at_unix_seconds: None,
        client_sessions: 0,
        workspaces: 0,
        paths,
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
        hello: Some(hello),
        connection_error: None,
    }
}

pub fn stop(paths: RuntimePaths) -> analyzed_ipc::Result<Stop> {
    analyzed_ipc::request_stop(&paths)
}

pub fn ensure_daemon(
    paths: RuntimePaths,
    workspace_root: impl AsRef<Path>,
) -> anyhow::Result<Hello> {
    if let Ok(hello) = analyzed_ipc::connect_hello(&paths) {
        return Ok(hello);
    }

    let _lock = StartupLock::acquire(&paths)?;

    if let Ok(hello) = analyzed_ipc::connect_hello(&paths) {
        return Ok(hello);
    }

    let mut child = Command::new(env::current_exe()?)
        .arg("daemon")
        .arg("--foreground")
        .arg("--workspace")
        .arg(workspace_root.as_ref())
        .arg("--startup-lock-owned")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    for _ in 0..50 {
        if let Ok(hello) = analyzed_ipc::connect_hello(&paths) {
            return Ok(hello);
        }

        if let Some(status) = child.try_wait()? {
            anyhow::bail!("daemon exited before accepting connections: {status}");
        }

        thread::sleep(std::time::Duration::from_millis(100));
    }

    anyhow::bail!("daemon did not accept connections before the startup timeout")
}

pub fn run_foreground(
    paths: RuntimePaths,
    workspace_root: impl AsRef<Path>,
    startup_lock_owned: bool,
) -> anyhow::Result<()> {
    let workspace_root = workspace_root.as_ref();
    let mut state = ServiceState::load(&[workspace_root])?;
    let stopping = AtomicBool::new(false);
    let listener = {
        let _lock = (!startup_lock_owned)
            .then(|| StartupLock::acquire(&paths))
            .transpose()?;
        paths.ensure_state_dir()?;
        let listener = bind_listener(&paths)?;
        write_state_file(&paths, &state.discovery(&paths))?;
        listener
    };

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

    fn discovery(&self, paths: &RuntimePaths) -> DaemonDiscovery {
        DaemonDiscovery {
            pid: self.pid,
            started_at_unix_seconds: self.started_at_unix_seconds,
            protocol_version: analyzed_ipc::PROTOCOL_VERSION,
            daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
            rust_analyzer_version: RUST_ANALYZER_VERSION.to_owned(),
            socket_path: paths.socket_path.to_string_lossy().into_owned(),
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

#[derive(Serialize)]
struct DaemonDiscovery {
    pid: u32,
    started_at_unix_seconds: u64,
    protocol_version: u32,
    daemon_version: String,
    rust_analyzer_version: String,
    socket_path: String,
}

fn write_state_file(paths: &RuntimePaths, discovery: &DaemonDiscovery) -> anyhow::Result<()> {
    fs::write(&paths.state_path, serde_json::to_vec_pretty(discovery)?)?;
    Ok(())
}
