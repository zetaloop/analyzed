use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    os::unix::fs::FileTypeExt,
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
}

#[derive(Clone, Debug, Serialize)]
pub struct RuntimePaths {
    pub runtime_dir: PathBuf,
    pub socket_path: PathBuf,
    pub lock_path: PathBuf,
    pub state_path: PathBuf,
}

impl RuntimePaths {
    pub fn discover() -> Self {
        let runtime_dir = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(env::temp_dir)
            .join("analyzed");
        let state_dir = ProjectDirs::from("dev", "zetaloop", "analyzed")
            .map(|dirs| dirs.data_local_dir().to_path_buf())
            .unwrap_or_else(|| runtime_dir.clone());

        Self {
            socket_path: runtime_dir.join("daemon.sock"),
            lock_path: runtime_dir.join("daemon.lock"),
            state_path: state_dir.join("daemon.json"),
            runtime_dir,
        }
    }

    pub fn ensure_runtime_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.runtime_dir)?;
        Ok(())
    }

    pub fn ensure_state_dir(&self) -> Result<()> {
        if let Some(parent) = self.state_path.parent() {
            fs::create_dir_all(parent)?;
        }

        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HelloRequest {
    pub protocol_version: u32,
    pub client_version: String,
}

impl HelloRequest {
    pub fn current() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub root: String,
    pub manifest: String,
    pub packages: usize,
    pub files: usize,
    pub proc_macro_server: bool,
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
pub struct HelloResponse {
    pub ok: bool,
    pub pid: u32,
    pub protocol_version: u32,
    pub daemon_version: String,
    pub rust_analyzer_version: String,
    pub capabilities: Vec<String>,
    pub state: Option<DaemonSnapshot>,
}

impl HelloResponse {
    pub fn current(pid: u32) -> Self {
        Self {
            ok: true,
            pid,
            protocol_version: PROTOCOL_VERSION,
            daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
            rust_analyzer_version: RUST_ANALYZER_VERSION.to_owned(),
            capabilities: vec!["lsp".to_owned(), "native_query".to_owned()],
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

pub fn connect_hello(paths: &RuntimePaths) -> Result<HelloResponse> {
    let mut stream = UnixStream::connect(&paths.socket_path)?;
    write_json_line(&mut stream, &HelloRequest::current())?;
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
