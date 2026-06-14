//! The context or environment in which the language server functions. In our
//! server implementation this is know as the `WorldState`.
//!
//! Each tick provides an immutable snapshot of the state as `WorldSnapshot`.

use std::{
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender, unbounded};
use hir::ChangeWithProcMacros;
use ide::{Analysis, Cancellable, FileId, SourceRootId};
use ide_db::{
    MiniCore,
    base_db::{Crate, ProcMacroPaths},
};
use load_cargo::SourceRootConfig;
use lsp_types::{SemanticTokens, Url};
use parking_lot::{Mutex, RwLock};
use proc_macro_api::ProcMacroClient;
use project_model::{ManifestPath, ProjectWorkspace, ProjectWorkspaceKind, WorkspaceBuildScripts};
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::{Level, span};
use triomphe::Arc;
use vfs::{AbsPathBuf, AnchoredPathBuf, VfsPath};

use crate::{
    config::{Config, ConfigErrors},
    diagnostics::{CheckFixes, DiagnosticCollection},
    discover,
    flycheck::{FlycheckHandle, FlycheckMessage, PackageSpecifier},
    line_index::{LineEndings, LineIndex},
    lsp::{from_proto, to_proto::url_from_abs_path},
    lsp_ext,
    main_loop::Task,
    mem_docs::MemDocs,
    op_queue::{Cause, OpQueue},
    target_spec::{CargoTargetSpec, ProjectJsonTargetSpec, TargetSpec},
    task_pool::{DeferredTaskQueue, TaskPool},
    test_runner::{CargoTestHandle, CargoTestMessage},
};

#[derive(Debug)]
pub(crate) struct FetchWorkspaceRequest {
    pub(crate) path: Option<AbsPathBuf>,
    pub(crate) force_crate_graph_reload: bool,
}

#[derive(Debug)]
pub(crate) struct FetchWorkspaceResponse {
    pub(crate) workspaces: Vec<anyhow::Result<ProjectWorkspace>>,
    pub(crate) force_crate_graph_reload: bool,
    pub(crate) analyzed_shared: crate::analyzed_bridge::SharedAnalyzerRuntime,
}

pub(crate) struct FetchBuildDataResponse {
    pub(crate) workspaces: Arc<Vec<ProjectWorkspace>>,
    pub(crate) build_scripts: Vec<anyhow::Result<WorkspaceBuildScripts>>,
}

// Enforces drop order
pub(crate) struct Handle<H, C> {
    pub(crate) handle: H,
    pub(crate) receiver: C,
}

pub(crate) type ReqHandler = fn(&mut GlobalState, lsp_server::Response);
type ReqQueue = lsp_server::ReqQueue<(String, Instant), ReqHandler>;

/// `GlobalState` is the primary mutable state of the language server
///
/// The most interesting components are `vfs`, which stores a consistent
/// snapshot of the file systems, and `analyzed_shared`, which stores our
/// shared salsa database handle.
///
/// Note that this struct has more than one impl in various modules!
#[doc(alias = "GlobalMess")]
pub(crate) struct GlobalState {
    sender: Sender<lsp_server::Message>,
    req_queue: ReqQueue,

    pub(crate) task_pool: Handle<TaskPool<Task>, Receiver<Task>>,
    pub(crate) fmt_pool: Handle<TaskPool<Task>, Receiver<Task>>,

    pub(crate) config: Arc<Config>,
    pub(crate) config_errors: Option<ConfigErrors>,
    pub(crate) analyzed_provider: crate::analyzed_bridge::SharedAnalyzerProvider,
    pub(crate) analyzed_shared: crate::analyzed_bridge::SharedAnalyzerRuntime,
    pub(crate) diagnostics: DiagnosticCollection,
    pub(crate) mem_docs: MemDocs,
    pub(crate) source_root_config: SourceRootConfig,
    /// A mapping that maps a local source root's `SourceRootId` to it parent's `SourceRootId`, if it has one.
    pub(crate) local_roots_parent_map: Arc<FxHashMap<SourceRootId, SourceRootId>>,
    pub(crate) semantic_tokens_cache: Arc<Mutex<FxHashMap<Url, SemanticTokens>>>,

    // status
    pub(crate) shutdown_requested: bool,
    pub(crate) last_reported_status: lsp_ext::ServerStatusParams,

    // proc macros
    pub(crate) proc_macro_clients: Arc<[Option<anyhow::Result<ProcMacroClient>>]>,
    pub(crate) build_deps_changed: bool,

    // Flycheck
    pub(crate) flycheck: Arc<[FlycheckHandle]>,
    pub(crate) flycheck_sender: Sender<FlycheckMessage>,
    pub(crate) flycheck_receiver: Receiver<FlycheckMessage>,
    pub(crate) last_flycheck_error: Option<String>,
    pub(crate) flycheck_formatted_commands: Vec<String>,

    // Test explorer
    pub(crate) test_run_session: Option<Vec<CargoTestHandle>>,
    pub(crate) test_run_sender: Sender<CargoTestMessage>,
    pub(crate) test_run_receiver: Receiver<CargoTestMessage>,
    pub(crate) test_run_remaining_jobs: usize,

    // Project loading
    pub(crate) discover_handles: Vec<discover::DiscoverHandle>,
    pub(crate) discover_sender: Sender<discover::DiscoverProjectMessage>,
    pub(crate) discover_receiver: Receiver<discover::DiscoverProjectMessage>,
    pub(crate) discover_jobs_active: u32,

    // Debouncing channel for fetching the workspace
    // we want to delay it until the VFS looks stable-ish (and thus is not currently in the middle
    // of a VCS operation like `git switch`)
    pub(crate) fetch_ws_receiver: Option<(Receiver<Instant>, FetchWorkspaceRequest)>,

    // VFS
    pub(crate) loader: Handle<Box<dyn vfs::loader::Handle>, Receiver<vfs::loader::Message>>,
    pub(crate) vfs: Arc<RwLock<(vfs::Vfs, FxHashMap<FileId, LineEndings>)>>,
    pub(crate) vfs_config_version: u32,
    pub(crate) vfs_progress_config_version: u32,
    pub(crate) vfs_done: bool,
    // used to track how long VFS loading takes. this can't be on `vfs::loader::Handle`,
    // as that handle's lifetime is the same as `GlobalState` itself.
    pub(crate) vfs_span: Option<tracing::span::EnteredSpan>,
    pub(crate) wants_to_switch: Option<Cause>,

    /// `workspaces` field stores the data we actually use, while the `OpQueue`
    /// stores the result of the last fetch.
    ///
    /// If the fetch (partially) fails, we do not update the current value.
    ///
    /// The handling of build data is subtle. We fetch workspace in two phases:
    ///
    /// *First*, we run `cargo metadata`, which gives us fast results for
    /// initial analysis.
    ///
    /// *Second*, we run `cargo check` which runs build scripts and compiles
    /// proc macros.
    ///
    /// We need both for the precise analysis, but we want rust-analyzer to be
    /// at least partially available just after the first phase. That's because
    /// first phase is much faster, and is much less likely to fail.
    ///
    /// This creates a complication -- by the time the second phase completes,
    /// the results of the first phase could be invalid. That is, while we run
    /// `cargo check`, the user edits `Cargo.toml`, we notice this, and the new
    /// `cargo metadata` completes before `cargo check`.
    ///
    /// An additional complication is that we want to avoid needless work. When
    /// the user just adds comments or whitespace to Cargo.toml, we do not want
    /// to invalidate any salsa caches.
    pub(crate) workspaces: Arc<Vec<ProjectWorkspace>>,
    pub(crate) crate_graph_file_dependencies: FxHashSet<vfs::VfsPath>,
    pub(crate) detached_files: FxHashSet<ManifestPath>,

    // op queues
    pub(crate) fetch_workspaces_queue: OpQueue<FetchWorkspaceRequest, FetchWorkspaceResponse>,
    pub(crate) fetch_build_data_queue: OpQueue<(), FetchBuildDataResponse>,
    pub(crate) fetch_proc_macros_queue: OpQueue<(ChangeWithProcMacros, Vec<ProcMacroPaths>), bool>,
    pub(crate) prime_caches_queue: OpQueue,

    /// A deferred task queue.
    ///
    /// This queue is used for doing database-dependent work inside of sync
    /// handlers, as accessing the database may block latency-sensitive
    /// interactions and should be moved away from the main thread.
    ///
    /// For certain features, such as [`GlobalState::handle_discover_msg`],
    /// this queue should run only *after* [`GlobalState::process_changes`] has
    /// been called.
    pub(crate) deferred_task_queue: DeferredTaskQueue,

    /// HACK: Workaround for <https://github.com/rust-lang/rust-analyzer/issues/19709>
    /// This is marked true if we failed to load a crate root file at crate graph creation,
    /// which will usually end up causing a bunch of incorrect diagnostics on startup.
    pub(crate) incomplete_crate_graph: bool,

    pub(crate) minicore: MiniCoreRustAnalyzerInternalOnly,
}

// FIXME: This should move to the VFS once the rewrite is done.
#[derive(Debug, Clone, Default)]
pub(crate) struct MiniCoreRustAnalyzerInternalOnly {
    pub(crate) minicore_text: Option<Arc<str>>,
}

/// An immutable snapshot of the world's state at a point in time.
pub(crate) struct GlobalStateSnapshot {
    pub(crate) config: Arc<Config>,
    pub(crate) analysis: Analysis,
    pub(crate) analyzed_shared: crate::analyzed_bridge::SharedAnalyzerRuntime,
    pub(crate) check_fixes: CheckFixes,
    mem_docs: MemDocs,
    pub(crate) semantic_tokens_cache: Arc<Mutex<FxHashMap<Url, SemanticTokens>>>,
    pub(crate) workspaces: Arc<Vec<ProjectWorkspace>>,
    // used to signal semantic highlighting to fall back to syntax based highlighting until
    // proc-macros have been loaded
    // FIXME: Can we derive this from somewhere else?
    pub(crate) proc_macros_loaded: bool,
    pub(crate) flycheck: Arc<[FlycheckHandle]>,
    minicore: MiniCoreRustAnalyzerInternalOnly,
}

impl std::panic::UnwindSafe for GlobalStateSnapshot {}

impl GlobalState {
    pub(crate) fn new(
        sender: Sender<lsp_server::Message>,
        config: Config,
        analyzed_provider: crate::analyzed_bridge::SharedAnalyzerProvider,
        analyzed_shared: crate::analyzed_bridge::SharedAnalyzerRuntime,
        analyzed_workspaces: Vec<ProjectWorkspace>,
    ) -> GlobalState {
        let loader = {
            let (sender, receiver) = unbounded::<vfs::loader::Message>();
            let handle: vfs_notify::NotifyHandle = vfs::loader::Handle::spawn(sender);
            let handle = Box::new(handle) as Box<dyn vfs::loader::Handle>;
            Handle { handle, receiver }
        };

        let task_pool = {
            let (sender, receiver) = unbounded();
            let handle = TaskPool::new_with_threads(sender, config.main_loop_num_threads());
            Handle { handle, receiver }
        };
        let fmt_pool = {
            let (sender, receiver) = unbounded();
            let handle = TaskPool::new_with_threads(sender, 1);
            Handle { handle, receiver }
        };
        let deferred_task_queue = {
            let (sender, receiver) = unbounded();
            DeferredTaskQueue { sender, receiver }
        };

        let (flycheck_sender, flycheck_receiver) = unbounded();
        let (test_run_sender, test_run_receiver) = unbounded();

        let (discover_sender, discover_receiver) = unbounded();

        let mut this = GlobalState {
            sender,
            req_queue: ReqQueue::default(),
            task_pool,
            fmt_pool,
            loader,
            config: Arc::new(config.clone()),
            analyzed_provider,
            analyzed_shared,
            diagnostics: Default::default(),
            mem_docs: MemDocs::default(),
            semantic_tokens_cache: Arc::new(Default::default()),
            shutdown_requested: false,
            last_reported_status: lsp_ext::ServerStatusParams {
                health: lsp_ext::Health::Ok,
                quiescent: true,
                message: None,
            },
            source_root_config: SourceRootConfig::default(),
            local_roots_parent_map: Arc::new(FxHashMap::default()),
            config_errors: Default::default(),

            proc_macro_clients: Arc::from_iter([]),

            build_deps_changed: false,

            flycheck: Arc::from_iter([]),
            flycheck_sender,
            flycheck_receiver,
            last_flycheck_error: None,
            flycheck_formatted_commands: vec![],

            test_run_session: None,
            test_run_sender,
            test_run_receiver,
            test_run_remaining_jobs: 0,

            discover_handles: vec![],
            discover_sender,
            discover_receiver,
            discover_jobs_active: 0,

            fetch_ws_receiver: None,

            vfs: Arc::new(RwLock::new((vfs::Vfs::default(), Default::default()))),
            vfs_config_version: 0,
            vfs_progress_config_version: 0,
            vfs_span: None,
            vfs_done: true,
            wants_to_switch: None,

            workspaces: Arc::new(analyzed_workspaces),
            crate_graph_file_dependencies: FxHashSet::default(),
            detached_files: FxHashSet::default(),
            fetch_workspaces_queue: OpQueue::default(),
            fetch_build_data_queue: OpQueue::default(),
            fetch_proc_macros_queue: OpQueue::default(),

            prime_caches_queue: OpQueue::default(),

            deferred_task_queue,
            incomplete_crate_graph: false,

            minicore: MiniCoreRustAnalyzerInternalOnly::default(),
        };
        // Apply any required database inputs from the config.
        this.update_configuration(config);
        this
    }

    pub(crate) fn process_changes(&mut self) -> bool {
        let _p = span!(Level::INFO, "GlobalState::process_changes").entered();
        self.analyzed_process_shared_changes()
    }

    pub(crate) fn snapshot(&self) -> GlobalStateSnapshot {
        GlobalStateSnapshot {
            config: Arc::clone(&self.config),
            workspaces: Arc::clone(&self.workspaces),
            analysis: self.analyzed_shared.analysis(),
            analyzed_shared: self.analyzed_shared.clone(),
            minicore: self.minicore.clone(),
            check_fixes: Arc::clone(&self.diagnostics.check_fixes),
            mem_docs: self.mem_docs.clone(),
            semantic_tokens_cache: Arc::clone(&self.semantic_tokens_cache),
            proc_macros_loaded: !self.config.expand_proc_macros()
                || self.fetch_proc_macros_queue.last_op_result().copied().unwrap_or(false),
            flycheck: self.flycheck.clone(),
        }
    }

    pub(crate) fn send_request<R: lsp_types::request::Request>(
        &mut self,
        params: R::Params,
        handler: ReqHandler,
    ) {
        let request = self.req_queue.outgoing.register(R::METHOD.to_owned(), params, handler);
        self.send(request.into());
    }

    pub(crate) fn complete_request(&mut self, response: lsp_server::Response) {
        let handler = self
            .req_queue
            .outgoing
            .complete(response.id.clone())
            .expect("received response for unknown request");
        handler(self, response)
    }

    pub(crate) fn send_notification<N: lsp_types::notification::Notification>(
        &self,
        params: N::Params,
    ) {
        let not = lsp_server::Notification::new(N::METHOD.to_owned(), params);
        self.send(not.into());
    }

    pub(crate) fn register_request(
        &mut self,
        request: &lsp_server::Request,
        request_received: Instant,
    ) {
        self.req_queue
            .incoming
            .register(request.id.clone(), (request.method.clone(), request_received));
    }

    pub(crate) fn respond(&mut self, response: lsp_server::Response) {
        if let Some((method, start)) = self.req_queue.incoming.complete(&response.id) {
            if let Some(err) = &response.error
                && err.message.starts_with("server panicked")
            {
                self.poke_ra_ap_rust_analyzer_developer(format!("{}, check the log", err.message));
            }

            let duration = start.elapsed();
            tracing::debug!(name: "message response", method, %response.id, duration = format_args!("{:0.2?}", duration));
            self.send(response.into());
        }
    }

    pub(crate) fn cancel(&mut self, request_id: lsp_server::RequestId) {
        if let Some(response) = self.req_queue.incoming.cancel(request_id) {
            self.send(response.into());
        }
    }

    pub(crate) fn is_completed(&self, request: &lsp_server::Request) -> bool {
        self.req_queue.incoming.is_completed(&request.id)
    }

    #[track_caller]
    fn send(&self, message: lsp_server::Message) {
        self.sender.send(message).unwrap();
    }

    pub(crate) fn publish_diagnostics(
        &mut self,
        uri: Url,
        version: Option<i32>,
        mut diagnostics: Vec<lsp_types::Diagnostic>,
    ) {
        // We put this on a separate thread to avoid blocking the main thread with serialization work
        self.task_pool.handle.spawn_with_sender(stdx::thread::ThreadIntent::Worker, {
            let sender = self.sender.clone();
            move |_| {
                // VSCode assumes diagnostic messages to be non-empty strings, so we need to patch
                // empty diagnostics. Neither the docs of VSCode nor the LSP spec say whether
                // diagnostic messages are actually allowed to be empty or not and patching this
                // in the VSCode client does not work as the assertion happens in the protocol
                // conversion. So this hack is here to stay, and will be considered a hack
                // until the LSP decides to state that empty messages are allowed.

                // See https://github.com/rust-lang/rust-analyzer/issues/11404
                // See https://github.com/rust-lang/rust-analyzer/issues/13130
                let patch_empty = |message: &mut String| {
                    if message.is_empty() {
                        " ".clone_into(message);
                    }
                };

                for d in &mut diagnostics {
                    patch_empty(&mut d.message);
                    if let Some(dri) = &mut d.related_information {
                        for dri in dri {
                            patch_empty(&mut dri.message);
                        }
                    }
                }

                let not = lsp_server::Notification::new(
                    <lsp_types::notification::PublishDiagnostics as lsp_types::notification::Notification>::METHOD.to_owned(),
                    lsp_types::PublishDiagnosticsParams { uri, diagnostics, version },
                );
                _ = sender.send(not.into());
            }
        });
    }

    pub(crate) fn check_workspaces_msrv(&self) -> impl Iterator<Item = String> + '_ {
        self.workspaces.iter().filter_map(|ws| {
            if let Some(toolchain) = &ws.toolchain
                && *toolchain < crate::MINIMUM_SUPPORTED_TOOLCHAIN_VERSION
            {
                return Some(format!(
                    "Workspace `{}` is using an outdated toolchain version `{}` but \
                        rust-analyzer only supports `{}` and higher.\n\
                        Consider using the rust-analyzer rustup component for your toolchain or
                        upgrade your toolchain to a supported version.\n\n",
                    ws.manifest_or_root(),
                    toolchain,
                    crate::MINIMUM_SUPPORTED_TOOLCHAIN_VERSION,
                ));
            }
            None
        })
    }

    pub(crate) fn debounce_workspace_fetch(&mut self) {
        if let Some((fetch_receiver, _)) = &mut self.fetch_ws_receiver {
            *fetch_receiver = crossbeam_channel::after(Duration::from_millis(100));
        }
    }
}

impl Drop for GlobalState {
    fn drop(&mut self) {}
}

impl GlobalStateSnapshot {
    /// Returns `None` if the file was excluded.
    pub(crate) fn url_to_file_id(&self, url: &Url) -> anyhow::Result<Option<FileId>> {
        self.analyzed_shared.url_to_file_id(url)
    }

    pub(crate) fn file_id_to_url(&self, id: FileId) -> Url {
        self.analyzed_shared
            .file_id_to_url(id)
            .expect("shared analyzer file id must have a url")
    }

    /// Returns `None` if the file was excluded.
    pub(crate) fn vfs_path_to_file_id(&self, vfs_path: &VfsPath) -> anyhow::Result<Option<FileId>> {
        self.analyzed_shared.vfs_path_to_file_id(vfs_path)
    }

    pub(crate) fn base_vfs_path_to_file_id(&self, vfs_path: &VfsPath) -> anyhow::Result<Option<FileId>> {
        self.analyzed_shared.base_vfs_path_to_file_id(vfs_path)
    }

    pub(crate) fn file_line_index(&self, file_id: FileId) -> Cancellable<LineIndex> {
        let endings = self
            .analyzed_shared
            .line_endings(file_id)
            .expect("shared analyzer line endings must be tracked");
        let index = self.analysis.file_line_index(file_id)?;
        let res = LineIndex { index, endings, encoding: self.config.caps().negotiated_encoding() };
        Ok(res)
    }

    pub(crate) fn file_version(&self, file_id: FileId) -> Option<i32> {
        let path = self.file_id_to_file_path(file_id);
        Some(self.mem_docs.get(&path)?.version)
    }

    pub(crate) fn url_file_version(&self, url: &Url) -> Option<i32> {
        let path = from_proto::vfs_path(url).ok()?;
        Some(self.mem_docs.get(&path)?.version)
    }

    pub(crate) fn anchored_path(&self, path: &AnchoredPathBuf) -> Url {
        let mut base = self.file_id_to_file_path(path.anchor);
        base.pop();
        let path = base.join(&path.path).unwrap();
        let path = path.as_path().unwrap();
        url_from_abs_path(path)
    }

    pub(crate) fn file_id_to_file_path(&self, file_id: FileId) -> vfs::VfsPath {
        self.analyzed_shared
            .file_id_to_vfs_path(file_id)
            .expect("shared analyzer file id must have a path")
    }

    pub(crate) fn target_spec_for_crate(&self, crate_id: Crate) -> Option<TargetSpec> {
        let file_id = self.analysis.crate_root(crate_id).ok()?;
        self.target_spec_for_file(file_id, crate_id)
    }

    pub(crate) fn target_spec_for_file(
        &self,
        file_id: FileId,
        crate_id: Crate,
    ) -> Option<TargetSpec> {
        let path = self.file_id_to_file_path(file_id);
        let path = path.as_path()?;

        for workspace in self.workspaces.iter() {
            match &workspace.kind {
                ProjectWorkspaceKind::Cargo { cargo, .. }
                | ProjectWorkspaceKind::DetachedFile { cargo: Some((cargo, _, _)), .. } => {
                    let Some(target_idx) = cargo.target_by_root(path) else {
                        continue;
                    };

                    let target_data = &cargo[target_idx];
                    let package_data = &cargo[target_data.package];

                    return Some(TargetSpec::Cargo(CargoTargetSpec {
                        workspace_root: cargo.workspace_root().to_path_buf(),
                        cargo_toml: package_data.manifest.clone(),
                        crate_id,
                        package: cargo.package_flag(package_data),
                        package_id: package_data.id.clone(),
                        target: target_data.name.clone(),
                        target_kind: target_data.kind,
                        required_features: target_data.required_features.clone(),
                        features: package_data.features.keys().cloned().collect(),
                        sysroot_root: workspace.sysroot.root().map(ToOwned::to_owned),
                    }));
                }
                ProjectWorkspaceKind::Json(project) => {
                    let Some(krate) = project.crate_by_root(path) else {
                        continue;
                    };
                    let Some(build) = krate.build.clone() else {
                        continue;
                    };

                    return Some(TargetSpec::ProjectJson(ProjectJsonTargetSpec {
                        label: build.label,
                        target_kind: build.target_kind,
                        shell_runnables: project.runnables().to_owned(),
                        project_root: project.project_root().to_owned(),
                    }));
                }
                ProjectWorkspaceKind::DetachedFile { .. } => {}
            };
        }

        None
    }

    pub(crate) fn all_workspace_dependencies_for_package(
        &self,
        package: &PackageSpecifier,
    ) -> Option<FxHashSet<PackageSpecifier>> {
        match package {
            PackageSpecifier::Cargo { package_id } => {
                self.workspaces.iter().find_map(|workspace| match &workspace.kind {
                    ProjectWorkspaceKind::Cargo { cargo, .. }
                    | ProjectWorkspaceKind::DetachedFile { cargo: Some((cargo, _, _)), .. } => {
                        let package = cargo.packages().find(|p| cargo[*p].id == *package_id)?;

                        cargo[package].all_member_deps.as_ref().map(|deps| {
                            deps.iter()
                                .map(|dep| cargo[*dep].id.clone())
                                .map(|p| PackageSpecifier::Cargo { package_id: p })
                                .collect()
                        })
                    }
                    _ => None,
                })
            }
            PackageSpecifier::BuildInfo { label } => {
                self.workspaces.iter().find_map(|workspace| match &workspace.kind {
                    ProjectWorkspaceKind::Json(p) => {
                        let krate = p.crate_by_label(label)?;
                        Some(
                            krate
                                .iter_deps()
                                .filter_map(|dep| p[dep].build.as_ref())
                                .map(|build| PackageSpecifier::BuildInfo {
                                    label: build.label.clone(),
                                })
                                .collect(),
                        )
                    }
                    _ => None,
                })
            }
        }
    }

    pub(crate) fn file_exists(&self, file_id: FileId) -> bool {
        self.analyzed_shared.file_exists(file_id).unwrap_or(false)
    }

    #[inline]
    pub(crate) fn minicore(&self) -> MiniCore<'_> {
        match &self.minicore.minicore_text {
            Some(minicore) => MiniCore::new(minicore),
            None => MiniCore::default(),
        }
    }
}
