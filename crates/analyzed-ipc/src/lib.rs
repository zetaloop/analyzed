use std::{
    env,
    fs::File,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
};

#[cfg(unix)]
use std::{
    fs,
    os::unix::{
        fs::{FileTypeExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::Path,
};

#[cfg(windows)]
use std::{
    ffi::OsStr,
    io::Read,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    ptr,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{
        ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
        INVALID_HANDLE_VALUE, WAIT_ABANDONED, WAIT_OBJECT_0,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ,
        FILE_GENERIC_WRITE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
    },
    System::{
        IO::{GetOverlappedResult, OVERLAPPED},
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
        },
        Threading::{CreateEventW, CreateMutexW, INFINITE, ReleaseMutex, WaitForSingleObject},
    },
};

pub const PROTOCOL_VERSION: u32 = 1;

pub type Result<T> = std::result::Result<T, IpcError>;

#[cfg(unix)]
pub type IpcListener = UnixListener;

#[cfg(unix)]
pub type IpcStream = UnixStream;

#[cfg(windows)]
pub struct IpcListener {
    pipe_name: String,
    pending: File,
    event: OwnedHandle,
}

// Synchronous named pipe operations serialize on the file object: a thread
// blocked in ReadFile holds up a concurrent WriteFile on the same instance,
// which deadlocks the full duplex LSP stream. All pipe handles are therefore
// opened overlapped, and every stream carries its own event so reads and
// writes proceed independently.
#[cfg(windows)]
pub struct IpcStream {
    file: File,
    event: OwnedHandle,
}

#[cfg(windows)]
impl IpcStream {
    fn new(file: File) -> std::io::Result<Self> {
        Ok(Self {
            file,
            event: create_event()?,
        })
    }

    pub fn try_clone(&self) -> std::io::Result<Self> {
        Self::new(self.file.try_clone()?)
    }

    fn overlapped(&self) -> OVERLAPPED {
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = self.event.as_raw_handle();
        overlapped
    }

    fn finish(&self, overlapped: &mut OVERLAPPED, started: i32) -> std::io::Result<usize> {
        if started == 0 {
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(code) if code == ERROR_IO_PENDING as i32 => {}
                Some(code) if code == ERROR_BROKEN_PIPE as i32 => return Ok(0),
                _ => return Err(error),
            }
        }

        let mut transferred = 0;
        let completed = unsafe {
            GetOverlappedResult(self.file.as_raw_handle(), overlapped, &mut transferred, 1)
        };
        if completed == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                return Ok(0);
            }

            return Err(error);
        }

        Ok(transferred as usize)
    }
}

#[cfg(windows)]
impl Read for IpcStream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let mut overlapped = self.overlapped();
        let started = unsafe {
            ReadFile(
                self.file.as_raw_handle(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                ptr::null_mut(),
                &mut overlapped,
            )
        };

        self.finish(&mut overlapped, started)
    }
}

#[cfg(windows)]
impl Write for IpcStream {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let mut overlapped = self.overlapped();
        let started = unsafe {
            WriteFile(
                self.file.as_raw_handle(),
                buffer.as_ptr(),
                buffer.len() as u32,
                ptr::null_mut(),
                &mut overlapped,
            )
        };

        self.finish(&mut overlapped, started)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
fn create_event() -> std::io::Result<OwnedHandle> {
    let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if event.is_null() {
        return Err(std::io::Error::last_os_error());
    }

    Ok(unsafe { OwnedHandle::from_raw_handle(event) })
}

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    #[error("socket path is occupied by a non-socket file: {0}")]
    OccupiedSocketPath(PathBuf),
    #[error("{0} is not set")]
    MissingEnvironment(&'static str),
    #[error("{0}")]
    Protocol(String),
}

#[cfg(unix)]
#[derive(Clone, Debug, Serialize)]
pub struct RuntimePaths {
    pub runtime_dir: PathBuf,
    pub socket_path: PathBuf,
}

#[cfg(windows)]
#[derive(Clone, Debug, Serialize)]
pub struct RuntimePaths {
    pub pipe_name: String,
    pub startup_mutex_name: String,
}

#[cfg(unix)]
impl RuntimePaths {
    pub fn discover() -> Result<Self> {
        let runtime_dir = runtime_root()?.join("analyzed");

        Ok(Self {
            socket_path: runtime_dir.join("daemon.sock"),
            runtime_dir,
        })
    }

    pub fn ensure_runtime_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.runtime_dir)?;
        fs::set_permissions(&self.runtime_dir, fs::Permissions::from_mode(0o700))?;
        Ok(())
    }
}

#[cfg(windows)]
impl RuntimePaths {
    pub fn discover() -> Result<Self> {
        let username =
            env::var("USERNAME").map_err(|_| IpcError::MissingEnvironment("USERNAME"))?;

        Ok(Self {
            pipe_name: format!(r"\\.\pipe\analyzed.{username}"),
            startup_mutex_name: format!(r"Global\analyzed.{username}.startup"),
        })
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

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct BackendKey {
    pub shared_world: SharedWorldKey,
    pub workspace_view: WorkspaceViewKey,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct SharedWorldKey {
    pub rust_analyzer_version: String,
    pub toolchain: Option<String>,
    pub sysroot: Option<String>,
    pub cargo_target: Option<String>,
    pub config: SharedWorldConfigKey,
    pub load: SharedWorldLoadKey,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct SharedWorldConfigKey {
    pub cargo: CargoConfigKey,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct CargoConfigKey {
    pub all_targets: bool,
    pub features: String,
    pub target: Option<String>,
    pub sysroot: Option<String>,
    pub sysroot_src: Option<String>,
    pub rustc_source: Option<String>,
    pub extra_includes: Vec<String>,
    pub cfg_overrides: String,
    pub wrap_rustc_in_build_scripts: bool,
    pub invocation_strategy: String,
    pub run_build_script_command: String,
    pub extra_args: Vec<String>,
    pub extra_env: Vec<(String, Option<String>)>,
    pub target_dir_config: String,
    pub set_test: bool,
    pub no_deps: bool,
    pub metadata_extra_args: Vec<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct SharedWorldLoadKey {
    pub load_out_dirs_from_check: bool,
    pub proc_macro_server: ProcMacroServerKey,
    pub prefill_caches: bool,
    pub num_worker_threads: u16,
    pub proc_macro_processes: u16,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum ProcMacroServerKey {
    None,
    Sysroot,
    Explicit(String),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct WorkspaceViewKey {
    pub workspace_roots: Vec<String>,
    pub analysis: AnalysisConfigKey,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct AnalysisConfigKey {
    pub initialization_options: Option<String>,
    pub workspace_configuration: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackendSnapshot {
    pub key: BackendKey,
    pub client_sessions: usize,
    pub overlay_sessions: usize,
    pub overlay_files: usize,
    pub workspace_loads: Vec<WorkspaceSnapshot>,
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
    pub backend_sessions: Vec<BackendSnapshot>,
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
    pub fn with_state(state: DaemonSnapshot, rust_analyzer_version: String) -> Self {
        Self {
            ok: true,
            pid: state.pid,
            protocol_version: PROTOCOL_VERSION,
            daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
            rust_analyzer_version,
            capabilities: vec!["lsp".to_owned()],
            state: Some(state),
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

#[cfg(unix)]
pub struct StartupLock {
    _file: File,
}

#[cfg(unix)]
impl StartupLock {
    pub fn acquire(paths: &RuntimePaths) -> Result<Self> {
        paths.ensure_runtime_dir()?;
        let file = File::open(&paths.runtime_dir)?;
        file.lock()?;

        Ok(Self { _file: file })
    }
}

#[cfg(windows)]
pub struct StartupLock {
    mutex: OwnedHandle,
}

#[cfg(windows)]
impl StartupLock {
    pub fn acquire(paths: &RuntimePaths) -> Result<Self> {
        let name = wide_null(&paths.startup_mutex_name);
        let handle = unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }

        let mutex = unsafe { OwnedHandle::from_raw_handle(handle) };
        match unsafe { WaitForSingleObject(mutex.as_raw_handle(), INFINITE) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self { mutex }),
            _ => Err(std::io::Error::last_os_error().into()),
        }
    }
}

#[cfg(windows)]
impl Drop for StartupLock {
    fn drop(&mut self) {
        unsafe { ReleaseMutex(self.mutex.as_raw_handle()) };
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

pub fn connect_lsp_session(paths: &RuntimePaths) -> Result<IpcStream> {
    let mut stream = connect_stream(paths)?;
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
    let mut stream = connect_stream(paths)?;
    write_json_line(&mut stream, request)?;
    read_json_line(&mut stream)
}

#[cfg(unix)]
pub fn bind_listener(paths: &RuntimePaths) -> Result<IpcListener> {
    paths.ensure_runtime_dir()?;
    remove_stale_socket(&paths.socket_path)?;

    Ok(UnixListener::bind(&paths.socket_path)?)
}

#[cfg(windows)]
pub fn bind_listener(paths: &RuntimePaths) -> Result<IpcListener> {
    Ok(IpcListener {
        pending: create_pipe(&paths.pipe_name, true)?,
        pipe_name: paths.pipe_name.clone(),
        event: create_event()?,
    })
}

#[cfg(unix)]
pub fn accept_client(listener: &mut IpcListener) -> Result<IpcStream> {
    let (stream, _) = listener.accept()?;
    Ok(stream)
}

#[cfg(windows)]
pub fn accept_client(listener: &mut IpcListener) -> Result<IpcStream> {
    listener.accept()
}

pub fn read_json_line<T>(stream: &mut IpcStream) -> Result<T>
where
    T: DeserializeOwned,
{
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    Ok(serde_json::from_str(&line)?)
}

pub fn write_json_line<T>(stream: &mut IpcStream, value: &T) -> Result<()>
where
    T: Serialize,
{
    serde_json::to_writer(&mut *stream, value)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    Ok(())
}

#[cfg(unix)]
fn connect_stream(paths: &RuntimePaths) -> Result<IpcStream> {
    Ok(UnixStream::connect(&paths.socket_path)?)
}

#[cfg(windows)]
fn connect_stream(paths: &RuntimePaths) -> Result<IpcStream> {
    connect_pipe(&paths.pipe_name)
}

#[cfg(unix)]
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

#[cfg(target_os = "macos")]
fn runtime_root() -> Result<PathBuf> {
    runtime_env("TMPDIR")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn runtime_root() -> Result<PathBuf> {
    runtime_env("XDG_RUNTIME_DIR")
}

#[cfg(unix)]
fn runtime_env(name: &'static str) -> Result<PathBuf> {
    env::var_os(name)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or(IpcError::MissingEnvironment(name))
}

#[cfg(windows)]
impl IpcListener {
    fn accept(&mut self) -> Result<IpcStream> {
        loop {
            match connect_pending_pipe(&self.pending, &self.event) {
                Ok(()) => {
                    let next = create_pipe(&self.pipe_name, false)?;
                    return Ok(IpcStream::new(std::mem::replace(&mut self.pending, next))?);
                }
                // The client connected and vanished before we picked the
                // instance up. Stand up a replacement first so the pipe name
                // never disappears, then retire the dead instance.
                Err(IpcError::Io(error)) if error.raw_os_error() == Some(ERROR_NO_DATA as i32) => {
                    let next = create_pipe(&self.pipe_name, false)?;
                    self.pending = next;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(windows)]
fn create_pipe(pipe_name: &str, first_instance: bool) -> Result<File> {
    let pipe_name = wide_null(pipe_name);
    let first_instance = if first_instance {
        FILE_FLAG_FIRST_PIPE_INSTANCE
    } else {
        0
    };
    let handle = unsafe {
        CreateNamedPipeW(
            pipe_name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | first_instance,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            65_536,
            65_536,
            0,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().into());
    }

    Ok(unsafe { File::from_raw_handle(handle) })
}

#[cfg(windows)]
fn connect_pending_pipe(file: &File, event: &OwnedHandle) -> Result<()> {
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.hEvent = event.as_raw_handle();

    let connected = unsafe { ConnectNamedPipe(file.as_raw_handle(), &mut overlapped) };
    if connected != 0 {
        return Ok(());
    }

    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(code) if code == ERROR_PIPE_CONNECTED as i32 => Ok(()),
        Some(code) if code == ERROR_IO_PENDING as i32 => {
            let mut transferred = 0;
            let completed = unsafe {
                GetOverlappedResult(file.as_raw_handle(), &overlapped, &mut transferred, 1)
            };
            if completed == 0 {
                return Err(std::io::Error::last_os_error().into());
            }

            Ok(())
        }
        _ => Err(error.into()),
    }
}

#[cfg(windows)]
fn connect_pipe(pipe_name: &str) -> Result<IpcStream> {
    let wide_name = wide_null(pipe_name);
    let deadline = Instant::now() + Duration::from_secs(1);

    loop {
        let handle = unsafe {
            CreateFileW(
                wide_name.as_ptr(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                0,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return Ok(IpcStream::new(unsafe { File::from_raw_handle(handle) })?);
        }

        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_PIPE_BUSY as i32) {
            return Err(error.into());
        }

        // Every instance is momentarily taken; wait for the daemon to stand
        // up the next one. This is the documented client side of the named
        // pipe connect handshake.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(error.into());
        }

        unsafe { WaitNamedPipeW(wide_name.as_ptr(), remaining.as_millis().max(1) as u32) };
    }
}

#[cfg(windows)]
fn wide_null(value: &str) -> Vec<u16> {
    OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
