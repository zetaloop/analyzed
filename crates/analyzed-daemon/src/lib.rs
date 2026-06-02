use std::{
    fs,
    os::unix::net::UnixStream,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use analyzed_ipc::{
    DaemonSnapshot, HelloRequest, HelloResponse, RuntimePaths, StartupLock, bind_listener,
    read_json_line, write_json_line,
};
use analyzed_ra::LoadedWorkspace;
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
    hello: Option<HelloResponse>,
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

pub fn online_status(paths: RuntimePaths, hello: HelloResponse) -> DaemonStatus {
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

pub fn pending_stop_status(paths: RuntimePaths) -> DaemonStatus {
    DaemonStatus {
        command: Some(PendingCommand {
            name: "stop",
            foreground: None,
        }),
        ..offline_status(paths)
    }
}

pub fn run_foreground(paths: RuntimePaths, workspace_root: impl AsRef<Path>) -> anyhow::Result<()> {
    let _lock = StartupLock::acquire(&paths)?;
    paths.ensure_state_dir()?;
    let listener = bind_listener(&paths)?;
    let workspace_root = workspace_root.as_ref();
    let mut state = ServiceState::load(&[workspace_root])?;

    write_state_file(&paths, &state.snapshot())?;

    for stream in listener.incoming() {
        let stream = stream?;
        state.client_sessions += 1;
        let result = handle_client(stream, &state);
        state.client_sessions -= 1;

        if let Err(error) = result {
            eprintln!("{error}");
        }
    }

    Ok(())
}

struct ServiceState {
    pid: u32,
    started_at_unix_seconds: u64,
    client_sessions: usize,
    loaded_workspaces: Vec<LoadedWorkspace>,
}

impl ServiceState {
    fn load(workspace_roots: &[&Path]) -> anyhow::Result<Self> {
        let loaded_workspaces = workspace_roots
            .iter()
            .map(analyzed_ra::load_cargo_workspace)
            .collect::<anyhow::Result<Vec<_>>>()?;

        Ok(Self {
            pid: std::process::id(),
            started_at_unix_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_secs()),
            client_sessions: 0,
            loaded_workspaces,
        })
    }

    fn snapshot(&self) -> DaemonSnapshot {
        DaemonSnapshot {
            pid: self.pid,
            started_at_unix_seconds: self.started_at_unix_seconds,
            client_sessions: self.client_sessions,
            workspaces: self.loaded_workspaces.len(),
            workspace_loads: self
                .loaded_workspaces
                .iter()
                .map(|workspace| {
                    let summary = workspace.summary();
                    analyzed_ipc::WorkspaceSnapshot {
                        root: summary.root.clone(),
                        manifest: summary.manifest.clone(),
                        packages: summary.packages,
                        files: summary.files,
                        proc_macro_server: summary.proc_macro_server,
                    }
                })
                .collect(),
        }
    }
}

fn handle_client(mut stream: UnixStream, state: &ServiceState) -> analyzed_ipc::Result<()> {
    let _request: HelloRequest = read_json_line(&mut stream)?;
    write_json_line(&mut stream, &HelloResponse::with_state(state.snapshot()))?;

    Ok(())
}

fn write_state_file(paths: &RuntimePaths, snapshot: &DaemonSnapshot) -> anyhow::Result<()> {
    fs::write(&paths.state_path, serde_json::to_vec_pretty(snapshot)?)?;
    Ok(())
}
