use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    os::unix::fs::{FileTypeExt, PermissionsExt},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use fs4::FileExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

pub const PROTOCOL_VERSION: u32 = 1;
pub const RUST_ANALYZER_VERSION: &str = "0.0.334";

pub type Result<T> = std::result::Result<T, IpcError>;

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    #[error("socket path is occupied by a non-socket file: {0}")]
    OccupiedSocketPath(PathBuf),
    #[error("runtime directory is unavailable")]
    RuntimeDirUnavailable,
    #[error("{0}")]
    Protocol(String),
}

#[derive(Clone, Debug, Serialize)]
pub struct RuntimePaths {
    pub runtime_dir: PathBuf,
    pub socket_path: PathBuf,
    pub lock_path: PathBuf,
    pub state_path: PathBuf,
}

impl RuntimePaths {
    pub fn discover() -> Result<Self> {
        let runtime_dir = runtime_dir()?.join("analyzed");
        let state_dir = ProjectDirs::from("dev", "zetaloop", "analyzed")
            .map(|dirs| dirs.data_local_dir().to_path_buf())
            .ok_or(IpcError::RuntimeDirUnavailable)?;

        Ok(Self {
            socket_path: runtime_dir.join("daemon.sock"),
            lock_path: runtime_dir.join("daemon.lock"),
            state_path: state_dir.join("daemon.json"),
            runtime_dir,
        })
    }

    pub fn ensure_runtime_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.runtime_dir)?;
        fs::set_permissions(&self.runtime_dir, fs::Permissions::from_mode(0o700))?;
        Ok(())
    }

    pub fn ensure_state_dir(&self) -> Result<()> {
        if let Some(parent) = self.state_path.parent() {
            fs::create_dir_all(parent)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }

        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClientInfo {
    pub protocol_version: u32,
    pub client_version: String,
}

impl ClientInfo {
    pub fn current() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum DaemonRequest {
    Hello(ClientInfo),
    Lsp(ClientInfo),
    Stop(ClientInfo),
}

impl DaemonRequest {
    pub fn hello() -> Self {
        Self::Hello(ClientInfo::current())
    }

    pub fn stop() -> Self {
        Self::Stop(ClientInfo::current())
    }

    pub fn lsp() -> Self {
        Self::Lsp(ClientInfo::current())
    }

    pub fn client_info(&self) -> &ClientInfo {
        match self {
            Self::Hello(client) | Self::Lsp(client) | Self::Stop(client) => client,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub root: String,
    pub manifest: String,
    pub packages: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DaemonSnapshot {
    pub pid: u32,
    pub started_at_unix_seconds: u64,
    pub client_sessions: usize,
    pub workspaces: usize,
    pub workspace_loads: Vec<WorkspaceSnapshot>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hello {
    pub ok: bool,
    pub pid: u32,
    pub protocol_version: u32,
    pub daemon_version: String,
    pub rust_analyzer_version: String,
    pub capabilities: Vec<String>,
    pub state: Option<DaemonSnapshot>,
}

impl Hello {
    pub fn current(pid: u32) -> Self {
        Self {
            ok: true,
            pid,
            protocol_version: PROTOCOL_VERSION,
            daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
            rust_analyzer_version: RUST_ANALYZER_VERSION.to_owned(),
            capabilities: vec!["lsp".to_owned()],
            state: None,
        }
    }

    pub fn with_state(state: DaemonSnapshot) -> Self {
        Self {
            pid: state.pid,
            state: Some(state),
            ..Self::current(0)
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Stop {
    pub accepted: bool,
    pub pid: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LspSession {
    pub accepted: bool,
    pub pid: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProtocolError {
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum DaemonResponse {
    Hello(Hello),
    Lsp(LspSession),
    Stop(Stop),
    Error(ProtocolError),
}

pub struct StartupLock {
    _file: File,
}

impl StartupLock {
    pub fn acquire(paths: &RuntimePaths) -> Result<Self> {
        paths.ensure_runtime_dir()?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&paths.lock_path)?;
        FileExt::lock(&file)?;

        Ok(Self { _file: file })
    }
}

pub fn connect_hello(paths: &RuntimePaths) -> Result<Hello> {
    match request(paths, &DaemonRequest::hello())? {
        DaemonResponse::Hello(hello) => Ok(hello),
        DaemonResponse::Error(error) => Err(IpcError::Protocol(error.message)),
        response => Err(IpcError::Protocol(format!(
            "unexpected daemon response: {response:?}"
        ))),
    }
}

pub fn request_stop(paths: &RuntimePaths) -> Result<Stop> {
    match request(paths, &DaemonRequest::stop())? {
        DaemonResponse::Stop(stop) => Ok(stop),
        DaemonResponse::Error(error) => Err(IpcError::Protocol(error.message)),
        response => Err(IpcError::Protocol(format!(
            "unexpected daemon response: {response:?}"
        ))),
    }
}

pub fn connect_lsp_session(paths: &RuntimePaths) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(&paths.socket_path)?;
    write_json_line(&mut stream, &DaemonRequest::lsp())?;

    match read_json_line(&mut stream)? {
        DaemonResponse::Lsp(session) if session.accepted => Ok(stream),
        DaemonResponse::Lsp(session) => Err(IpcError::Protocol(format!(
            "daemon rejected lsp session for pid {}",
            session.pid
        ))),
        DaemonResponse::Error(error) => Err(IpcError::Protocol(error.message)),
        response => Err(IpcError::Protocol(format!(
            "unexpected daemon response: {response:?}"
        ))),
    }
}

pub fn request(paths: &RuntimePaths, request: &DaemonRequest) -> Result<DaemonResponse> {
    let mut stream = UnixStream::connect(&paths.socket_path)?;
    write_json_line(&mut stream, request)?;
    read_json_line(&mut stream)
}

pub fn bind_listener(paths: &RuntimePaths) -> Result<UnixListener> {
    paths.ensure_runtime_dir()?;
    remove_stale_socket(&paths.socket_path)?;

    Ok(UnixListener::bind(&paths.socket_path)?)
}

pub fn read_json_line<T>(stream: &mut UnixStream) -> Result<T>
where
    T: DeserializeOwned,
{
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    Ok(serde_json::from_str(&line)?)
}

pub fn write_json_line<T>(stream: &mut UnixStream, value: &T) -> Result<()>
where
    T: Serialize,
{
    serde_json::to_writer(&mut *stream, value)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    Ok(())
}

fn remove_stale_socket(path: &Path) -> Result<()> {
    if path.try_exists()? {
        let file_type = fs::symlink_metadata(path)?.file_type();
        if !file_type.is_socket() {
            return Err(IpcError::OccupiedSocketPath(path.to_path_buf()));
        }

        fs::remove_file(path)?;
    }

    Ok(())
}

fn runtime_dir() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        return Ok(path);
    }

    #[cfg(target_os = "macos")]
    {
        return Ok(env::temp_dir());
    }

    #[cfg(target_os = "linux")]
    {
        return ProjectDirs::from("dev", "zetaloop", "analyzed")
            .map(|dirs| dirs.cache_dir().join("run"))
            .ok_or(IpcError::RuntimeDirUnavailable);
    }

    #[allow(unreachable_code)]
    ProjectDirs::from("dev", "zetaloop", "analyzed")
        .map(|dirs| dirs.cache_dir().join("run"))
        .ok_or(IpcError::RuntimeDirUnavailable)
}
