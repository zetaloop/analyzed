use std::path::Path;

use ra_ap_ide::AnalysisHost;
use ra_ap_load_cargo::{LoadCargoConfig, ProcMacroServerChoice, load_workspace};
use ra_ap_proc_macro_api::ProcMacroClient;
use ra_ap_project_model::{CargoConfig, ProjectManifest, ProjectWorkspace};
use ra_ap_vfs::{AbsPathBuf, Vfs};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceSummary {
    pub root: String,
    pub manifest: String,
    pub packages: usize,
    pub files: usize,
    pub proc_macro_server: bool,
}

pub struct LoadedWorkspace {
    summary: WorkspaceSummary,
    _host: AnalysisHost,
    _vfs: Vfs,
    _proc_macro_client: Option<ProcMacroClient>,
}

impl LoadedWorkspace {
    pub fn summary(&self) -> &WorkspaceSummary {
        &self.summary
    }
}

pub fn load_cargo_workspace(root: impl AsRef<Path>) -> anyhow::Result<LoadedWorkspace> {
    let cargo_config = CargoConfig::default();
    let load_config = LoadCargoConfig {
        load_out_dirs_from_check: false,
        with_proc_macro_server: ProcMacroServerChoice::Sysroot,
        prefill_caches: false,
        num_worker_threads: 1,
        proc_macro_processes: 1,
    };
    let root = AbsPathBuf::assert_utf8(std::fs::canonicalize(root)?);
    let manifest = ProjectManifest::discover_single(&root)?;
    let manifest_path = manifest.manifest_path().clone();
    let workspace = ProjectWorkspace::load(manifest, &cargo_config, &|_| {})?;
    let root = workspace.workspace_root().to_string();
    let packages = workspace.n_packages();
    let (db, vfs, proc_macro_client) =
        load_workspace(workspace, &cargo_config.extra_env, &load_config)?;
    let files = vfs.iter().count();
    let host = AnalysisHost::with_database(db);

    Ok(LoadedWorkspace {
        summary: WorkspaceSummary {
            root,
            manifest: manifest_path.to_string(),
            packages,
            files,
            proc_macro_server: proc_macro_client.is_some(),
        },
        _host: host,
        _vfs: vfs,
        _proc_macro_client: proc_macro_client,
    })
}
