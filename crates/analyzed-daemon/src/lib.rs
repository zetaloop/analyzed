use analyzed_ipc::RuntimePaths;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct DaemonStatus {
    running: bool,
    pid: Option<u32>,
    client_sessions: usize,
    workspaces: usize,
    paths: RuntimePaths,
    command: Option<PendingCommand>,
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
        client_sessions: 0,
        workspaces: 0,
        paths,
        command: None,
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
