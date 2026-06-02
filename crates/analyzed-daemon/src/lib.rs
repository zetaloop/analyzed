use std::{
    env, fs,
    io::ErrorKind,
    os::unix::net::UnixStream,
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use analyzed_ipc::{
    ClientInfo, DaemonRequest, DaemonResponse, DaemonSnapshot, Hello, ProtocolError,
    RUST_ANALYZER_VERSION, RuntimePaths, StartupLock, Stop, WorkspaceSnapshot, bind_listener,
    read_json_line, write_json_line,
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

        thread::sleep(Duration::from_millis(100));
    }

    anyhow::bail!("daemon did not accept connections before the startup timeout")
}

pub fn run_foreground(
    paths: RuntimePaths,
    workspace_root: impl AsRef<Path>,
    startup_lock_owned: bool,
) -> anyhow::Result<()> {
    let workspace_root = workspace_root.as_ref();
    let runtime = DaemonRuntime::load(&[workspace_root])?;
    let state = Arc::clone(&runtime.state);
    let listener = {
        let _lock = (!startup_lock_owned)
            .then(|| StartupLock::acquire(&paths))
            .transpose()?;
        paths.ensure_state_dir()?;
        let listener = bind_listener(&paths)?;
        write_state_file(&paths, &state.discovery(&paths))?;
        listener
    };
    listener.set_nonblocking(true)?;

    let mut sessions = Vec::new();

    while !state.stopping.load(Ordering::SeqCst) {
        reap_finished_sessions(&mut sessions);

        match listener.accept() {
            Ok((stream, _)) => {
                let state = Arc::clone(&state);
                sessions.push(thread::spawn(move || {
                    state.client_sessions.fetch_add(1, Ordering::SeqCst);
                    let result = handle_client(stream, &state);
                    state.client_sessions.fetch_sub(1, Ordering::SeqCst);

                    if let Err(error) = result {
                        eprintln!("{error}");
                    }
                }));
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error.into()),
        }
    }

    join_client_sessions(sessions);
    drop(runtime);

    Ok(())
}

struct DaemonRuntime {
    _analysis: AnalysisStore,
    state: Arc<ServiceState>,
}

impl DaemonRuntime {
    fn load(workspace_roots: &[&Path]) -> anyhow::Result<Self> {
        let mut analysis = AnalysisStore::new();

        for root in workspace_roots {
            analysis.load_cargo_workspace(root)?;
        }

        let workspace_loads = analysis
            .workspace_summaries()
            .map(|summary| WorkspaceSnapshot {
                root: summary.root.clone(),
                manifest: summary.manifest.clone(),
                packages: summary.packages,
                files: summary.files,
                proc_macro_server: summary.proc_macro_server,
            })
            .collect();

        Ok(Self {
            _analysis: analysis,
            state: Arc::new(ServiceState {
                pid: std::process::id(),
                started_at_unix_seconds: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |duration| duration.as_secs()),
                client_sessions: AtomicUsize::new(0),
                workspace_loads,
                stopping: AtomicBool::new(false),
            }),
        })
    }
}

struct ServiceState {
    pid: u32,
    started_at_unix_seconds: u64,
    client_sessions: AtomicUsize,
    workspace_loads: Vec<WorkspaceSnapshot>,
    stopping: AtomicBool,
}

impl ServiceState {
    fn snapshot(&self) -> DaemonSnapshot {
        let workspace_loads = self.workspace_loads.clone();

        DaemonSnapshot {
            pid: self.pid,
            started_at_unix_seconds: self.started_at_unix_seconds,
            client_sessions: self.client_sessions.load(Ordering::SeqCst),
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

fn handle_client(mut stream: UnixStream, state: &ServiceState) -> analyzed_ipc::Result<()> {
    let request: DaemonRequest = read_json_line(&mut stream)?;
    let response = handle_request(request, state);
    write_json_line(&mut stream, &response)?;

    Ok(())
}

fn handle_request(request: DaemonRequest, state: &ServiceState) -> DaemonResponse {
    if let Some(error) = validate_client(request.client_info()) {
        return DaemonResponse::Error(error);
    }

    match request {
        DaemonRequest::Hello(_) => DaemonResponse::Hello(Hello::with_state(state.snapshot())),
        DaemonRequest::Stop(_) => {
            state.stopping.store(true, Ordering::SeqCst);
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

fn reap_finished_sessions(sessions: &mut Vec<JoinHandle<()>>) {
    let mut index = 0;

    while index < sessions.len() {
        if sessions[index].is_finished() {
            let session = sessions.swap_remove(index);
            let _ = session.join();
        } else {
            index += 1;
        }
    }
}

fn join_client_sessions(sessions: Vec<JoinHandle<()>>) {
    for session in sessions {
        let _ = session.join();
    }
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
