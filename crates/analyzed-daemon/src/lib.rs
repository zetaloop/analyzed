use std::{
    env, fs,
    io::{BufReader, ErrorKind},
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
    AnalysisConfigKey, BackendKey, BackendSnapshot, CargoConfigKey, ClientInfo, DaemonRequest,
    DaemonResponse, DaemonSnapshot, Hello, LspSession, ProcMacroServerKey, ProtocolError,
    RUST_ANALYZER_VERSION, RuntimePaths, SharedWorldConfigKey, SharedWorldKey, SharedWorldLoadKey,
    StartupLock, Stop, WorkspaceSnapshot, WorkspaceViewKey, bind_listener, read_json_line,
    write_json_line,
};
use crossbeam_channel::unbounded;
use daemonize::Daemonize;
use lsp_server::{Connection, Message};
use ra_ap_rust_analyzer::{SharedAnalyzerProvider, shared_analyzer_registry};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct DaemonStatus {
    running: bool,
    pid: Option<u32>,
    started_at_unix_seconds: Option<u64>,
    client_sessions: usize,
    backend_sessions: Vec<BackendSnapshot>,
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
        backend_sessions: Vec::new(),
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
        backend_sessions: snapshot.map_or_else(Vec::new, |state| state.backend_sessions.clone()),
        workspaces: snapshot.map_or(0, |state| state.workspaces),
        paths,
        hello: Some(hello),
        connection_error: None,
    }
}

pub fn stop(paths: RuntimePaths) -> analyzed_ipc::Result<Stop> {
    analyzed_ipc::request_stop(&paths)
}

pub fn connect_lsp_session(
    paths: RuntimePaths,
    workspace_root: impl AsRef<Path>,
) -> anyhow::Result<UnixStream> {
    ensure_daemon(paths.clone(), workspace_root)?;
    Ok(analyzed_ipc::connect_lsp_session(&paths)?)
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

    let mut command = Command::new(env::current_exe()?);
    command
        .arg("daemon")
        .arg("--foreground")
        .arg("--workspace")
        .arg(workspace_root.as_ref())
        .arg("--startup-lock-owned")
        .arg("--daemonize")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = command.spawn()?;
    let mut child_exited = false;

    for _ in 0..50 {
        if let Ok(hello) = analyzed_ipc::connect_hello(&paths) {
            return Ok(hello);
        }

        if !child_exited && let Some(status) = child.try_wait()? {
            if !status.success() {
                anyhow::bail!("daemon exited before accepting connections: {status}");
            }

            child_exited = true;
        }

        thread::sleep(Duration::from_millis(100));
    }

    anyhow::bail!("daemon did not accept connections before the startup timeout")
}

pub fn run_foreground(
    paths: RuntimePaths,
    workspace_root: impl AsRef<Path>,
    startup_lock_owned: bool,
    daemonize: bool,
) -> anyhow::Result<()> {
    let _workspace_root = workspace_root;

    if daemonize {
        Daemonize::new()
            .working_directory(env::current_dir()?)
            .start()?;
    }

    let runtime = DaemonRuntime::load();
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
                stream.set_nonblocking(false)?;
                let state = Arc::clone(&state);
                sessions.push(thread::spawn(move || {
                    state.client_sessions.fetch_add(1, Ordering::SeqCst);
                    let session_id = state.next_session_id.fetch_add(1, Ordering::SeqCst);
                    let result = handle_client(stream, state.clone(), session_id);
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
    state: Arc<ServiceState>,
}

impl DaemonRuntime {
    fn load() -> Self {
        Self {
            state: Arc::new(ServiceState {
                pid: std::process::id(),
                started_at_unix_seconds: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |duration| duration.as_secs()),
                client_sessions: AtomicUsize::new(0),
                next_session_id: AtomicUsize::new(1),
                stopping: AtomicBool::new(false),
            }),
        }
    }
}

struct ServiceState {
    pid: u32,
    started_at_unix_seconds: u64,
    client_sessions: AtomicUsize,
    next_session_id: AtomicUsize,
    stopping: AtomicBool,
}

impl ServiceState {
    fn snapshot(&self) -> DaemonSnapshot {
        let registry = shared_analyzer_registry();
        let backend_sessions = registry
            .backend_snapshots()
            .into_iter()
            .map(backend_snapshot_from_shared)
            .collect::<Vec<_>>();
        let workspace_loads = registry
            .workspace_loads()
            .into_iter()
            .map(workspace_snapshot_from_shared)
            .collect::<Vec<_>>();

        DaemonSnapshot {
            pid: self.pid,
            started_at_unix_seconds: self.started_at_unix_seconds,
            client_sessions: self.client_sessions.load(Ordering::SeqCst),
            backend_sessions,
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
    state: Arc<ServiceState>,
    session_id: usize,
) -> analyzed_ipc::Result<()> {
    let request: DaemonRequest = read_json_line(&mut stream)?;
    if let Some(error) = validate_client(request.client_info()) {
        write_json_line(&mut stream, &DaemonResponse::Error(error))?;
        return Ok(());
    }

    match request {
        DaemonRequest::Hello(_) => {
            write_json_line(
                &mut stream,
                &DaemonResponse::Hello(Hello::with_state(state.snapshot())),
            )?;
        }
        DaemonRequest::Lsp(_) => {
            write_json_line(
                &mut stream,
                &DaemonResponse::Lsp(LspSession {
                    accepted: true,
                    pid: state.pid,
                }),
            )?;
            handle_lsp_session(stream, state, session_id).map_err(|error| {
                analyzed_ipc::IpcError::Protocol(format!("lsp session failed: {error}"))
            })?;
        }
        DaemonRequest::Stop(_) => {
            state.stopping.store(true, Ordering::SeqCst);
            write_json_line(
                &mut stream,
                &DaemonResponse::Stop(Stop {
                    accepted: true,
                    pid: state.pid,
                }),
            )?;
        }
    }

    Ok(())
}

struct LspStreamThreads {
    reader: JoinHandle<anyhow::Result<()>>,
    writer: JoinHandle<anyhow::Result<()>>,
}

impl LspStreamThreads {
    fn join(self) -> anyhow::Result<()> {
        match (self.reader.join(), self.writer.join()) {
            (Ok(Ok(())), Ok(Ok(()))) => Ok(()),
            (Ok(Err(reader)), Ok(Err(writer))) => anyhow::bail!("{reader}\n{writer}"),
            (Ok(Err(error)), _) | (_, Ok(Err(error))) => Err(error),
            (Err(_), _) | (_, Err(_)) => anyhow::bail!("lsp stream thread panicked"),
        }
    }
}

fn handle_lsp_session(
    stream: UnixStream,
    _state: Arc<ServiceState>,
    _session_id: usize,
) -> anyhow::Result<()> {
    let (connection, threads) = lsp_stream_connection(stream)?;
    let registry = shared_analyzer_registry();
    let provider = SharedAnalyzerProvider::new(move |key, shared_config, reload_path| {
        registry.register(key, shared_config, reload_path)
    });
    let result = ra_ap_rust_analyzer::run_shared_rust_analyzer_lsp_session(connection, provider);
    let join_result = threads.join();

    match (result, join_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(session), Err(threads)) => anyhow::bail!("{session}\n{threads}"),
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
}

fn lsp_stream_connection(stream: UnixStream) -> anyhow::Result<(Connection, LspStreamThreads)> {
    let reader_stream = stream.try_clone()?;
    let mut reader = BufReader::new(reader_stream);
    let initialize = Message::read(&mut reader)?.ok_or_else(|| {
        anyhow::anyhow!("lsp client disconnected before sending initialize request")
    })?;
    validate_initialize(&initialize)?;
    let (writer_sender, writer_receiver) = unbounded::<Message>();
    let (reader_sender, reader_receiver) = unbounded::<Message>();
    reader_sender.send(initialize)?;

    let reader = thread::spawn(move || {
        while let Some(message) = Message::read(&mut reader)? {
            if reader_sender.send(message).is_err() {
                break;
            }
        }

        Ok(())
    });
    let writer = thread::spawn(move || {
        let mut writer = stream;
        for message in writer_receiver {
            message.write(&mut writer)?;
        }

        Ok(())
    });

    Ok((
        Connection {
            sender: writer_sender,
            receiver: reader_receiver,
        },
        LspStreamThreads { reader, writer },
    ))
}

fn validate_initialize(message: &Message) -> anyhow::Result<()> {
    let Message::Request(request) = message else {
        anyhow::bail!("lsp client sent a non-request message before initialize");
    };
    if request.method != "initialize" {
        anyhow::bail!(
            "lsp client sent {} before initialize request",
            request.method
        );
    }

    Ok(())
}

fn backend_snapshot_from_shared(
    snapshot: ra_ap_rust_analyzer::SharedAnalyzerBackendSnapshot,
) -> BackendSnapshot {
    BackendSnapshot {
        key: backend_key_from_shared(snapshot.key),
        client_sessions: snapshot.client_sessions,
        overlay_sessions: snapshot.overlay_sessions,
        overlay_files: snapshot.overlay_files,
        workspace_loads: snapshot
            .workspace_loads
            .into_iter()
            .map(workspace_snapshot_from_shared)
            .collect(),
    }
}

fn workspace_snapshot_from_shared(
    summary: ra_ap_rust_analyzer::WorkspaceSummary,
) -> WorkspaceSnapshot {
    WorkspaceSnapshot {
        root: summary.root,
        manifest: summary.manifest,
        packages: summary.packages,
        files: summary.files,
        proc_macro_server: summary.proc_macro_server,
    }
}

fn backend_key_from_shared(key: ra_ap_rust_analyzer::SharedAnalyzerBackendKey) -> BackendKey {
    BackendKey {
        shared_world: SharedWorldKey {
            rust_analyzer_version: key.shared_world.rust_analyzer_version,
            toolchain: key.shared_world.toolchain,
            sysroot: key.shared_world.sysroot,
            cargo_target: key.shared_world.cargo_target,
            config: SharedWorldConfigKey {
                cargo: cargo_config_key_from_shared(key.shared_world.config.cargo),
            },
            load: load_key_from_shared(key.shared_world.load),
        },
        workspace_view: WorkspaceViewKey {
            workspace_roots: key.workspace_view.workspace_roots,
            analysis: AnalysisConfigKey {
                initialization_options: key.workspace_view.analysis.initialization_options,
                workspace_configuration: key.workspace_view.analysis.workspace_configuration,
            },
        },
    }
}

fn cargo_config_key_from_shared(
    key: ra_ap_rust_analyzer::SharedAnalyzerCargoConfigKey,
) -> CargoConfigKey {
    CargoConfigKey {
        all_targets: key.all_targets,
        features: key.features,
        target: key.target,
        sysroot: key.sysroot,
        sysroot_src: key.sysroot_src,
        rustc_source: key.rustc_source,
        extra_includes: key.extra_includes,
        cfg_overrides: key.cfg_overrides,
        wrap_rustc_in_build_scripts: key.wrap_rustc_in_build_scripts,
        invocation_strategy: key.invocation_strategy,
        run_build_script_command: key.run_build_script_command,
        extra_args: key.extra_args,
        extra_env: key.extra_env,
        target_dir_config: key.target_dir_config,
        set_test: key.set_test,
        no_deps: key.no_deps,
        metadata_extra_args: key.metadata_extra_args,
    }
}

fn load_key_from_shared(key: ra_ap_rust_analyzer::SharedAnalyzerLoadKey) -> SharedWorldLoadKey {
    SharedWorldLoadKey {
        load_out_dirs_from_check: key.load_out_dirs_from_check,
        proc_macro_server: match key.proc_macro_server {
            ra_ap_rust_analyzer::SharedAnalyzerProcMacroServerKey::None => ProcMacroServerKey::None,
            ra_ap_rust_analyzer::SharedAnalyzerProcMacroServerKey::Sysroot => {
                ProcMacroServerKey::Sysroot
            }
            ra_ap_rust_analyzer::SharedAnalyzerProcMacroServerKey::Explicit(path) => {
                ProcMacroServerKey::Explicit(path)
            }
        },
        prefill_caches: key.prefill_caches,
        num_worker_threads: key.num_worker_threads,
        proc_macro_processes: key.proc_macro_processes,
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
