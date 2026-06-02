use analyzed_ipc::{HelloResponse, RuntimePaths};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct DaemonStatus {
    running: bool,
    pid: Option<u32>,
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
    DaemonStatus {
        running: true,
        pid: Some(hello.pid),
        client_sessions: 0,
        workspaces: 0,
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
