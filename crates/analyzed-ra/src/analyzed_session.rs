use std::{env, fs, sync::Once};

use crossbeam_channel::{Receiver, Sender};
use ide::FileId;
use ide_db::{FxHashMap, base_db::{SourceRoot, SourceRootId}};
use lsp_server::{Connection, Message};
use lsp_types::Url;
use paths::Utf8PathBuf;
use triomphe::Arc;
use vfs::{AbsPathBuf, ChangeKind, VfsPath};

use crate::{
    analyzed_bridge::{SharedAnalyzerProvider, SharedAnalyzerRuntime, patch_path_prefix},
    config::{Config, ConfigChange, ConfigErrors},
    from_json, server_capabilities, version,
    global_state::FetchWorkspaceRequest,
    line_index::LineEndings,
};

pub(crate) struct RustAnalyzerSession {
    state: crate::global_state::GlobalState,
}

impl RustAnalyzerSession {
    pub(crate) fn new(
        sender: Sender<Message>,
        config: crate::config::Config,
        provider: SharedAnalyzerProvider,
        analyzed_shared: SharedAnalyzerRuntime,
        analyzed_workspaces: Vec<project_model::ProjectWorkspace>,
    ) -> Self {
        Self {
            state: crate::global_state::GlobalState::new_analyzed(
                sender,
                config,
                provider,
                analyzed_shared,
                analyzed_workspaces,
            ),
        }
    }

    pub(crate) fn run_shared(self, receiver: Receiver<Message>) -> anyhow::Result<()> {
        run_shared_state(self.state, receiver)
    }
}

pub(crate) fn run_shared_lsp_session(
    connection: Connection,
    provider: SharedAnalyzerProvider,
) -> anyhow::Result<()> {
    let (initialize_id, initialize_params) = connection.initialize_start()?;
    tracing::info!("InitializeParams: {}", initialize_params);
    let config = config_from_initialize_params(&connection, &initialize_params)?;
    let initialize_result = lsp_types::InitializeResult {
        capabilities: server_capabilities(&config),
        server_info: Some(lsp_types::ServerInfo {
            name: String::from("rust-analyzer"),
            version: Some(version().to_string()),
        }),
        offset_encoding: None,
    };

    connection.initialize_finish(initialize_id, serde_json::to_value(initialize_result)?)?;

    run_shared_lsp_session_with_config(config, connection, provider)
}

pub(crate) fn run_shared_lsp_session_with_config(
    mut config: Config,
    connection: Connection,
    provider: SharedAnalyzerProvider,
) -> anyhow::Result<()> {
    if config.discover_workspace_config().is_none()
        && !config.has_linked_projects()
        && config.detached_files().is_empty()
    {
        config.rediscover_workspaces();
    }

    initialize_rayon();
    let (key, shared_config) =
        crate::analyzed_bridge::shared_analyzer_context_from_config(&config)?;
    let session = provider.resolve(key, shared_config)?;
    let analyzed_shared = session.runtime();
    let analyzed_workspaces = Vec::new();
    let Connection { sender, receiver } = connection;
    RustAnalyzerSession::new(sender, config, provider, analyzed_shared, analyzed_workspaces)
        .run_shared(receiver)
}

fn run_shared_state(
    mut state: crate::global_state::GlobalState,
    inbox: Receiver<Message>,
) -> anyhow::Result<()> {
    if state.config.did_save_text_document_dynamic_registration() {
        let additional_patterns = state
            .config
            .discover_workspace_config()
            .map(|cfg| cfg.files_to_watch.clone().into_iter())
            .into_iter()
            .flatten()
            .map(|file| format!("**/{file}"));
        state.register_did_save_capability(additional_patterns);
    }

    if state.config.discover_workspace_config().is_none() {
        state.fetch_workspaces_queue.request_op(
            "startup".to_owned(),
            FetchWorkspaceRequest { path: None, force_crate_graph_reload: false },
        );
        if let Some((cause, FetchWorkspaceRequest { path, force_crate_graph_reload })) =
            state.fetch_workspaces_queue.should_start_op()
        {
            state.fetch_workspaces(cause, path, force_crate_graph_reload);
        }
    }
    state.update_status_or_notify();

    while let Ok(event) = state.next_event(&inbox) {
        let Some(event) = event else {
            anyhow::bail!("client exited without proper shutdown sequence");
        };
        if matches!(
            &event,
            super::Event::Lsp(lsp_server::Message::Notification(lsp_server::Notification {
                method,
                ..
            }))
            if method
                == <lsp_types::notification::Exit as lsp_types::notification::Notification>::METHOD
        ) {
            return Ok(());
        }
        state.analyzed_shared.set_busy(true);
        state.handle_event(event);
        let idle =
            state.task_pool.handle.is_empty() && state.fmt_pool.handle.is_empty();
        state.analyzed_shared.set_busy(!idle);
    }

    anyhow::bail!("A receiver has been dropped, something panicked!")
}

impl crate::global_state::GlobalState {
    pub(crate) fn analyzed_process_shared_changes(&mut self) -> bool {
        let shared = self.analyzed_shared.clone();
        let generation_changed = shared.config_generation_changed();
        let mut modified_ratoml_files = Vec::new();
        let mut workspace_structure_change = None;
        let mut changed = false;

        {
            let mut guard = self.vfs.write();
            let changed_files = guard.0.take_changes();
            if !changed_files.is_empty() {
                changed = true;
            }

            let additional_files = self
                .config
                .discover_workspace_config()
                .map(|cfg| cfg.files_to_watch.iter().map(String::as_str).collect::<Vec<_>>())
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

                if let Some(("rust-analyzer", Some("toml"))) =
                    vfs_path.name_and_extension()
                {
                    modified_ratoml_files.push((
                        file_kind,
                        crate::analyzed_bridge::normalize_vfs_path(&vfs_path),
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
                        workspace_structure_change
                            .get_or_insert((path.to_path_buf(), false));
                    }
                }

                if !file_exists
                    && let Ok(Some(file_id)) = shared.vfs_path_to_file_id(&vfs_path)
                {
                    self.diagnostics.clear_native_for(file_id);
                }
            }
        }

        if changed || generation_changed {
            let open_files = self
                .mem_docs
                .iter()
                .filter_map(|path| {
                    let doc = self.mem_docs.get(path)?;
                    let text = std::str::from_utf8(&doc.data).ok()?.to_owned();
                    let (text, line_endings) = LineEndings::normalize(text);
                    Some((path.clone(), text, line_endings))
                })
                .collect::<Vec<_>>();

            let overlay_needed = match shared.overlay_needed(&open_files) {
                Ok(needed) => needed,
                Err(error) => {
                    tracing::error!("failed to check shared analyzer overlay: {error}");
                    return false;
                }
            };
            if overlay_needed {
                let overlay_files = match shared.prepare_overlay_files(open_files) {
                    Ok(files) => files,
                    Err(error) => {
                        tracing::error!(
                            "failed to prepare shared analyzer overlay: {error}"
                        );
                        return false;
                    }
                };
                let sync = match shared.sync_open_files(overlay_files) {
                    Ok(sync) => sync,
                    Err(error) => {
                        tracing::error!("failed to sync shared analyzer overlay: {error}");
                        return false;
                    }
                };

                if sync.changed {
                    changed = true;
                }
                for file_id in sync.removed_files {
                    self.diagnostics.clear_native_for(file_id);
                }
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
                    Some((path.clone(), source_root_id, is_library, text))
                }));
                files
            };
            let user_config_path = (|| {
                let mut path = Config::user_config_dir_path()?;
                path.push("rust-analyzer.toml");
                Some(path)
            })();
            let user_config_vfs_path =
                user_config_path.as_ref().map(|path| VfsPath::from(path.clone()));
            let user_config_text = user_config_vfs_path
                .as_ref()
                .and_then(|path| {
                    self.mem_docs.get(path).and_then(|doc| {
                        std::str::from_utf8(&doc.data).ok().map(ToOwned::to_owned)
                    })
                })
                .or_else(|| {
                    user_config_path
                        .as_ref()
                        .and_then(|path| fs::read_to_string(path).ok())
                });
            let shared_source_root_parent_map = Arc::new(shared.source_root_parent_map());
            let source_roots = {
                let guard = self.vfs.read();
                self.source_root_config.partition(&guard.0)
            };
            let config_change = self.analyzed_config_change_from_ratoml(
                modified_ratoml_files,
                &source_roots,
                shared_ratoml_files,
                user_config_text,
                shared_source_root_parent_map,
            );
            let (config, errors, should_update) = self.config.apply_change(config_change);
            self.config_errors = (!errors.is_empty()).then_some(errors);

            if should_update {
                self.update_configuration(config);
            } else {
                self.config = Arc::new(config);
            }
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
                _ = self
                    .deferred_task_queue
                    .sender
                    .send(crate::main_loop::DeferredTask::CheckProcMacroSources(modified_rust_files));
            }
        }

        if let Some((path, force_crate_graph_reload)) = workspace_structure_change {
            self.enqueue_workspace_fetch(path, force_crate_graph_reload);
        }

        changed
    }

    pub(crate) fn analyzed_reload_config_from_shared(&mut self) {
        let shared = &self.analyzed_shared;
        let shared_ratoml_files = shared.ratoml_files();
        let user_config_path = (|| {
            let mut path = Config::user_config_dir_path()?;
            path.push("rust-analyzer.toml");
            Some(path)
        })();
        let user_config_vfs_path = user_config_path.as_ref().map(|path| VfsPath::from(path.clone()));
        let user_config_text = user_config_vfs_path
            .as_ref()
            .and_then(|path| {
                self.mem_docs.get(path).and_then(|doc| {
                    std::str::from_utf8(&doc.data).ok().map(ToOwned::to_owned)
                })
            })
            .or_else(|| user_config_path.as_ref().and_then(|path| fs::read_to_string(path).ok()));
        let shared_source_root_parent_map = Arc::new(shared.source_root_parent_map());
        let source_roots = {
            let guard = self.vfs.read();
            self.source_root_config.partition(&guard.0)
        };
        let config_change = self.analyzed_config_change_from_ratoml(
            Vec::new(),
            &source_roots,
            shared_ratoml_files,
            user_config_text,
            shared_source_root_parent_map,
        );
        let (config, errors, should_update) = self.config.apply_change(config_change);
        self.config_errors = (!errors.is_empty()).then_some(errors);

        if should_update {
            self.update_configuration(config);
        } else {
            self.config = Arc::new(config);
        }
    }

    fn analyzed_config_change_from_ratoml(
        &self,
        modified_ratoml_files: Vec<(ChangeKind, VfsPath, Option<String>)>,
        source_roots: &[SourceRoot],
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
                VfsPath::from({
                    let mut path = workspace.workspace_root().to_owned();
                    path.push("rust-analyzer.toml");
                    path
                })
            })
            .collect::<Vec<_>>();
        let mut change = ConfigChange::default();
        let mut ratoml_files = shared_ratoml_files
            .into_iter()
            .map(|(path, source_root_id, is_library, text)| {
                (path, source_root_id, is_library, Some(Arc::<str>::from(text)))
            })
            .collect::<Vec<_>>();
        let mut user_config_changed = false;

        for (_kind, vfs_path, text) in modified_ratoml_files {
            let text = text.map(Arc::<str>::from);
            if vfs_path.as_path() == user_config_abs_path {
                change.change_user_config(text.clone());
                user_config_changed = true;
            }

            let Some((source_root_id, source_root)) =
                source_root_for_path(source_roots, &vfs_path)
            else {
                continue;
            };
            ratoml_files.push((vfs_path, source_root_id, source_root.is_library, text));
        }

        if !user_config_changed
            && let Some(text) = user_config_text
        {
            change.change_user_config(Some(Arc::<str>::from(text)));
        }

        for (vfs_path, source_root_id, is_library, text) in ratoml_files {
            if is_library {
                continue;
            }

            let entry = if workspace_ratoml_paths.contains(&vfs_path) {
                change.change_workspace_ratoml(source_root_id, vfs_path.clone(), text.clone())
            } else {
                change.change_ratoml(source_root_id, vfs_path.clone(), text.clone())
            };

            if let Some((kind, old_path, old_text)) = entry
                && old_path < vfs_path
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

    pub(crate) fn analyzed_base_url_to_file_id(
        &self,
        url: &Url,
    ) -> anyhow::Result<Option<FileId>> {
        self.analyzed_shared.base_url_to_file_id(url)
    }

    pub(crate) fn analyzed_filter_diagnostics(
        &self,
        diagnostics: Vec<lsp_types::Diagnostic>,
    ) -> Vec<lsp_types::Diagnostic> {
        let rustc_diagnostics = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.source.as_deref() == Some("rustc"))
            .map(analyzed_diagnostic_key)
            .collect::<Vec<_>>();

        diagnostics
            .into_iter()
            .filter(|diagnostic| {
                diagnostic.source.as_deref() != Some("rust-analyzer")
                    || !rustc_diagnostics
                        .iter()
                        .any(|key| *key == analyzed_diagnostic_key(diagnostic))
            })
            .collect()
    }

    pub(crate) fn publish_changed_diagnostics(&mut self, file_id: FileId) {
        let Some(uri) = self.analyzed_shared.file_id_to_url(file_id) else {
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
        let diagnostics = self.analyzed_filter_diagnostics(diagnostics);
        self.publish_diagnostics(uri, version, diagnostics);
    }

    pub(crate) fn record_flycheck_diagnostic(
        &mut self,
        id: usize,
        generation: crate::diagnostics::DiagnosticsGeneration,
        package_id: &Option<crate::flycheck::PackageSpecifier>,
        diag: crate::diagnostics::flycheck_to_proto::MappedRustDiagnostic,
    ) {
        match self.analyzed_base_url_to_file_id(&diag.url) {
            Ok(Some(file_id)) => self.diagnostics.add_check_diagnostic(
                id,
                generation,
                package_id,
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
        crate::analyzed_bridge::shared_analyzer_registry().mark_gc_dirty();
    }

    pub(crate) fn mark_gc_when_idle(&mut self) {
        if self.task_pool.handle.is_empty() && self.fmt_pool.handle.is_empty() {
            crate::analyzed_bridge::shared_analyzer_registry().mark_gc_dirty();
        }
    }

    pub(crate) fn handle_event(&mut self, event: super::Event) {
        self._handle_event(event)
    }

    pub(crate) fn handle_task(
        &mut self,
        prime_caches_progress: &mut Vec<super::PrimeCachesProgress>,
        task: super::Task,
    ) {
        match task {
            super::Task::AnalyzedFetchWorkspace(resp) => {
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
            }
            super::Task::AnalyzedRunFlycheck(path) => {
                crate::handlers::notification::run_flycheck(self, path);
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
        let subscriptions: std::sync::Arc<[FileId]> = self
            .analyzed_workspace_file_ids()
            .into_iter()
            .collect();
        self.spawn_native_diagnostics(generation, subscriptions);
    }

    pub(crate) fn update_tests(&mut self) {
        if !self.vfs_done {
            return;
        }
        let subscriptions = self.analyzed_workspace_file_ids();
        self.spawn_discover_tests(subscriptions);
    }

    fn analyzed_workspace_file_ids(&self) -> Vec<FileId> {
        let shared = &self.analyzed_shared;
        let file_ids = self
            .mem_docs
            .iter()
            .filter_map(|path| shared.vfs_path_to_file_id(path).ok().flatten())
            .collect::<Vec<_>>();
        let snap = self.snapshot();
        file_ids
            .into_iter()
            .filter(|&file_id| {
                snap.analysis
                    .is_library_file(file_id)
                    .is_ok_and(|is_library| !is_library)
            })
            .collect()
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

fn analyzed_diagnostic_key(
    diagnostic: &lsp_types::Diagnostic,
) -> (lsp_types::Range, Option<String>) {
    let code = diagnostic.code.as_ref().map(|code| match code {
        lsp_types::NumberOrString::Number(code) => code.to_string(),
        lsp_types::NumberOrString::String(code) => code.clone(),
    });

    (diagnostic.range, code)
}

fn source_root_for_path<'a>(
    source_roots: &'a [SourceRoot],
    path: &VfsPath,
) -> Option<(SourceRootId, &'a SourceRoot)> {
    source_roots.iter().enumerate().find_map(|(index, source_root)| {
        source_root
            .file_for_path(path)
            .map(|_| (SourceRootId(index as u32), source_root))
    })
}

fn config_from_initialize_params(
    connection: &Connection,
    initialize_params: &serde_json::Value,
) -> anyhow::Result<Config> {
    let lsp_types::InitializeParams {
        root_uri,
        mut capabilities,
        workspace_folders,
        initialization_options,
        client_info,
        ..
    } = from_json::<lsp_types::InitializeParams>("InitializeParams", initialize_params)?;

    if let Some(value) = initialize_params.pointer("/capabilities/workspace/diagnostics")
        && let Ok(diagnostics) =
            from_json::<lsp_types::DiagnosticWorkspaceClientCapabilities>(
                "DiagnosticWorkspaceClientCapabilities",
                value,
            )
    {
        capabilities.workspace.get_or_insert_default().diagnostic.get_or_insert(diagnostics);
    }

    let root_path = match root_uri
        .and_then(|it| it.to_file_path().ok())
        .map(patch_path_prefix)
        .and_then(|it| Utf8PathBuf::from_path_buf(it).ok())
        .and_then(|it| AbsPathBuf::try_from(it).ok())
    {
        Some(it) => it,
        None => AbsPathBuf::assert_utf8(env::current_dir()?),
    };

    if let Some(client_info) = &client_info {
        tracing::info!(
            "Client '{}' {}",
            client_info.name,
            client_info.version.as_deref().unwrap_or_default()
        );
    }

    let workspace_roots = workspace_folders
        .map(|workspaces| {
            workspaces
                .into_iter()
                .filter_map(|it| it.uri.to_file_path().ok())
                .map(patch_path_prefix)
                .filter_map(|it| Utf8PathBuf::from_path_buf(it).ok())
                .filter_map(|it| AbsPathBuf::try_from(it).ok())
                .collect::<Vec<_>>()
        })
        .filter(|workspaces| !workspaces.is_empty())
        .unwrap_or_else(|| vec![root_path.clone()]);
    let mut config = Config::new(root_path, capabilities, workspace_roots, client_info);

    if let Some(json) = initialization_options {
        let mut change = ConfigChange::default();
        change.change_client_config(json);

        let errors: ConfigErrors;
        (config, errors, _) = config.apply_change(change);

        if !errors.is_empty() {
            let notification = lsp_server::Notification::new(
                <lsp_types::notification::ShowMessage as lsp_types::notification::Notification>::METHOD.to_owned(),
                lsp_types::ShowMessageParams {
                    typ: lsp_types::MessageType::WARNING,
                    message: errors.to_string(),
                },
            );
            connection.sender.send(lsp_server::Message::Notification(notification))?;
        }
    }

    Ok(config)
}

fn initialize_rayon() {
    static RAYON: Once = Once::new();

    RAYON.call_once(|| {
        _ = rayon::ThreadPoolBuilder::new()
            .thread_name(|index| format!("RayonWorker{index}"))
            .build_global();
    });
}
