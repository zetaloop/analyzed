use std::{
    collections::BTreeSet,
    fs,
    panic::AssertUnwindSafe,
    sync::Once,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender};
use ide::FileId;
use ide_db::{FxHashMap, base_db::SourceRootId};
use lsp_server::{Connection, Message};
use lsp_types::Uri;
use triomphe::Arc;
use vfs::{ChangeKind, VfsPath};

use crate::{
    config::{Config, ConfigChange},
    global_state::FetchWorkspaceRequest,
    line_index::LineEndings,
    shared_analyzer::{SharedAnalyzerRuntime, SharedBaseFileChange},
};

pub(crate) struct Session {
    state: crate::global_state::GlobalState,
}

struct ActiveSession(SharedAnalyzerRuntime);

impl Drop for ActiveSession {
    fn drop(&mut self) {
        self.0.retire();
        self.0.cancel_operations("shared analyzer session exited");
    }
}

impl Session {
    pub(crate) fn new(
        sender: Sender<Message>,
        config: crate::config::Config,
        shared: SharedAnalyzerRuntime,
        workspaces: Vec<project_model::ProjectWorkspace>,
    ) -> Self {
        let state =
            crate::global_state::GlobalState::new_with_shared(sender, config, shared, workspaces);
        state.listen_workspace_updates();
        Self { state }
    }

    pub(crate) fn run_shared(self, receiver: Receiver<Message>) -> anyhow::Result<()> {
        let _active = ActiveSession(self.state.shared.clone());
        self.state.run(receiver)
    }
}

pub(crate) fn run_shared_lsp_session(connection: Connection) -> anyhow::Result<()> {
    crate::session::run_session(connection, crate::session::IoThreads::External, None)
}

pub(crate) fn run_shared_lsp_session_with_config(
    mut config: Config,
    connection: Connection,
) -> anyhow::Result<()> {
    if config.discover_workspace_config().is_none()
        && !config.has_linked_projects()
        && config.detached_files().is_empty()
    {
        config.rediscover_workspaces();
    }

    initialize_rayon();
    let (key, shared_config) =
        crate::shared_analyzer::shared_analyzer_context_from_config(&config)?;
    let session = crate::shared_analyzer::shared_analyzer_registry().register(
        key,
        shared_config,
        None,
        false,
        false,
        None,
        &|_| {},
    )?;
    let shared = session.runtime();
    let workspaces = Vec::new();
    let Connection { sender, receiver } = connection;
    Session::new(sender, config, shared, workspaces).run_shared(receiver)
}

impl crate::global_state::GlobalState {
    pub(crate) fn process_shared_changes(&mut self) -> (bool, Option<Duration>) {
        if !self.reload_pending
            && !self.proc_macro_clients_failed
            && !self.shared.reload_registered()
            && !self.shared.rebuild_registered()
        {
            self.proc_macro_clients = self.shared.proc_macro_clients();
        }
        let shared = self.shared.clone();
        let generation_changed = shared.config_generation_changed();
        let mut modified_ratoml_files = Vec::new();
        let mut workspace_structure_change = None;
        let mut base_file_changes = Vec::new();
        let mut changed = false;
        let mut cancellation_time = None;

        {
            let mut guard = self.vfs.write();
            let changed_files = guard.0.take_changes();
            if !changed_files.is_empty() {
                changed = true;
            }

            let additional_files = self
                .config
                .discover_workspace_config()
                .map(|cfg| {
                    cfg.files_to_watch
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let (vfs, line_endings_map) = &mut *guard;

            for file in changed_files.into_values() {
                let vfs_path = vfs.file_path(file.file_id).clone();
                let file_kind = file.kind();
                let file_exists = file.exists();
                let file_is_created_or_deleted = file.is_created_or_deleted();
                let text = match file.change {
                    vfs::Change::Create(bytes, _) | vfs::Change::Modify(bytes, _) => {
                        String::from_utf8(bytes).ok().map(|text| {
                            let (text, line_endings) = LineEndings::normalize(text);
                            line_endings_map.insert(file.file_id, line_endings);
                            text
                        })
                    }
                    vfs::Change::Delete => None,
                };

                if let Some(("rust-analyzer", Some("toml"))) = vfs_path.name_and_extension() {
                    modified_ratoml_files.push((
                        file_kind,
                        crate::shared_analyzer::normalize_vfs_path(&vfs_path),
                        text.clone(),
                    ));
                }

                if let Some(path) = vfs_path.as_path() {
                    if file_is_created_or_deleted {
                        workspace_structure_change
                            .get_or_insert((path.to_path_buf(), false))
                            .1 |= self.crate_graph_file_dependencies.contains(&vfs_path);
                    } else if crate::reload::should_refresh_for_change(
                        path,
                        file_kind,
                        &additional_files,
                    ) {
                        workspace_structure_change.get_or_insert((path.to_path_buf(), false));
                    }
                }

                if !file_exists && let Ok(Some(file_id)) = shared.vfs_path_to_file_id(&vfs_path) {
                    self.diagnostics.clear_native_for(file_id);
                }

                if !self.mem_docs.contains(&vfs_path)
                    || self.source_root_config.path_is_library(&vfs_path)
                {
                    base_file_changes.push(SharedBaseFileChange {
                        path: vfs_path,
                        line_endings: line_endings_map.get(&file.file_id).copied(),
                        exists: file_exists,
                        text,
                    });
                }
            }
        }

        if let Err(error) = shared.apply_base_file_changes(base_file_changes) {
            tracing::error!("failed to apply shared analyzer base file changes: {error}");
            return (false, None);
        }
        let generation_changed_after_changes = shared.config_generation_changed();
        let force_overlay_rebuild = generation_changed || generation_changed_after_changes;

        if changed || generation_changed {
            let open_files = self
                .mem_docs
                .iter()
                .filter(|path| !self.source_root_config.path_is_library(path))
                .filter_map(|path| {
                    let doc = self.mem_docs.get(path)?;
                    let text = std::str::from_utf8(&doc.data).ok()?.to_owned();
                    let (text, line_endings) = LineEndings::normalize(text);
                    Some((path.clone(), text, line_endings))
                })
                .collect::<Vec<_>>();

            let sync_start = Instant::now();
            let sync = match shared.sync_open_files(open_files, force_overlay_rebuild) {
                Ok(sync) => sync,
                Err(error) => {
                    tracing::error!("failed to sync shared analyzer overlay: {error}");
                    return (false, None);
                }
            };
            cancellation_time = Some(sync_start.elapsed());

            if sync.changed {
                changed = true;
            }
            for file_id in sync.removed_files {
                self.diagnostics.clear_native_for(file_id);
            }
        }

        let config_input_changed = generation_changed || !modified_ratoml_files.is_empty();
        if config_input_changed && !self.workspaces.is_empty() {
            let shared_ratoml_files = {
                let mut files = shared.ratoml_files();
                files.extend(self.mem_docs.iter().filter_map(|path| {
                    if path.name_and_extension() != Some(("rust-analyzer", Some("toml"))) {
                        return None;
                    }

                    let doc = self.mem_docs.get(path)?;
                    let text = std::str::from_utf8(&doc.data).ok()?.to_owned();
                    let (source_root_id, is_library) = shared.source_root_for_path(path)?;
                    let path = crate::shared_analyzer::normalize_vfs_path(path);
                    Some((path, source_root_id, is_library, text))
                }));
                files
            };
            let user_config_path = (|| {
                let mut path = Config::user_config_dir_path()?;
                path.push("rust-analyzer.toml");
                Some(path)
            })();
            let user_config_vfs_path = user_config_path
                .as_ref()
                .map(|path| VfsPath::from(path.clone()));
            let user_config_text = user_config_vfs_path
                .as_ref()
                .and_then(|path| {
                    self.mem_docs
                        .get(path)
                        .and_then(|doc| std::str::from_utf8(&doc.data).ok().map(ToOwned::to_owned))
                })
                .or_else(|| {
                    user_config_path
                        .as_ref()
                        .and_then(|path| fs::read_to_string(path).ok())
                });
            let shared_source_root_parent_map = Arc::new(shared.source_root_parent_map());
            let config_change = self.config_change_from_ratoml(
                modified_ratoml_files,
                shared_ratoml_files,
                user_config_text,
                shared_source_root_parent_map,
            );
            self.apply_config_change(config_change);
            changed = true;
        }
        if changed && !matches!(&workspace_structure_change, Some((.., true))) {
            let modified_rust_files = self
                .mem_docs
                .iter()
                .filter(|path| {
                    path.as_path()
                        .is_some_and(|path| path.extension() == Some("rs"))
                })
                .filter_map(|path| shared.vfs_path_to_file_id(path).ok().flatten())
                .collect::<Vec<_>>();
            if !modified_rust_files.is_empty() {
                _ = self.deferred_task_queue.sender.send(
                    crate::main_loop::DeferredTask::CheckProcMacroSources(modified_rust_files),
                );
            }
        }

        if let Some((path, force_crate_graph_reload)) = workspace_structure_change {
            self.enqueue_workspace_fetch(path, force_crate_graph_reload);
        }

        (changed, cancellation_time)
    }

    pub(crate) fn reload_config_from_shared(&mut self) {
        let shared = &self.shared;
        let shared_ratoml_files = shared.ratoml_files();
        let user_config_path = (|| {
            let mut path = Config::user_config_dir_path()?;
            path.push("rust-analyzer.toml");
            Some(path)
        })();
        let user_config_vfs_path = user_config_path
            .as_ref()
            .map(|path| VfsPath::from(path.clone()));
        let user_config_text = user_config_vfs_path
            .as_ref()
            .and_then(|path| {
                self.mem_docs
                    .get(path)
                    .and_then(|doc| std::str::from_utf8(&doc.data).ok().map(ToOwned::to_owned))
            })
            .or_else(|| {
                user_config_path
                    .as_ref()
                    .and_then(|path| fs::read_to_string(path).ok())
            });
        let shared_source_root_parent_map = Arc::new(shared.source_root_parent_map());
        let config_change = self.config_change_from_ratoml(
            Vec::new(),
            shared_ratoml_files,
            user_config_text,
            shared_source_root_parent_map,
        );
        self.apply_config_change(config_change);
    }

    fn apply_config_change(&mut self, config_change: ConfigChange) {
        let (config, errors, should_update) = self.config.apply_change(config_change);
        self.config_errors = (!errors.is_empty()).then_some(errors);

        if should_update {
            self.update_configuration(config);
        } else {
            self.config = Arc::new(config);
        }
    }

    fn config_change_from_ratoml(
        &self,
        modified_ratoml_files: Vec<(ChangeKind, VfsPath, Option<String>)>,
        shared_ratoml_files: Vec<(VfsPath, SourceRootId, bool, String)>,
        user_config_text: Option<String>,
        shared_source_root_parent_map: Arc<FxHashMap<SourceRootId, SourceRootId>>,
    ) -> ConfigChange {
        let user_config_path = (|| {
            let mut path = Config::user_config_dir_path()?;
            path.push("rust-analyzer.toml");
            Some(path)
        })();
        let user_config_abs_path = user_config_path.as_deref();
        let workspace_ratoml_paths = self
            .workspaces
            .iter()
            .map(|workspace| {
                let path = VfsPath::from({
                    let mut path = workspace.workspace_root().to_owned();
                    path.push("rust-analyzer.toml");
                    path
                });
                crate::shared_analyzer::path_key(&path)
            })
            .collect::<BTreeSet<_>>();
        let mut change = ConfigChange::default();
        let mut ratoml_files = shared_ratoml_files
            .into_iter()
            .map(|(path, source_root_id, is_library, text)| {
                let path = crate::shared_analyzer::normalize_vfs_path(&path);
                (
                    path,
                    source_root_id,
                    is_library,
                    Some(Arc::<str>::from(text)),
                )
            })
            .collect::<Vec<_>>();
        let mut user_config_changed = false;

        for (_kind, vfs_path, text) in modified_ratoml_files {
            let vfs_path = crate::shared_analyzer::normalize_vfs_path(&vfs_path);
            let text = text.map(Arc::<str>::from);
            if vfs_path.as_path() == user_config_abs_path {
                change.change_user_config(text.clone());
                user_config_changed = true;
            }

            let Some((source_root_id, is_library)) = self.shared.source_root_for_path(&vfs_path)
            else {
                continue;
            };
            ratoml_files.push((vfs_path, source_root_id, is_library, text));
        }

        if !user_config_changed && let Some(text) = user_config_text {
            change.change_user_config(Some(Arc::<str>::from(text)));
        }

        for (vfs_path, source_root_id, is_library, text) in ratoml_files {
            let key = crate::shared_analyzer::path_key(&vfs_path);
            let is_workspace_ratoml = workspace_ratoml_paths.contains(&key);
            if is_library {
                continue;
            }

            let entry = if is_workspace_ratoml {
                change.change_workspace_ratoml(source_root_id, vfs_path.clone(), text.clone())
            } else {
                change.change_ratoml(source_root_id, vfs_path.clone(), text.clone())
            };

            if let Some((kind, old_path, old_text)) = entry
                && crate::shared_analyzer::path_key(&old_path)
                    < crate::shared_analyzer::path_key(&vfs_path)
            {
                match kind {
                    crate::config::RatomlFileKind::Crate => {
                        change.change_ratoml(source_root_id, old_path, old_text);
                    }
                    crate::config::RatomlFileKind::Workspace => {
                        change.change_workspace_ratoml(source_root_id, old_path, old_text);
                    }
                }
            }
        }

        change.change_source_root_parent_map(shared_source_root_parent_map);
        change
    }

    pub(crate) fn base_url_to_file_id(&self, url: &Uri) -> anyhow::Result<Option<FileId>> {
        self.shared.base_url_to_file_id(url)
    }

    pub(crate) fn filter_diagnostics(
        &self,
        diagnostics: Vec<lsp_types::Diagnostic>,
    ) -> Vec<lsp_types::Diagnostic> {
        let rustc_diagnostics = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.source.as_deref() == Some("rustc"))
            .map(diagnostic_key)
            .collect::<Vec<_>>();

        diagnostics
            .into_iter()
            .filter(|diagnostic| {
                diagnostic.source.as_deref() != Some("rust-analyzer")
                    || !rustc_diagnostics
                        .iter()
                        .any(|key| *key == diagnostic_key(diagnostic))
            })
            .collect()
    }

    pub(crate) fn publish_changed_diagnostics(&mut self, file_id: FileId) {
        let Some(uri) = self.shared.file_id_to_url(file_id) else {
            return;
        };
        let version = crate::lsp::from_proto::vfs_path(&uri)
            .ok()
            .and_then(|path| self.mem_docs.get(&path).map(|it| it.version));

        let diagnostics = self
            .diagnostics
            .diagnostics_for(file_id)
            .cloned()
            .collect::<Vec<_>>();
        let diagnostics = self.filter_diagnostics(diagnostics);
        self.publish_diagnostics(uri, version, diagnostics);
    }

    pub(crate) fn record_flycheck_diagnostic(
        &mut self,
        id: usize,
        generation: crate::diagnostics::DiagnosticsGeneration,
        package_id: Option<crate::flycheck::PackageSpecifier>,
        diag: crate::diagnostics::flycheck_to_proto::MappedRustDiagnostic,
    ) {
        match self.base_url_to_file_id(&diag.url) {
            Ok(Some(file_id)) => self.diagnostics.add_check_diagnostic(
                id,
                generation,
                &package_id,
                file_id,
                diag.diagnostic,
                diag.fix,
            ),
            Ok(None) => {}
            Err(err) => {
                tracing::error!(
                    "flycheck {id}: File with cargo diagnostic not found in VFS: {err}"
                );
            }
        };
    }

    pub(crate) fn mark_prime_caches_gc(&mut self) {
        crate::shared_analyzer::shared_analyzer_registry().request_gc();
    }

    pub(crate) fn mark_idle_gc(&mut self) {}

    pub(crate) fn handle_event(&mut self, event: super::Event) {
        self.shared.set_busy(true);
        self._handle_event(event);
        let idle = self.is_quiescent()
            && self.task_pool.handle.is_empty()
            && self.fmt_pool.handle.is_empty();
        self.shared.set_busy(!idle);
    }

    pub(crate) fn handle_task(
        &mut self,
        prime_caches_progress: &mut Vec<super::PrimeCachesProgress>,
        task: super::Task,
    ) -> Option<Duration> {
        match task {
            super::Task::FetchedWorkspace(resp) => {
                self.fetch_workspaces_queue.op_completed(resp);
                if let Err(e) = self.fetch_workspace_error() {
                    tracing::error!("FetchWorkspaceError: {e}");
                }
                self.wants_to_switch = Some("fetched workspace".to_owned());
                self.diagnostics.clear_check_all();
                self.report_progress(
                    "Fetching",
                    crate::lsp::utils::Progress::End,
                    None,
                    None,
                    None,
                );
                None
            }
            super::Task::FetchedProcMacros(progress) => {
                self.handle_shared_proc_macro_progress(progress)
            }
            super::Task::SharedReloadReady(operation) => {
                self.handle_shared_reload_ready(operation);
                None
            }
            super::Task::SharedRebuildReady(operation) => {
                self.handle_shared_rebuild_ready(operation);
                None
            }
            super::Task::WorkspaceUpdated(runtime) => {
                if self.shared.is_same_session(&runtime) && self.shared.workspace_update_pending() {
                    self.fetch_workspaces_queue.request_op(
                        "shared workspace updated".to_owned(),
                        FetchWorkspaceRequest {
                            path: None,
                            force_crate_graph_reload: false,
                        },
                    );
                }
                None
            }
            super::Task::SharedBuildDataReady(cause, operation) => {
                self.handle_shared_build_data_ready(cause, operation);
                None
            }
            super::Task::SharedProcMacrosReady(cause, operation) => {
                self.handle_shared_proc_macros_ready(cause, operation);
                None
            }
            super::Task::RetryDeferred(task) => {
                self.deferred_task_queue.sender.send(task).unwrap();
                None
            }
            super::Task::RetryDiscoverTests(subscriptions) => {
                self.spawn_discover_tests(subscriptions);
                None
            }
            _ => {
                let upstream = UpstreamTask::try_from(task)
                    .unwrap_or_else(|_| unreachable!("analyzed task variants handled above"));
                self._handle_task(prime_caches_progress, upstream)
            }
        }
    }

    pub(crate) fn update_diagnostics(&mut self) {
        let generation = self.diagnostics.next_generation();
        let subscriptions: std::sync::Arc<[FileId]> =
            self.workspace_file_ids().into_iter().collect();
        self.spawn_native_diagnostics(generation, subscriptions);
    }

    pub(crate) fn update_tests(&mut self) {
        if !self.vfs_done {
            return;
        }
        let subscriptions = self.workspace_file_ids();
        self.spawn_discover_tests(subscriptions);
    }

    fn workspace_file_ids(&self) -> Vec<FileId> {
        let shared = &self.shared;
        let file_ids = self
            .mem_docs
            .iter()
            .filter_map(|path| shared.vfs_path_to_file_id(path).ok().flatten())
            .collect::<Vec<_>>();
        let mut snapshot = self.snapshot();
        loop {
            let replay = snapshot.replay();
            let result = file_ids
                .iter()
                .copied()
                .filter(|&file_id| {
                    snapshot
                        .analysis
                        .is_library_file(file_id)
                        .is_ok_and(|it| !it)
                })
                .collect();
            if replay.replayable() {
                drop(snapshot);
                if let Some(next) = replay.next() {
                    snapshot = next;
                    continue;
                }
            }
            return result;
        }
    }
}

pub(crate) fn prime_caches(
    analysis: AssertUnwindSafe<crate::shared_analyzer::SharedAnalyzerPendingAnalysis>,
    f: impl FnOnce(AssertUnwindSafe<ide::Analysis>, Sender<super::Task>)
    + Send
    + std::panic::UnwindSafe
    + 'static,
) -> impl FnOnce(Sender<super::Task>) + Send + std::panic::UnwindSafe + 'static {
    pending_analysis(
        analysis,
        super::Task::PrimeCaches(super::PrimeCachesProgress::End { cancelled: true }),
        f,
    )
}

fn pending_analysis(
    analysis: AssertUnwindSafe<crate::shared_analyzer::SharedAnalyzerPendingAnalysis>,
    retry: super::Task,
    f: impl FnOnce(AssertUnwindSafe<ide::Analysis>, Sender<super::Task>)
    + Send
    + std::panic::UnwindSafe
    + 'static,
) -> impl FnOnce(Sender<super::Task>) + Send + std::panic::UnwindSafe + 'static {
    move |sender| {
        let (analysis, snapshot) = analysis.0.activate_snapshot();
        let (pending, tasks) = crossbeam_channel::unbounded();
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            f(AssertUnwindSafe(analysis), pending)
        }));
        if snapshot.replayable() {
            sender.send(retry).unwrap();
        } else if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        } else {
            for task in tasks {
                sender.send(task).unwrap();
            }
        }
    }
}

pub(crate) fn discover_tests(
    snapshot: crate::shared_global_state::PendingGlobalStateSnapshot,
    subscriptions: Vec<FileId>,
    f: impl FnOnce(crate::global_state::GlobalStateSnapshot) -> super::Task
    + Send
    + std::panic::UnwindSafe
    + 'static,
) -> impl FnOnce() -> super::Task + Send + std::panic::UnwindSafe + 'static {
    move || {
        let snapshot = snapshot.activate();
        let replay = snapshot.replay();
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| f(snapshot)));
        if replay.replayable() {
            return super::Task::RetryDiscoverTests(subscriptions);
        }
        match result {
            Ok(task) => task,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
}

pub(crate) fn check_if_indexed(
    snapshot: crate::shared_global_state::PendingGlobalStateSnapshot,
    uri: Uri,
    f: impl FnOnce(crate::global_state::GlobalStateSnapshot, Sender<super::Task>)
    + Send
    + std::panic::UnwindSafe
    + 'static,
) -> impl FnOnce(Sender<super::Task>) + Send + std::panic::UnwindSafe + 'static {
    move |sender| {
        let snapshot = snapshot.activate();
        let replay = snapshot.replay();
        let (pending, tasks) = crossbeam_channel::unbounded();
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| f(snapshot, pending)));
        if replay.replayable() {
            sender
                .send(super::Task::RetryDeferred(
                    super::DeferredTask::CheckIfIndexed(uri),
                ))
                .unwrap();
        } else if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        } else {
            for task in tasks {
                sender.send(task).unwrap();
            }
        }
    }
}

pub(crate) fn check_proc_macro_sources(
    analysis: AssertUnwindSafe<crate::shared_analyzer::SharedAnalyzerPendingAnalysis>,
    modified_rust_files: Vec<FileId>,
    f: impl FnOnce(AssertUnwindSafe<ide::Analysis>, Sender<super::Task>)
    + Send
    + std::panic::UnwindSafe
    + 'static,
) -> impl FnOnce(Sender<super::Task>) + Send + std::panic::UnwindSafe + 'static {
    pending_analysis(
        analysis,
        super::Task::RetryDeferred(super::DeferredTask::CheckProcMacroSources(
            modified_rust_files,
        )),
        f,
    )
}

pub(crate) fn fetch_native_diagnostics(
    pending: &AssertUnwindSafe<&crate::shared_global_state::PendingGlobalStateSnapshot>,
    subscriptions: std::sync::Arc<[FileId]>,
    slice: std::ops::Range<usize>,
    kind: super::NativeDiagnosticsFetchKind,
) -> Vec<(FileId, Vec<lsp_types::Diagnostic>)> {
    let semantic = matches!(kind, super::NativeDiagnosticsFetchKind::Semantic);
    let mut snapshot = pending.0.activate();
    loop {
        let replay = snapshot.replay();
        let diagnostics = crate::diagnostics::_fetch_native_diagnostics(
            &snapshot,
            subscriptions.clone(),
            slice.clone(),
            if semantic {
                super::NativeDiagnosticsFetchKind::Semantic
            } else {
                super::NativeDiagnosticsFetchKind::Syntax
            },
        );
        if !replay.replayable() {
            return diagnostics;
        }
        drop(snapshot);
        let Some(next) = replay.next() else {
            return diagnostics;
        };
        snapshot = next;
    }
}

#[derive(Debug)]
pub(crate) enum UpstreamTask {
    Response(lsp_server::Response),
    DiscoverLinkedProjects(super::DiscoverProjectParam),
    Retry(lsp_server::Request),
    Diagnostics(super::DiagnosticsTaskKind),
    DiscoverTest(crate::lsp_ext::DiscoverTestResults),
    PrimeCaches(super::PrimeCachesProgress),
    FetchWorkspace(crate::reload::ProjectWorkspaceProgress),
    FetchBuildData(crate::reload::BuildDataProgress),
    LoadProcMacros(crate::reload::ProcMacroProgress),
    BuildDepsHaveChanged,
}

impl TryFrom<super::Task> for UpstreamTask {
    type Error = super::Task;

    fn try_from(task: super::Task) -> Result<Self, Self::Error> {
        Ok(match task {
            super::Task::Response(it) => UpstreamTask::Response(it),
            super::Task::DiscoverLinkedProjects(it) => UpstreamTask::DiscoverLinkedProjects(it),
            super::Task::Retry(it) => UpstreamTask::Retry(it),
            super::Task::Diagnostics(it) => UpstreamTask::Diagnostics(it),
            super::Task::DiscoverTest(it) => UpstreamTask::DiscoverTest(it),
            super::Task::PrimeCaches(it) => UpstreamTask::PrimeCaches(it),
            super::Task::FetchWorkspace(it) => UpstreamTask::FetchWorkspace(it),
            super::Task::FetchBuildData(it) => UpstreamTask::FetchBuildData(it),
            super::Task::LoadProcMacros(it) => UpstreamTask::LoadProcMacros(it),
            super::Task::BuildDepsHaveChanged => UpstreamTask::BuildDepsHaveChanged,
            other => return Err(other),
        })
    }
}

fn diagnostic_key(diagnostic: &lsp_types::Diagnostic) -> (lsp_types::Range, Option<String>) {
    let code = diagnostic.code.as_ref().map(|code| match code {
        lsp_types::Code::Int(code) => code.to_string(),
        lsp_types::Code::String(code) => code.clone(),
    });

    (diagnostic.range, code)
}

fn initialize_rayon() {
    static RAYON: Once = Once::new();

    RAYON.call_once(|| {
        _ = rayon::ThreadPoolBuilder::new()
            .thread_name(|index| format!("RayonWorker{index}"))
            .build_global();
    });
}
