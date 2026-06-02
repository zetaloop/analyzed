use std::{env, path::PathBuf};

use directories::ProjectDirs;
use serde::Serialize;

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
}
