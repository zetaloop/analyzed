use std::time::Duration;

use hir::ChangeWithProcMacros;
use ide_db::base_db::ProcMacroPaths;
use project_model::{ProjectWorkspaceKind, WorkspaceBuildScripts};
use stdx::thread::ThreadIntent;
use triomphe::Arc;
use vfs::AbsPathBuf;

use crate::{
    config::Config,
    global_state::{
        FetchBuildDataResponse, FetchWorkspaceRequest, FetchWorkspaceResponse, GlobalState,
    },
    main_loop::Task,
    op_queue::Cause,
    reload::{BuildDataProgress, ProjectWorkspaceProgress},
    shared_analyzer::SharedAnalyzerOperationToken,
};

pub(crate) fn handle_workspace_reload(state: &mut GlobalState, _: ()) -> anyhow::Result<()> {
    let Some((operation, waiter)) = state.shared.begin_reload()? else {
        return Ok(());
    };
    state.proc_macro_clients = Arc::from_iter([]);
    state
        .task_pool
        .handle
        .spawn_with_sender(ThreadIntent::Worker, move |sender| {
            if waiter.wait().is_ok() {
                _ = sender.send(Task::SharedReloadReady(operation));
            }
        });
    Ok(())
}

pub(crate) fn handle_proc_macros_rebuild(state: &mut GlobalState, _: ()) -> anyhow::Result<()> {
    if state.shared.rebuild_registered() {
        state.rebuild_queued = true;
    } else if state.shared.begin_rebuild()? {
        state.prepare_rebuild("rebuild proc macros request");
    }
    Ok(())
}

impl GlobalState {
    fn prepare_rebuild(&mut self, cause: &str) {
        self.proc_macro_clients = Arc::from_iter([]);
        self.rebuild_proc_macros = false;
        self.proc_macro_clients_failed = false;
        self.build_deps_changed = false;

        let operation = self
            .shared
            .rebuild_operation_token()
            .expect("shared rebuild was registered");
        let waiter = self
            .shared
            .rebuild_waiter()
            .expect("shared rebuild was registered");
        self.task_pool
            .handle
            .spawn_with_sender(ThreadIntent::Worker, move |sender| {
                if waiter.wait().is_ok() {
                    _ = sender.send(Task::SharedRebuildReady(operation));
                }
            });
        tracing::debug!(%cause, "queued shared proc-macro rebuild");
    }

    fn start_rebuild(&mut self) {
        self.reload_pending = true;
        self.rebuild_proc_macros = true;
        self.fetch_build_data_queue
            .request_op("rebuild proc macros request".to_owned(), ());
    }

    pub(crate) fn handle_shared_build_data_ready(
        &mut self,
        cause: String,
        operation: SharedAnalyzerOperationToken,
    ) {
        if self.shared.operation_token_matches(&operation) {
            self.fetch_build_data(cause);
        }
    }

    pub(crate) fn handle_shared_proc_macros_ready(
        &mut self,
        cause: String,
        operation: SharedAnalyzerOperationToken,
    ) {
        if self.shared.operation_token_matches(&operation) {
            self.fetch_proc_macros(cause, ChangeWithProcMacros::default(), Vec::new());
        }
    }

    fn start_queued_rebuild(&mut self) {
        if !self.rebuild_queued || self.shared.rebuild_registered() {
            return;
        }
        match self.shared.begin_rebuild() {
            Ok(true) => {
                self.rebuild_queued = false;
                self.prepare_rebuild("queued rebuild proc macros request");
            }
            Ok(false) => {}
            Err(error) => tracing::error!(%error, "failed to queue shared proc-macro rebuild"),
        }
    }

    fn clear_stage_operations(&mut self) {
        self.proc_macro_operation = None;
        self.build_data_operation = None;
    }

    fn finish_shared_operation(&mut self, failed: bool) {
        self.clear_stage_operations();
        self.reload_pending = false;
        self.proc_macro_clients_failed = failed;
        self.start_queued_rebuild();
    }

    fn refresh_proc_macro_clients(&mut self) {
        if !self.reload_pending
            && !self.proc_macro_clients_failed
            && !self.shared.reload_registered()
            && !self.shared.rebuild_registered()
        {
            self.proc_macro_clients = self.shared.proc_macro_clients();
        }
    }

    pub(crate) fn handle_shared_reload_ready(&mut self, operation: SharedAnalyzerOperationToken) {
        if !self.shared.activate_reload(&operation) {
            return;
        }
        self.proc_macro_clients = Arc::from_iter([]);
        self.reload_workspace = true;
        self.reload_pending = true;
        self.proc_macro_clients_failed = false;
        self.build_deps_changed = false;
        let req = FetchWorkspaceRequest {
            path: None,
            force_crate_graph_reload: false,
        };
        self.fetch_workspaces_queue
            .request_op("reload workspace request".to_owned(), req);
    }

    pub(crate) fn handle_shared_rebuild_ready(&mut self, operation: SharedAnalyzerOperationToken) {
        if self.shared.operation_token_matches(&operation) && self.shared.rebuild_registered() {
            self.start_rebuild();
        }
    }

    pub(crate) fn update_configuration(&mut self, config: Config) {
        let world_changed = match (
            crate::shared_analyzer::shared_analyzer_context_from_config(&self.config),
            crate::shared_analyzer::shared_analyzer_context_from_config(&config),
        ) {
            (Ok((old, _)), Ok((new, _))) => old.shared_world != new.shared_world,
            _ => true,
        };
        self._update_configuration(config);

        if world_changed && !self.fetch_workspaces_queue.op_requested() {
            let req = FetchWorkspaceRequest {
                path: None,
                force_crate_graph_reload: false,
            };
            self.fetch_workspaces_queue
                .request_op("analysis database config changed".to_owned(), req);
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
        let reload = self.reload_workspace;
        self.reload_workspace = false;
        let reload_id = reload.then(|| {
            self.workspace_reload_id += 1;
            self.workspace_reload_id
        });

        let shared_context =
            crate::shared_analyzer::shared_analyzer_context_from_config(&self.config);
        let continuation = self.shared.reload_operation_token();
        let can_adopt = continuation.is_none() && !reload && reload_path.is_none();
        let adopted = can_adopt
            && shared_context
                .as_ref()
                .ok()
                .is_some_and(|(key, _)| self.shared.take_workspace_update(key));
        let current_shared = self.shared.downgrade();
        self.task_pool
            .handle
            .spawn_with_sender(ThreadIntent::Worker, move |sender| {
                if sender
                    .send(Task::FetchWorkspace(ProjectWorkspaceProgress::Begin))
                    .is_err()
                {
                    return;
                }
                let fallback = |error| {
                    current_shared
                        .upgrade()
                        .map(|shared| FetchWorkspaceResponse {
                            workspaces: vec![Err(error)],
                            force_crate_graph_reload,
                            shared,
                            reload_id,
                            adopted: false,
                        })
                };
                let response = match shared_context {
                    Ok((key, config)) => {
                        let progress_sender = sender.clone();
                        let progress = Box::new(move |message| {
                            _ = progress_sender.send(Task::FetchWorkspace(
                                ProjectWorkspaceProgress::Report(message),
                            ));
                        });
                        match crate::shared_analyzer::shared_analyzer_registry().register(
                            key,
                            config,
                            reload_path,
                            reload,
                            true,
                            continuation,
                            progress.as_ref(),
                        ) {
                        Ok(session) => match session.workspaces() {
                            Ok((workspaces, build_data_loaded)) => Some(FetchWorkspaceResponse {
                                workspaces,
                                force_crate_graph_reload,
                                shared: session.runtime(),
                                reload_id,
                                adopted: adopted || can_adopt && build_data_loaded,
                            }),
                            Err(error) => fallback(error),
                        },
                            Err(error) => fallback(error),
                        }
                    }
                    Err(error) => fallback(error),
                };
                let Some(response) = response else {
                    return;
                };
                _ = sender.send(Task::FetchedWorkspace(response));
            });
    }

    fn advance_reload(&mut self) {
        if !self.shared.reload_active() {
            return;
        }
        self.reload_pending = true;
        if self.fetch_build_data_queue.op_in_progress()
            || self.fetch_build_data_queue.op_requested()
        {
            return;
        }
        if self.shared.build_data_pending() {
            self.fetch_build_data_queue
                .request_op("workspace reload".to_owned(), ());
            return;
        }
        if self.fetch_proc_macros_queue.op_in_progress()
            || self.fetch_proc_macros_queue.op_requested()
        {
            return;
        }
        if self.shared.proc_macros_pending() {
            self.fetch_proc_macros_queue.request_op(
                "workspace reload".to_owned(),
                (ChangeWithProcMacros::default(), Vec::new()),
            );
            return;
        }
        if self.shared.finish_reload(Ok(())) {
            self.finish_shared_operation(false);
        }
    }

    pub(crate) fn listen_workspace_updates(&self) {
        let updates = self.shared.workspace_updates();
        let sender = self.task_pool.handle.sender.clone();
        let runtime = self.shared.downgrade();
        std::thread::Builder::new()
            .name("workspace-updates".to_owned())
            .spawn(move || {
                while updates.recv().is_ok() {
                    let Some(runtime) = runtime.upgrade() else {
                        return;
                    };
                    if sender.send(Task::WorkspaceUpdated(runtime)).is_err() {
                        return;
                    }
                }
            })
            .expect("failed to spawn workspace update listener");
    }

    pub(crate) fn switch_workspaces(&mut self, cause: Cause) -> Option<Duration> {
        let reload_response = self
            .fetch_workspaces_queue
            .last_op_result()
            .and_then(|response| response.reload_id)
            .is_some_and(|id| self.handled_workspace_reload != Some(id));
        let mut operation_scope_changed = false;
        let reload_error = reload_response
            .then(|| {
                self.fetch_workspaces_queue
                    .last_op_result()
                    .is_some_and(|response| !response.force_crate_graph_reload)
                    .then(|| self.fetch_workspace_error().err())
                    .flatten()
            })
            .flatten();
        if let Some(FetchWorkspaceResponse {
            shared,
            reload_id,
            workspaces,
            adopted,
            ..
        }) = self.fetch_workspaces_queue.last_op_result()
        {
            let runtime_changed = !self.shared.is_same_session(shared);
            operation_scope_changed = !self.shared.operation_scope_matches(shared);
            if operation_scope_changed {
                let rebuild_registered = self.shared.rebuild_registered();
                self.shared
                    .cancel_operations("shared analyzer backend key changed");
                if self.rebuilding_proc_macros.is_some()
                    || self.rebuild_proc_macros
                    || rebuild_registered
                {
                    self.rebuild_queued = true;
                }
                self.rebuilding_proc_macros = None;
                self.rebuild_proc_macros = false;
                self.build_data_rebuild_id = None;
                self.build_data_operation = None;
                self.proc_macro_operation = None;
                self.rebuild_response_current = None;
                self.reload_pending = false;
            } else {
                self.shared.transfer_operations(shared);
            }
            if runtime_changed {
                self.shared.retire();
            }
            self.shared = shared.clone();
            if runtime_changed {
                self.listen_workspace_updates();
            }
            self.workspace_adoption = (*adopted && !self.build_data_adoption)
                .then(|| {
                    workspaces
                        .iter()
                        .map(|workspace| workspace.as_ref().ok().cloned())
                        .collect::<Option<Vec<_>>>()
                        .map(Arc::new)
                })
                .flatten();
            if let Some(id) = *reload_id
                && self.handled_workspace_reload != Some(id)
            {
                self.handled_workspace_reload = Some(id);
                self.reload_pending =
                    workspaces.iter().all(Result::is_ok) && self.shared.reload_active();
                self.proc_macro_clients_failed = workspaces.iter().any(Result::is_err);
                self.proc_macro_clients = Arc::from_iter([]);
            } else if reload_id.is_none() && workspaces.iter().all(Result::is_ok) {
                self.proc_macro_clients_failed = false;
            }
        }
        self.build_data_response_current = self
            .build_data_operation
            .as_ref()
            .is_some_and(|operation| self.shared.operation_token_matches(operation))
            && self
                .fetch_build_data_queue
                .last_op_result()
                .is_some_and(|response| Arc::ptr_eq(&response.workspaces, &self.workspaces));
        self.rebuild_response_current = self.rebuilding_proc_macros.filter(|id| {
            self.fetch_build_data_queue
                .last_op_result()
                .is_some_and(|response| {
                    response.rebuild_id == Some(*id) && self.build_data_response_current
                })
        });
        let stale_build_data = self.fetch_build_data_queue.last_op_result().is_some()
            && !self.build_data_response_current
            && !self.build_data_adoption;
        if stale_build_data && self.rebuilding_proc_macros.is_some() {
            self.set_proc_macro_clients(false);
        }
        if stale_build_data {
            self.fetch_build_data_queue.last_op_result = None;
        }
        if !reload_response {
            self.refresh_proc_macro_clients();
        }
        let result = self._switch_workspaces(cause);
        if self.workspace_adoption.is_some()
            && !self.fetch_build_data_queue.op_requested()
            && !self.fetch_build_data_queue.op_in_progress()
        {
            self.fetch_build_data_queue
                .request_op("shared workspace updated".to_owned(), ());
        }
        if let Some(error) = reload_error {
            if self.shared.finish_reload(Err(anyhow::format_err!(error))) {
                self.finish_shared_operation(true);
            }
            self.rebuild_response_current = None;
            self.build_data_response_current = false;
            self.build_data_adoption = false;
            return result;
        }
        if self.rebuilding_proc_macros.is_some() {
            self.set_proc_macro_clients(false);
        }
        let build_data_response_current = self.build_data_response_current;
        let build_data_operation_current = self
            .build_data_operation
            .as_ref()
            .is_some_and(|operation| self.shared.operation_token_matches(operation));
        if !build_data_response_current
            && build_data_operation_current
            && self.rebuilding_proc_macros.is_none()
            && self.shared.finish_normal_operation(Err(anyhow::format_err!(
                "shared build-data result became stale"
            )))
        {
            self.build_data_operation = None;
            self.fetch_build_data_queue
                .request_op("shared build-data result became stale".to_owned(), ());
        }
        self.rebuild_response_current = None;
        self.build_data_response_current = false;
        self.build_data_adoption = false;
        self.advance_reload();
        if operation_scope_changed {
            self.start_queued_rebuild();
        }
        result
    }

    pub(crate) fn set_proc_macro_clients(&mut self, same_workspaces: bool) {
        let mut build_data_updated = false;
        if same_workspaces
            && self.rebuilding_proc_macros.is_none()
            && self.build_data_response_current
            && let Some(FetchBuildDataResponse {
                rebuild_id: None,
                workspaces,
                build_scripts,
                reload,
            }) = self.fetch_build_data_queue.last_op_result()
        {
            match self.shared.update_build_data(
                workspaces,
                build_scripts,
                self.build_data_generation,
                self.build_data_operation
                    .as_ref()
                    .expect("shared build-data response has an operation"),
            ) {
                Ok(true) => build_data_updated = true,
                Ok(false) => {
                    if !*reload
                        && self.build_data_operation.as_ref().is_some_and(|operation| {
                            self.shared.normal_operation_matches(operation)
                        })
                        && self.shared.finish_normal_operation(Err(anyhow::format_err!(
                            "shared build-data result became stale"
                        )))
                    {
                        self.build_data_operation = None;
                    }
                    self.fetch_build_data_queue
                        .request_op("shared build-data result became stale".to_owned(), ());
                }
                Err(error) => {
                    tracing::error!(%error, "failed to update shared build data");
                    if *reload {
                        if self.shared.finish_reload(Err(error)) {
                            self.finish_shared_operation(true);
                        }
                    } else if self
                        .build_data_operation
                        .as_ref()
                        .is_some_and(|operation| self.shared.normal_operation_matches(operation))
                        && self.shared.finish_normal_operation(Err(error))
                    {
                        self.build_data_operation = None;
                    }
                }
            }
        }
        if build_data_updated
            && same_workspaces
            && self.build_data_response_current
            && let Some(FetchBuildDataResponse {
                rebuild_id: None,
                reload,
                ..
            }) = self.fetch_build_data_queue.last_op_result()
            && *reload
            && (!self.config.expand_proc_macros() || !self.shared.proc_macros_pending())
            && self.shared.finish_reload(Ok(()))
        {
            self.finish_shared_operation(false);
        }

        if let Some(rebuild_id) = self.rebuilding_proc_macros
            && let Some(FetchBuildDataResponse {
                rebuild_id: Some(response_id),
                workspaces,
                build_scripts,
                ..
            }) = self.fetch_build_data_queue.last_op_result()
            && *response_id == rebuild_id
        {
            let current = same_workspaces && self.rebuild_response_current == Some(rebuild_id);
            let result = if current {
                self.shared.update_rebuild_build_data(
                    workspaces,
                    build_scripts,
                    self.build_data_generation,
                    self.build_data_operation
                        .as_ref()
                        .expect("shared build-data response has an operation"),
                )
            } else {
                Ok(false)
            };
            match result {
                Ok(true)
                    if self.config.expand_proc_macros() && self.shared.proc_macros_pending() =>
                {
                    self.fetch_proc_macros_queue.request_op(
                        "proc macros changed".to_owned(),
                        (ChangeWithProcMacros::default(), Vec::new()),
                    );
                }
                Ok(true) => {
                    self.rebuilding_proc_macros = None;
                    self.shared.finish_rebuild(Ok(()));
                    self.finish_shared_operation(false);
                }
                Ok(false) => {
                    self.rebuilding_proc_macros = None;
                    self.rebuild_proc_macros = true;
                    self.reload_pending = true;
                    self.proc_macro_clients_failed = false;
                    self.fetch_build_data_queue
                        .request_op("shared proc-macro result became stale".to_owned(), ());
                }
                Err(error) => {
                    tracing::error!(%error, "failed to rebuild shared proc macros");
                    self.rebuilding_proc_macros = None;
                    self.shared.finish_rebuild(Err(error));
                    self.finish_shared_operation(true);
                }
            }
        }

        if build_data_updated
            && !self.reload_pending
            && !self.shared.proc_macros_pending()
            && self
                .build_data_operation
                .as_ref()
                .is_some_and(|operation| self.shared.normal_operation_matches(operation))
            && self.shared.finish_normal_operation(Ok(()))
        {
            self.build_data_operation = None;
        }
        self.refresh_proc_macro_clients();
    }

    pub(crate) fn recreate_crate_graph_from_shared(
        &mut self,
        cause: String,
        switching_from_empty_workspace: bool,
    ) -> Option<Duration> {
        if let Some(FetchWorkspaceResponse { shared, .. }) =
            self.fetch_workspaces_queue.last_op_result()
        {
            let runtime_changed = !self.shared.is_same_session(shared);
            if self.shared.operation_scope_matches(shared) {
                self.shared.transfer_operations(shared);
            } else {
                self.shared
                    .cancel_operations("shared analyzer backend key changed");
            }
            if runtime_changed {
                self.shared.retire();
            }
            self.shared = shared.clone();
        }
        self.reload_config_from_shared();
        self.recreate_crate_graph(cause, switching_from_empty_workspace)
    }

    pub(crate) fn recreate_crate_graph(
        &mut self,
        cause: String,
        initial_build: bool,
    ) -> Option<Duration> {
        self.detached_files = self
            .workspaces
            .iter()
            .filter_map(|ws| match &ws.kind {
                project_model::ProjectWorkspaceKind::DetachedFile { file, .. } => {
                    Some(file.clone())
                }
                _ => None,
            })
            .collect();
        self.incomplete_crate_graph = false;
        let cancellation_time = self.finish_loading_crate_graph();
        if !initial_build
            && self.config.expand_proc_macros()
            && self.shared.proc_macros_pending()
            && !self.fetch_proc_macros_queue.op_in_progress()
        {
            self.fetch_proc_macros_queue
                .request_op(cause, (ChangeWithProcMacros::default(), Vec::new()));
        }
        cancellation_time
    }

    pub(crate) fn fetch_build_data(&mut self, cause: Cause) {
        if let Some(workspaces) = self.workspace_adoption.take() {
            self.build_data_adoption = true;
            self.workspaces = workspaces;
            self.build_deps_changed = false;
            let workspaces = Arc::clone(&self.workspaces);
            let build_scripts = workspaces
                .iter()
                .map(|workspace| {
                    Ok(match &workspace.kind {
                        ProjectWorkspaceKind::Cargo { build_scripts, .. }
                        | ProjectWorkspaceKind::DetachedFile {
                            cargo: Some((_, build_scripts, _)),
                            ..
                        } => build_scripts.clone(),
                        _ => WorkspaceBuildScripts::default(),
                    })
                })
                .collect();
            self.task_pool.handle.spawn(ThreadIntent::Worker, move || {
                Task::FetchBuildData(BuildDataProgress::End((workspaces, build_scripts)))
            });
            return;
        }
        self.build_data_adoption = false;
        if !self.reload_pending {
            match self.shared.begin_normal_operation() {
                Ok(true) => {}
                Ok(false) => {
                    let operation = self.shared.operation_token();
                    let waiter = self
                        .shared
                        .normal_waiter()
                        .expect("shared normal operation was registered");
                    self.task_pool
                        .handle
                        .spawn_with_sender(ThreadIntent::Worker, move |sender| {
                            if waiter.wait().is_ok() {
                                _ = sender.send(Task::SharedBuildDataReady(cause, operation));
                            }
                        });
                    return;
                }
                Err(error) => {
                    tracing::error!(%error, "failed to register shared build-data operation");
                    return;
                }
            }
        }
        self.build_data_operation = Some(self.shared.operation_token());
        self.build_data_reload = self.reload_pending;
        self.build_data_generation = self.shared.workspace_generation();
        if self.rebuild_proc_macros {
            self.rebuild_proc_macros = false;
            self.proc_macro_rebuild_id += 1;
            self.rebuilding_proc_macros = Some(self.proc_macro_rebuild_id);
            self.build_data_rebuild_id = self.rebuilding_proc_macros;
        } else {
            self.build_data_rebuild_id = None;
        }
        self._fetch_build_data(cause);
    }

    pub(crate) fn handle_shared_proc_macro_progress(
        &mut self,
        progress: crate::shared_analyzer::SharedProcMacroProgress,
    ) -> Option<Duration> {
        let (state, message, cancellation_time) = match progress {
            crate::shared_analyzer::SharedProcMacroProgress::Begin => {
                (crate::lsp::utils::Progress::Begin, None, None)
            }
            crate::shared_analyzer::SharedProcMacroProgress::Report(message) => {
                (crate::lsp::utils::Progress::Report, Some(message), None)
            }
            crate::shared_analyzer::SharedProcMacroProgress::End(response) => {
                self.fetch_proc_macros_queue.op_completed(true);
                let response_current = self
                    .proc_macro_operation
                    .as_ref()
                    .is_some_and(|operation| self.shared.operation_token_matches(operation));
                let mut retry = !response_current;
                if response_current {
                    match self.shared.commit_proc_macro_load(
                        response,
                        self.proc_macro_operation
                            .as_ref()
                            .expect("shared proc-macro response has an operation"),
                    ) {
                        Ok(true) => {
                            if self.rebuilding_proc_macros.take().is_some() {
                                self.shared.finish_rebuild(Ok(()));
                                self.finish_shared_operation(false);
                            } else if self.shared.finish_reload(Ok(())) {
                                self.finish_shared_operation(false);
                            }
                        }
                        Ok(false) => retry = true,
                        Err(error) => {
                            tracing::error!(%error, "failed to commit shared proc macros");
                            let message = format!("{error:#}");
                            if self.rebuilding_proc_macros.take().is_some() {
                                self.shared
                                    .finish_rebuild(Err(anyhow::format_err!("{message}")));
                                self.finish_shared_operation(true);
                            } else if self
                                .shared
                                .finish_reload(Err(anyhow::format_err!("{message}")))
                            {
                                self.finish_shared_operation(true);
                            } else if self
                                .proc_macro_operation
                                .as_ref()
                                .is_some_and(|operation| {
                                    self.shared.normal_operation_matches(operation)
                                })
                                && self.shared.finish_normal_operation(Err(anyhow::format_err!(
                                    "{message}"
                                )))
                            {
                                self.clear_stage_operations();
                            }
                        }
                    }
                }
                self.refresh_proc_macro_clients();
                let cancellation_time = self.finish_loading_crate_graph();
                let normal_operation_current = self
                    .proc_macro_operation
                    .as_ref()
                    .is_some_and(|operation| self.shared.normal_operation_matches(operation));
                if retry {
                    if self.rebuilding_proc_macros.take().is_some() {
                        self.rebuild_proc_macros = true;
                        self.fetch_build_data_queue
                            .request_op("shared proc-macro result became stale".to_owned(), ());
                    } else {
                        if !self.reload_pending
                            && !self.proc_macro_clients_failed
                            && normal_operation_current
                            && self.shared.finish_normal_operation(Err(anyhow::format_err!(
                                "shared proc-macro result became stale"
                            )))
                        {
                            self.clear_stage_operations();
                        }
                        self.advance_reload();
                    }
                    if self.config.expand_proc_macros()
                        && !self.rebuild_proc_macros
                        && self.shared.proc_macros_pending()
                        && !self.fetch_proc_macros_queue.op_requested()
                        && !self.fetch_proc_macros_queue.op_in_progress()
                    {
                        self.fetch_proc_macros_queue.request_op(
                            "shared proc-macro result became stale".to_owned(),
                            (ChangeWithProcMacros::default(), Vec::new()),
                        );
                    } else if !self.reload_pending
                        && !self.proc_macro_clients_failed
                        && !self.shared.proc_macros_pending()
                        && normal_operation_current
                        && self.shared.finish_normal_operation(Ok(()))
                    {
                        self.clear_stage_operations();
                    }
                } else if !self.reload_pending
                    && !self.proc_macro_clients_failed
                    && !self.shared.proc_macros_pending()
                    && normal_operation_current
                    && self.shared.finish_normal_operation(Ok(()))
                {
                    self.clear_stage_operations();
                }
                (crate::lsp::utils::Progress::End, None, cancellation_time)
            }
        };
        self.report_progress("Loading proc-macros", state, message, None, None);
        cancellation_time
    }

    pub(crate) fn fetch_proc_macros(
        &mut self,
        cause: Cause,
        change: ChangeWithProcMacros,
        paths: Vec<ProcMacroPaths>,
    ) {
        let _ = (change, paths);
        tracing::info!(%cause, "will load shared proc macros");
        if !self.reload_pending {
            match self.shared.begin_normal_operation() {
                Ok(true) => {}
                Ok(false) => {
                    let operation = self.shared.operation_token();
                    let waiter = self
                        .shared
                        .normal_waiter()
                        .expect("shared normal operation was registered");
                    self.task_pool
                        .handle
                        .spawn_with_sender(ThreadIntent::Worker, move |sender| {
                            if waiter.wait().is_ok() {
                                _ = sender.send(Task::SharedProcMacrosReady(cause, operation));
                            }
                        });
                    return;
                }
                Err(error) => {
                    tracing::error!(%error, "failed to register shared proc-macro operation");
                    return;
                }
            }
        }
        self.proc_macro_operation = Some(self.shared.operation_token());
        let Some(request) = self.shared.proc_macro_load_request() else {
            self.handle_shared_proc_macro_progress(
                crate::shared_analyzer::SharedProcMacroProgress::Begin,
            );
            self.handle_shared_proc_macro_progress(
                crate::shared_analyzer::SharedProcMacroProgress::End(
                    crate::shared_analyzer::SharedProcMacroLoadResponse {
                        generation: self.shared.workspace_generation(),
                        workspaces: Vec::new(),
                    },
                ),
            );
            return;
        };
        self.task_pool
            .handle
            .spawn_with_sender(ThreadIntent::Worker, move |sender| {
                if sender
                    .send(Task::FetchedProcMacros(
                        crate::shared_analyzer::SharedProcMacroProgress::Begin,
                    ))
                    .is_err()
                {
                    return;
                }
                let progress_sender = sender.clone();
                let response = request.load(move |path| {
                    _ = progress_sender.send(Task::FetchedProcMacros(
                        crate::shared_analyzer::SharedProcMacroProgress::Report(path),
                    ));
                });
                _ = sender.send(Task::FetchedProcMacros(
                    crate::shared_analyzer::SharedProcMacroProgress::End(response),
                ));
            });
    }
}
