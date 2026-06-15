use hir::ChangeWithProcMacros;
use ide_db::base_db::ProcMacroPaths;
use project_model::WorkspaceBuildScripts;
use stdx::thread::ThreadIntent;
use triomphe::Arc;
use vfs::AbsPathBuf;

use crate::{
    config::Config,
    global_state::{FetchBuildDataResponse, FetchWorkspaceRequest, FetchWorkspaceResponse, GlobalState},
    main_loop::Task,
    op_queue::Cause,
    reload::ProjectWorkspaceProgress,
};

impl GlobalState {
    pub(crate) fn update_configuration(&mut self, config: Config) {
        let _p = tracing::info_span!("GlobalState::update_configuration").entered();
        let old_config = std::mem::replace(&mut self.config, Arc::new(config));
        if self.config.linked_or_discovered_projects() != old_config.linked_or_discovered_projects()
        {
            let req = FetchWorkspaceRequest { path: None, force_crate_graph_reload: false };
            self.fetch_workspaces_queue.request_op("discovered projects changed".to_owned(), req)
        } else if self.config.flycheck(None) != old_config.flycheck(None) {
            self.reload_flycheck();
        }

        if self.config.cargo(None) != old_config.cargo(None) {
            let req = FetchWorkspaceRequest { path: None, force_crate_graph_reload: false };
            self.fetch_workspaces_queue.request_op("cargo config changed".to_owned(), req)
        }

        if self.config.cfg_set_test(None) != old_config.cfg_set_test(None) {
            let req = FetchWorkspaceRequest { path: None, force_crate_graph_reload: false };
            self.fetch_workspaces_queue.request_op("cfg_set_test config changed".to_owned(), req)
        }
    }

    pub(crate) fn fetch_workspaces(
        &mut self,
        cause: Cause,
        path: Option<AbsPathBuf>,
        force_crate_graph_reload: bool,
    ) {
        tracing::info!(%cause, "will fetch workspaces");
        let reload_path = path.clone();

        let provider = self.analyzed_provider.clone();
        let shared_context = crate::analyzed_bridge::shared_analyzer_context_from_config(&self.config);
        let current_shared = self.analyzed_shared.clone();
        self.task_pool.handle.spawn_with_sender(ThreadIntent::Worker, move |sender| {
            if sender.send(Task::FetchWorkspace(ProjectWorkspaceProgress::Begin)).is_err() {
                return;
            }
            let response = match shared_context {
                Ok((key, config)) => provider
                    .resolve_reloading(key, config, reload_path)
                    .and_then(|session| {
                        let analyzed_shared = session.runtime();
                        let workspaces = session.workspaces()?;
                        Ok(FetchWorkspaceResponse {
                            workspaces: workspaces.into_iter().map(Ok).collect(),
                            force_crate_graph_reload,
                            analyzed_shared,
                        })
                    })
                    .unwrap_or_else(|error| FetchWorkspaceResponse {
                        workspaces: vec![Err(error)],
                        force_crate_graph_reload,
                        analyzed_shared: current_shared.clone(),
                    }),
                Err(error) => FetchWorkspaceResponse {
                    workspaces: vec![Err(error)],
                    force_crate_graph_reload,
                    analyzed_shared: current_shared.clone(),
                },
            };
            _ = sender.send(Task::AnalyzedFetchWorkspace(response));
        });
    }

    pub(crate) fn analyzed_install_shared_and_check_msrv(
        &mut self,
    ) -> impl Iterator<Item = String> + use<> {
        if let Some(FetchWorkspaceResponse { analyzed_shared, .. }) =
            self.fetch_workspaces_queue.last_op_result()
        {
            self.analyzed_shared = analyzed_shared.clone();
        }
        self.diagnostics.clear_check_all();
        let messages = self.check_workspaces_msrv().collect::<Vec<_>>();
        messages.into_iter()
    }

    pub(crate) fn analyzed_reload_config_then_recreate_crate_graph(
        &mut self,
        cause: String,
        initial_build: bool,
    ) {
        self.analyzed_reload_config_from_shared();
        self.recreate_crate_graph(cause, initial_build);
    }

    pub(crate) fn recreate_crate_graph(&mut self, cause: String, initial_build: bool) {
        let _ = (cause, initial_build);
        self.detached_files = self
            .workspaces
            .iter()
            .filter_map(|ws| match &ws.kind {
                project_model::ProjectWorkspaceKind::DetachedFile { file, .. } => Some(file.clone()),
                _ => None,
            })
            .collect();
        self.incomplete_crate_graph = false;
        self.finish_loading_crate_graph();
    }

    pub(crate) fn fetch_build_data(&mut self, cause: Cause) {
        let _ = cause;
        let workspaces = Arc::new(self.workspaces.as_ref().clone());
        let response = FetchBuildDataResponse {
            build_scripts: workspaces.iter().map(|_| Ok(WorkspaceBuildScripts::default())).collect(),
            workspaces,
        };
        self.fetch_build_data_queue.op_completed(response);
    }

    pub(crate) fn fetch_proc_macros(
        &mut self,
        cause: Cause,
        change: ChangeWithProcMacros,
        paths: Vec<ProcMacroPaths>,
    ) {
        let _ = (cause, change, paths);
        self.fetch_proc_macros_queue.op_completed(true);
    }
}
