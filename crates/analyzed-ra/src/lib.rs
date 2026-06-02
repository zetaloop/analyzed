use std::path::Path;

use ra_ap_ide::{AnalysisHost, RootDatabase};
use ra_ap_load_cargo::{LoadCargoConfig, ProcMacroServerChoice, load_workspace_into_db};
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
    _vfs: Vfs,
    _proc_macro_client: Option<ProcMacroClient>,
}

impl LoadedWorkspace {
    pub fn summary(&self) -> &WorkspaceSummary {
        &self.summary
    }
}

pub struct AnalysisStore {
    host: AnalysisHost,
    loaded_workspaces: Vec<LoadedWorkspace>,
}

impl AnalysisStore {
    pub fn new() -> Self {
        Self {
            host: AnalysisHost::with_database(RootDatabase::new(None)),
            loaded_workspaces: Vec::new(),
        }
    }

    pub fn load_cargo_workspace(
        &mut self,
        root: impl AsRef<Path>,
    ) -> anyhow::Result<&WorkspaceSummary> {
        let loaded = load_cargo_workspace_into_host(&mut self.host, root)?;
        self.loaded_workspaces.push(loaded);

        Ok(self
            .loaded_workspaces
            .last()
            .expect("workspace was just inserted")
            .summary())
    }

    pub fn workspace_summaries(&self) -> impl Iterator<Item = &WorkspaceSummary> {
        self.loaded_workspaces.iter().map(LoadedWorkspace::summary)
    }
}

impl Default for AnalysisStore {
    fn default() -> Self {
        Self::new()
    }
}

fn load_cargo_workspace_into_host(
    host: &mut AnalysisHost,
    root: impl AsRef<Path>,
) -> anyhow::Result<LoadedWorkspace> {
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
    let db = host.raw_database_mut();
    let (vfs, proc_macro_client) =
        load_workspace_into_db(workspace, &cargo_config.extra_env, &load_config, db)?;
    let files = vfs.iter().count();

    Ok(LoadedWorkspace {
        summary: WorkspaceSummary {
            root,
            manifest: manifest_path.to_string(),
            packages,
            files,
            proc_macro_server: proc_macro_client.is_some(),
        },
        _vfs: vfs,
        _proc_macro_client: proc_macro_client,
    })
}
