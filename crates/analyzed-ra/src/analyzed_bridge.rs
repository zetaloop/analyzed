	use std::{
	    collections::{BTreeMap, BTreeSet, btree_map::Entry},
	    env,
	    path::{Path, PathBuf},
	    sync::{
	        Arc, Condvar, Mutex, OnceLock, Weak,
	        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
	    },
	    thread,
	    time::Duration,
	};

	use hir::{ChangeWithProcMacros, ProcMacrosBuilder, collect_ty_garbage};
	use ide::{Analysis, AnalysisHost, FileId, RootDatabase};
	use ide_db::{
	    FxHashMap,
	    base_db::{
	        CrateGraphBuilder, DependencyBuilder, FileSet, LibraryRoots, LocalRoots,
	        ProcMacroPaths, SourceDatabase, SourceRoot, SourceRootId, all_crates,
	        salsa::{Database, Durability, Setter as _},
	    },
	};
	use load_cargo::{
	    AnalyzedProcMacroLoad, AnalyzedWorkspaceLoad, LoadCargoConfig, ProcMacroServerChoice,
	    analyzed_load_workspace_change,
	};
	use lsp_types::Url;
	use proc_macro_api::ProcMacroClient;
	use project_model::{CargoConfig, ManifestPath, ProjectWorkspace};
	use serde::Serialize;
	use vfs::{AbsPathBuf, Vfs, VfsPath};

pub const RUST_ANALYZER_VERSION: &str = env!("ANALYZED_RA_VERSION");

#[derive(Clone, Debug, Serialize)]
pub struct RustAnalyzerLspBoundary {
    pub main_loop: &'static str,
}

pub fn rust_analyzer_lsp_boundary() -> RustAnalyzerLspBoundary {
    let _main_loop = crate::main_loop;

    RustAnalyzerLspBoundary {
        main_loop: "ra_ap_rust_analyzer::main_loop",
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RustAnalyzerPrivateBoundary {
    pub global_state: &'static str,
    pub request_dispatcher: &'static str,
    pub notification_dispatcher: &'static str,
}

pub fn rust_analyzer_private_boundary() -> RustAnalyzerPrivateBoundary {
    let _global_state_size = std::mem::size_of::<crate::global_state::GlobalState>();
    let _request_dispatcher_size =
        std::mem::size_of::<crate::handlers::dispatch::RequestDispatcher<'_>>();
    let _notification_dispatcher_size =
        std::mem::size_of::<crate::handlers::dispatch::NotificationDispatcher<'_>>();

    RustAnalyzerPrivateBoundary {
        global_state: std::any::type_name::<crate::global_state::GlobalState>(),
        request_dispatcher: std::any::type_name::<
            crate::handlers::dispatch::RequestDispatcher<'_>,
        >(),
        notification_dispatcher: std::any::type_name::<
            crate::handlers::dispatch::NotificationDispatcher<'_>,
        >(),
    }
}

pub fn run_shared_rust_analyzer_lsp_session(
    connection: lsp_server::Connection,
    provider: SharedAnalyzerProvider,
) -> anyhow::Result<()> {
    crate::main_loop::analyzed_session::run_shared_lsp_session(connection, provider)
}

pub fn run_shared_rust_analyzer_lsp_session_with_config(
    config: crate::config::Config,
    connection: lsp_server::Connection,
) -> anyhow::Result<()> {
    let registry = shared_analyzer_registry();
    let provider = SharedAnalyzerProvider::new(move |key, config, reload_path| {
        registry.register(key, config, reload_path)
    });

    crate::main_loop::analyzed_session::run_shared_lsp_session_with_config(
        config,
        connection,
        provider,
    )
}

#[derive(Clone)]
pub struct SharedAnalyzerProvider {
    resolve: Arc<
        dyn Fn(
                SharedAnalyzerBackendKey,
                Arc<SharedAnalyzerConfig>,
                Option<AbsPathBuf>,
            ) -> anyhow::Result<SharedAnalyzerSession>
            + Send
            + Sync
            + std::panic::RefUnwindSafe,
    >,
}

impl SharedAnalyzerProvider {
    pub fn new<F>(resolve: F) -> Self
    where
        F: Fn(
                SharedAnalyzerBackendKey,
                Arc<SharedAnalyzerConfig>,
                Option<AbsPathBuf>,
            ) -> anyhow::Result<SharedAnalyzerSession>
            + Send
            + Sync
            + std::panic::RefUnwindSafe
            + 'static,
    {
        Self { resolve: Arc::new(resolve) }
    }

    pub(crate) fn resolve(
        &self,
        key: SharedAnalyzerBackendKey,
        config: Arc<SharedAnalyzerConfig>,
    ) -> anyhow::Result<SharedAnalyzerSession> {
        (self.resolve)(key, config, None)
    }

    pub(crate) fn resolve_reloading(
        &self,
        key: SharedAnalyzerBackendKey,
        config: Arc<SharedAnalyzerConfig>,
        reload_path: Option<AbsPathBuf>,
    ) -> anyhow::Result<SharedAnalyzerSession> {
        (self.resolve)(key, config, reload_path)
    }
}

pub fn shared_analyzer_registry() -> Arc<SharedAnalyzerRegistry> {
    static REGISTRY: OnceLock<Arc<SharedAnalyzerRegistry>> = OnceLock::new();
    Arc::clone(REGISTRY.get_or_init(|| Arc::new(SharedAnalyzerRegistry::new())))
}

#[derive(Debug, Serialize)]
pub struct SharedAnalyzerBackendSnapshot {
    pub key: SharedAnalyzerBackendKey,
    pub client_sessions: usize,
    pub overlay_sessions: usize,
    pub overlay_files: usize,
    pub workspace_loads: Vec<WorkspaceSummary>,
}

pub struct SharedAnalyzerRegistry {
    state: Mutex<SharedAnalyzerRegistryState>,
    gc: SharedAnalyzerGcCoordinator,
}

#[derive(Default)]
struct SharedAnalyzerRegistryState {
    worlds: BTreeMap<SharedAnalyzerWorldKey, SharedAnalyzerWorldEntry>,
    views: BTreeMap<SharedAnalyzerBackendKey, SharedAnalyzerViewEntry>,
    loads: BTreeMap<SharedAnalyzerWorkspaceLoadKey, Arc<SharedAnalyzerWorkspaceLoad>>,
}

struct SharedAnalyzerWorldEntry {
    client_sessions: usize,
    world: Arc<Mutex<SharedWorld>>,
}

struct SharedAnalyzerViewEntry {
    client_sessions: usize,
    view: WorkspaceView,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SharedAnalyzerWorkspaceLoadKey {
    world: SharedAnalyzerWorldKey,
    project: String,
}

struct SharedAnalyzerWorkspaceLoad {
    result: Mutex<Option<Result<usize, String>>>,
    ready: Condvar,
}

struct SharedAnalyzerRegistryLease {
    registry: Weak<SharedAnalyzerRegistry>,
    key: SharedAnalyzerBackendKey,
}

struct SharedAnalyzerGcCoordinator {
    dirty: Arc<AtomicBool>,
}

#[derive(Default)]
struct SharedWorldAccess {
    state: Mutex<SharedWorldAccessState>,
    ready: Condvar,
}

#[derive(Default)]
struct SharedWorldAccessState {
    readers: BTreeMap<u64, usize>,
    pending_writers: BTreeMap<Option<u64>, usize>,
}

pub(crate) struct SharedAnalyzerReadPermit {
    access: Arc<SharedWorldAccess>,
    session_id: u64,
}

struct SharedAnalyzerWritePermit {
    access: Arc<SharedWorldAccess>,
    owner: Option<u64>,
}

impl SharedWorldAccess {
    fn read(self: &Arc<Self>, session_id: u64) -> SharedAnalyzerReadPermit {
        let mut state = self.state.lock().expect("shared world access mutex poisoned");
        while state.read_is_blocked(session_id) {
            state = self
                .ready
                .wait(state)
                .expect("shared world access mutex poisoned");
        }
        *state.readers.entry(session_id).or_default() += 1;
        SharedAnalyzerReadPermit { access: Arc::clone(self), session_id }
    }

    fn write(self: &Arc<Self>, owner: Option<u64>) -> SharedAnalyzerWritePermit {
        let mut state = self.state.lock().expect("shared world access mutex poisoned");
        *state.pending_writers.entry(owner).or_default() += 1;
        while state.write_is_blocked(owner) {
            state = self
                .ready
                .wait(state)
                .expect("shared world access mutex poisoned");
        }
        SharedAnalyzerWritePermit { access: Arc::clone(self), owner }
    }
}

impl SharedWorldAccessState {
    fn read_is_blocked(&self, session_id: u64) -> bool {
        self.pending_writers
            .iter()
            .any(|(owner, count)| *count > 0 && *owner != Some(session_id))
    }

    fn write_is_blocked(&self, owner: Option<u64>) -> bool {
        self.readers
            .iter()
            .any(|(reader, count)| *count > 0 && owner != Some(*reader))
    }
}

impl Drop for SharedAnalyzerReadPermit {
    fn drop(&mut self) {
        let mut state = self.state();
        let count = state
            .readers
            .get_mut(&self.session_id)
            .expect("shared world reader was registered");
        *count -= 1;
        if *count == 0 {
            state.readers.remove(&self.session_id);
        }
        self.access.ready.notify_all();
    }
}

impl SharedAnalyzerReadPermit {
    fn state(&self) -> std::sync::MutexGuard<'_, SharedWorldAccessState> {
        self.access
            .state
            .lock()
            .expect("shared world access mutex poisoned")
    }
}

impl Drop for SharedAnalyzerWritePermit {
    fn drop(&mut self) {
        let mut state = self.state();
        let count = state
            .pending_writers
            .get_mut(&self.owner)
            .expect("shared world writer was registered");
        *count -= 1;
        if *count == 0 {
            state.pending_writers.remove(&self.owner);
        }
        self.access.ready.notify_all();
    }
}

impl SharedAnalyzerWritePermit {
    fn state(&self) -> std::sync::MutexGuard<'_, SharedWorldAccessState> {
        self.access
            .state
            .lock()
            .expect("shared world access mutex poisoned")
    }
}

impl SharedAnalyzerRegistry {
    fn new() -> Self {
        let gc = SharedAnalyzerGcCoordinator::spawn();

        Self {
            state: Mutex::new(SharedAnalyzerRegistryState::default()),
            gc,
        }
    }

    fn state(
        &self,
    ) -> anyhow::Result<std::sync::MutexGuard<'_, SharedAnalyzerRegistryState>> {
        self.state
            .lock()
            .map_err(|error| anyhow::format_err!("shared analyzer registry mutex is poisoned: {error}"))
    }

    pub fn register(
        self: &Arc<Self>,
        key: SharedAnalyzerBackendKey,
        config: Arc<SharedAnalyzerConfig>,
        reload_path: Option<AbsPathBuf>,
    ) -> anyhow::Result<SharedAnalyzerSession> {
        let world = self.world(&key.shared_world)?;
        self.retain_world(&key.shared_world)?;
        let reload = reload_path.is_some();

        let result = (|| {
            let mut workspaces = Vec::new();

            for project in config.projects() {
                workspaces.push(self.ensure_workspace_loaded(
                    key.shared_world.clone(),
                    Arc::clone(&world),
                    shared_project_key(project),
                    SharedAnalyzerWorkspaceLoadSource::Project(project.clone()),
                    &config,
                    reload,
                )?);
            }
            for file in config.detached_files() {
                workspaces.push(self.ensure_workspace_loaded(
                    key.shared_world.clone(),
                    Arc::clone(&world),
                    shared_detached_file_key(file),
                    SharedAnalyzerWorkspaceLoadSource::DetachedFile(file.clone()),
                    &config,
                    reload,
                )?);
            }

            let view = WorkspaceView::new(workspaces, config.excluded_paths().to_vec());
            {
                let mut state = self.state()?;
                match state.views.entry(key.clone()) {
                    Entry::Occupied(mut entry) => {
                        entry.get_mut().client_sessions += 1;
                    }
                    Entry::Vacant(entry) => {
                        entry.insert(SharedAnalyzerViewEntry {
                            client_sessions: 1,
                            view: view.clone(),
                        });
                    }
                }
            }

            Ok(SharedAnalyzerSession::new_registered(
                world,
                view,
                Arc::downgrade(self),
                key.clone(),
            ))
        })();

        if result.is_err() {
            self.release_world(&key.shared_world);
        }

        result
    }

    fn retain_world(&self, key: &SharedAnalyzerWorldKey) -> anyhow::Result<()> {
        let mut state = self.state()?;
        state
            .worlds
            .get_mut(key)
            .expect("shared world was registered")
            .client_sessions += 1;

        Ok(())
    }

    fn release_world(&self, key: &SharedAnalyzerWorldKey) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        release_world_state(&mut state, key);
    }

    fn world(
        &self,
        key: &SharedAnalyzerWorldKey,
    ) -> anyhow::Result<Arc<Mutex<SharedWorld>>> {
        let mut state = self.state()?;
        let entry = match state.worlds.entry(key.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(SharedAnalyzerWorldEntry {
                client_sessions: 0,
                world: Arc::new(Mutex::new(SharedWorld::new())),
            }),
        };

        Ok(Arc::clone(&entry.world))
    }

    fn ensure_workspace_loaded(
        &self,
        world_key: SharedAnalyzerWorldKey,
        world: Arc<Mutex<SharedWorld>>,
        load_key: String,
        source: SharedAnalyzerWorkspaceLoadSource,
        config: &SharedAnalyzerConfig,
        reload: bool,
    ) -> anyhow::Result<usize> {
        if !reload
            && let Some(index) = world
                .lock()
                .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?
                .workspace_index(&load_key)
        {
            return Ok(index);
        }

        let registry_load_key = SharedAnalyzerWorkspaceLoadKey {
            world: world_key,
            project: load_key,
        };
        let (load, leader) = {
            let mut state = self.state()?;
            match state.loads.entry(registry_load_key.clone()) {
                Entry::Occupied(entry) => (Arc::clone(entry.get()), false),
                Entry::Vacant(entry) => {
                    let load = Arc::new(SharedAnalyzerWorkspaceLoad::new());
                    entry.insert(Arc::clone(&load));
                    (load, true)
                }
            }
        };

        if leader {
            let result = SharedWorld::prepare_workspace_load(source, config)
                .and_then(|loaded| {
                    let access = world
                        .lock()
                        .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?
                        .access();
                    let _write = access.write(None);
                    world
                        .lock()
                        .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?
                        .commit_workspace(loaded, reload)
                });
            load.finish(result);
            self.state()?.loads.remove(&registry_load_key);
        }

        load.wait()
    }

    pub fn unregister(&self, key: &SharedAnalyzerBackendKey) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };

        if let Some(entry) = state.views.get_mut(key) {
            entry.client_sessions = entry.client_sessions.saturating_sub(1);
            if entry.client_sessions == 0 {
                state.views.remove(key);
            }
        }

        release_world_state(&mut state, &key.shared_world);
    }

    pub fn backend_snapshots(&self) -> Vec<SharedAnalyzerBackendSnapshot> {
        let entries = {
            let Ok(state) = self.state.lock() else {
                return Vec::new();
            };
            state
                .views
                .iter()
                .filter_map(|(key, entry)| {
                    let world = state.worlds.get(&key.shared_world)?;
                    Some((
                        key.clone(),
                        entry.client_sessions,
                        Arc::clone(&world.world),
                        entry.view.clone(),
                    ))
                })
                .collect::<Vec<_>>()
        };

        entries
            .into_iter()
            .map(|(key, client_sessions, world, view)| {
                let (overlay_sessions, overlay_files, workspace_loads) = world
                    .lock()
                    .map(|world| {
                        (
                            world.active_overlay_sessions(),
                            world.overlay_files(),
                            world.workspace_summaries(&view),
                        )
                    })
                    .unwrap_or_default();

                SharedAnalyzerBackendSnapshot {
                    key,
                    client_sessions,
                    overlay_sessions,
                    overlay_files,
                    workspace_loads,
                }
            })
            .collect()
    }

    pub fn workspace_loads(&self) -> Vec<WorkspaceSummary> {
        self.backend_snapshots()
            .into_iter()
            .flat_map(|snapshot| snapshot.workspace_loads)
            .collect()
    }

    pub(crate) fn mark_gc_dirty(&self) {
        self.gc.mark_dirty();
    }
}

fn release_world_state(
    state: &mut SharedAnalyzerRegistryState,
    key: &SharedAnalyzerWorldKey,
) {
    if let Some(entry) = state.worlds.get_mut(key) {
        entry.client_sessions = entry.client_sessions.saturating_sub(1);
        if entry.client_sessions == 0 {
            state.worlds.remove(key);
        }
    }
}

impl SharedAnalyzerWorkspaceLoad {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            ready: Condvar::new(),
        }
    }

    fn finish(&self, result: anyhow::Result<usize>) {
        if let Ok(mut slot) = self.result.lock() {
            *slot = Some(result.map_err(|error| format!("{error:#}")));
            self.ready.notify_all();
        }
    }

    fn wait(&self) -> anyhow::Result<usize> {
        let mut slot = self
            .result
            .lock()
            .map_err(|error| anyhow::format_err!("shared analyzer load mutex is poisoned: {error}"))?;

        loop {
            if let Some(result) = &*slot {
                return result
                    .as_ref()
                    .copied()
                    .map_err(|error| anyhow::format_err!("{error}"));
            }
            slot = self
                .ready
                .wait(slot)
                .map_err(|error| anyhow::format_err!("shared analyzer load mutex is poisoned: {error}"))?;
        }
    }
}

impl SharedAnalyzerGcCoordinator {
    fn spawn() -> Self {
        let dirty = Arc::new(AtomicBool::new(false));
        let worker_dirty = Arc::clone(&dirty);

        thread::Builder::new()
            .name("shared analyzer gc".to_owned())
            .spawn(move || loop {
                if !worker_dirty.swap(false, Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }

                thread::sleep(Duration::from_millis(200));
                let registry = shared_analyzer_registry();
                let worlds = {
                    let Ok(state) = registry.state.lock() else {
                        continue;
                    };
                    state
                        .worlds
                        .values()
                        .map(|entry| Arc::clone(&entry.world))
                        .collect::<Vec<_>>()
                };

                let mut targets = Vec::new();
                let mut blocked = false;
                for world in worlds {
                    match world.lock() {
                        Ok(world_guard) => {
                            let access = world_guard.access();
                            if world_guard.any_session_busy() {
                                blocked = true;
                                break;
                            }
                            targets.push((Arc::clone(&world), access));
                        }
                        Err(error) => {
                            tracing::error!("shared world mutex is poisoned during gc: {error}");
                            blocked = true;
                            break;
                        }
                    }
                }

                if blocked {
                    worker_dirty.store(true, Ordering::SeqCst);
                    continue;
                }

                if !targets.is_empty() {
                    for (world, access) in targets {
                        let _write = access.write(None);
                        match world.lock() {
                            Ok(mut world) => world.synthetic_write(),
                            Err(error) => {
                                tracing::error!("shared world mutex is poisoned during gc: {error}");
                                blocked = true;
                                break;
                            }
                        }
                    }
                    if blocked {
                        worker_dirty.store(true, Ordering::SeqCst);
                        continue;
                    }
                    unsafe { collect_ty_garbage() };
                }
            })
            .expect("failed to spawn shared analyzer gc thread");

        Self { dirty }
    }

    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::SeqCst);
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SharedAnalyzerBackendKey {
    pub shared_world: SharedAnalyzerWorldKey,
    pub workspace_view: SharedAnalyzerViewKey,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SharedAnalyzerWorldKey {
    pub rust_analyzer_version: String,
    pub toolchain: Option<String>,
    pub sysroot: Option<String>,
    pub cargo_target: Option<String>,
    pub config: SharedAnalyzerWorldConfigKey,
    pub load: SharedAnalyzerLoadKey,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SharedAnalyzerWorldConfigKey {
    pub cargo: SharedAnalyzerCargoConfigKey,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SharedAnalyzerCargoConfigKey {
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

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SharedAnalyzerLoadKey {
    pub load_out_dirs_from_check: bool,
    pub proc_macro_server: SharedAnalyzerProcMacroServerKey,
    pub prefill_caches: bool,
    pub num_worker_threads: u16,
    pub proc_macro_processes: u16,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum SharedAnalyzerProcMacroServerKey {
    None,
    Sysroot,
    Explicit(String),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SharedAnalyzerViewKey {
    pub workspace_roots: Vec<String>,
    pub projects: Vec<String>,
    pub excluded_paths: Vec<String>,
    pub analysis: SharedAnalyzerAnalysisKey,
}

#[derive(Clone)]
enum SharedAnalyzerWorkspaceLoadSource {
    Project(crate::config::LinkedProject),
    DetachedFile(ManifestPath),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SharedAnalyzerAnalysisKey {
    pub initialization_options: Option<String>,
    pub workspace_configuration: Option<String>,
}

pub(crate) fn shared_analyzer_context_from_config(
    config: &crate::config::Config,
) -> anyhow::Result<(SharedAnalyzerBackendKey, Arc<SharedAnalyzerConfig>)> {
    let mut workspace_roots = config
        .workspace_roots()
        .iter()
        .map(|root| path_key(&VfsPath::from(root.clone())))
        .collect::<Vec<_>>();
    workspace_roots.sort();
    workspace_roots.dedup();
    let mut excluded_paths = config
        .excluded()
        .map(|path| path_key(&VfsPath::from(path)))
        .collect::<Vec<_>>();
    excluded_paths.sort();
    excluded_paths.dedup();
    let projects = config.linked_or_discovered_projects();
    let detached_files = config
        .detached_files()
        .iter()
        .cloned()
        .map(ManifestPath::try_from)
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    let mut project_keys = projects.iter().map(shared_project_key).collect::<Vec<_>>();
    project_keys.extend(detached_files.iter().map(shared_detached_file_key));
    project_keys.sort();
    project_keys.dedup();
    let cargo_config = config.cargo(None);
    let load = shared_load_config_from_config(config)?;
    let analysis = SharedAnalyzerAnalysisKey {
        initialization_options: None,
        workspace_configuration: None,
    };
    let backend_key = SharedAnalyzerBackendKey {
        shared_world: SharedAnalyzerWorldKey {
            rust_analyzer_version: RUST_ANALYZER_VERSION.to_owned(),
            toolchain: env::var("RUSTUP_TOOLCHAIN").ok(),
            sysroot: env::var("RUST_SRC_PATH").ok(),
            cargo_target: cargo_config
                .target
                .clone()
                .or_else(|| env::var("CARGO_BUILD_TARGET").ok()),
            config: SharedAnalyzerWorldConfigKey {
                cargo: cargo_config_key(&cargo_config),
            },
            load: load.key.clone(),
        },
        workspace_view: SharedAnalyzerViewKey {
            workspace_roots: workspace_roots.clone(),
            projects: project_keys,
            excluded_paths: excluded_paths.clone(),
            analysis,
        },
    };

    Ok((
        backend_key,
        Arc::new(SharedAnalyzerConfig {
            workspace_roots,
            excluded_paths,
            projects,
            detached_files,
            cargo_config,
            load,
        }),
    ))
}

pub struct SharedAnalyzerConfig {
    workspace_roots: Vec<String>,
    excluded_paths: Vec<String>,
    projects: Vec<crate::config::LinkedProject>,
    detached_files: Vec<ManifestPath>,
    pub(crate) cargo_config: CargoConfig,
    pub(crate) load: SharedLoadConfig,
}

impl SharedAnalyzerConfig {
    pub fn workspace_roots(&self) -> &[String] {
        &self.workspace_roots
    }

    pub fn excluded_paths(&self) -> &[String] {
        &self.excluded_paths
    }

    fn projects(&self) -> &[crate::config::LinkedProject] {
        &self.projects
    }

    fn detached_files(&self) -> &[ManifestPath] {
        &self.detached_files
    }
}

fn shared_project_key(project: &crate::config::LinkedProject) -> String {
    match project {
        crate::config::LinkedProject::ProjectManifest(manifest) => format!("manifest:{manifest}"),
        crate::config::LinkedProject::InlineProjectJson(project) => format!("json:{project:?}"),
    }
}

fn shared_detached_file_key(file: &ManifestPath) -> String {
    format!("detached:{file}")
}

#[derive(Clone)]
pub(crate) struct SharedLoadConfig {
    key: SharedAnalyzerLoadKey,
}

impl SharedLoadConfig {
    pub(crate) fn to_load_cargo_config(&self) -> LoadCargoConfig {
        LoadCargoConfig {
            load_out_dirs_from_check: self.key.load_out_dirs_from_check,
            with_proc_macro_server: match &self.key.proc_macro_server {
                SharedAnalyzerProcMacroServerKey::None => ProcMacroServerChoice::None,
                SharedAnalyzerProcMacroServerKey::Sysroot => ProcMacroServerChoice::Sysroot,
                SharedAnalyzerProcMacroServerKey::Explicit(path) => {
                    ProcMacroServerChoice::Explicit(AbsPathBuf::assert_utf8(PathBuf::from(path)))
                }
            },
            prefill_caches: self.key.prefill_caches,
            num_worker_threads: self.key.num_worker_threads as usize,
            proc_macro_processes: self.key.proc_macro_processes as usize,
        }
    }
}

fn shared_load_config_from_config(
    config: &crate::config::Config,
) -> anyhow::Result<SharedLoadConfig> {
    Ok(SharedLoadConfig {
        key: SharedAnalyzerLoadKey {
            load_out_dirs_from_check: config.run_build_scripts(None),
            proc_macro_server: if config.expand_proc_macros() {
                config
                    .proc_macro_srv()
                    .map(|path| SharedAnalyzerProcMacroServerKey::Explicit(path.to_string()))
                    .unwrap_or(SharedAnalyzerProcMacroServerKey::Sysroot)
            } else {
                SharedAnalyzerProcMacroServerKey::None
            },
            prefill_caches: config.prefill_caches(),
            num_worker_threads: u16::try_from(config.prime_caches_num_threads())?,
            proc_macro_processes: u16::try_from(config.proc_macro_num_processes())?,
        },
    })
}

fn cargo_config_key(config: &CargoConfig) -> SharedAnalyzerCargoConfigKey {
    let mut extra_env = config
        .extra_env
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Vec<_>>();
    extra_env.sort();

    SharedAnalyzerCargoConfigKey {
        all_targets: config.all_targets,
        features: format!("{:?}", config.features),
        target: config.target.clone(),
        sysroot: config.sysroot.as_ref().map(|it| format!("{it:?}")),
        sysroot_src: config.sysroot_src.as_ref().map(ToString::to_string),
        rustc_source: config.rustc_source.as_ref().map(|it| format!("{it:?}")),
        extra_includes: config
            .extra_includes
            .iter()
            .map(ToString::to_string)
            .collect(),
        cfg_overrides: format!("{:?}", config.cfg_overrides),
        wrap_rustc_in_build_scripts: config.wrap_rustc_in_build_scripts,
        invocation_strategy: format!("{:?}", config.invocation_strategy),
        run_build_script_command: format!("{:?}", config.run_build_script_command),
        extra_args: config.extra_args.clone(),
        extra_env,
        target_dir_config: format!("{:?}", config.target_dir_config),
        set_test: config.set_test,
        no_deps: config.no_deps,
        metadata_extra_args: config.metadata_extra_args.clone(),
    }
}

pub(crate) fn patch_path_prefix(path: PathBuf) -> PathBuf {
    use std::path::{Component, Prefix};

    if cfg!(windows) {
        let mut components = path.components();
        match components.next() {
            Some(Component::Prefix(prefix)) => {
                let prefix = match prefix.kind() {
                    Prefix::Disk(disk) => format!("{}:", disk.to_ascii_uppercase() as char),
                    Prefix::VerbatimDisk(disk) => {
                        format!(r"\\?\{}:", disk.to_ascii_uppercase() as char)
                    }
                    _ => return path,
                };
                let mut path = PathBuf::new();
                path.push(prefix);
                path.extend(components);
                path
            }
            _ => path,
        }
    } else {
        path
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceSummary {
    pub root: String,
    pub manifest: String,
    pub packages: usize,
    pub files: usize,
    pub proc_macro_server: bool,
}

#[derive(Clone)]
pub struct SharedAnalyzerSession {
    world: Arc<Mutex<SharedWorld>>,
    view: WorkspaceView,
    runtime: SharedAnalyzerRuntime,
}

impl SharedAnalyzerSession {
    pub fn new(world: Arc<Mutex<SharedWorld>>, view: WorkspaceView) -> Self {
        let runtime = SharedAnalyzerRuntime::new(Arc::clone(&world), &view);

        Self {
            world,
            view,
            runtime,
        }
    }

    fn new_registered(
        world: Arc<Mutex<SharedWorld>>,
        view: WorkspaceView,
        registry: Weak<SharedAnalyzerRegistry>,
        key: SharedAnalyzerBackendKey,
    ) -> Self {
        let runtime =
            SharedAnalyzerRuntime::new_registered(Arc::clone(&world), &view, registry, key);

        Self {
            world,
            view,
            runtime,
        }
    }

    pub fn workspaces(&self) -> anyhow::Result<Vec<ProjectWorkspace>> {
        let world = self
            .world
            .lock()
            .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?;

        Ok(world.workspaces(&self.view))
    }

    pub fn runtime(&self) -> SharedAnalyzerRuntime {
        self.runtime.clone()
    }
}

#[derive(Clone)]
pub struct SharedAnalyzerRuntime {
    world: Arc<Mutex<SharedWorld>>,
    session: Arc<SharedAnalyzerRuntimeSession>,
}

struct SharedAnalyzerRuntimeSession {
    world: Arc<Mutex<SharedWorld>>,
    access: Arc<SharedWorldAccess>,
    id: u64,
    activity: Arc<AtomicBool>,
    input_generation: Arc<AtomicU64>,
    config_generation_seen: AtomicU64,
    edit_generation: AtomicU64,
    workspace_indexes: Vec<usize>,
    excluded_paths: Vec<String>,
    line_endings: Mutex<SharedLineEndings>,
    file_mappings: Mutex<SharedFileMappings>,
    analysis_cache: Mutex<SharedAnalysisCache>,
    registry_lease: Option<SharedAnalyzerRegistryLease>,
}

// Visible crate roots and the session mappings only move when the world's
// inputs move. Recomputing them on every snapshot walks every crate in the
// merged world and re-verifies it against the current salsa revision, which
// under cross-session write traffic turns each snapshot into seconds of
// revalidation and starves the session's main loop.
#[derive(Default)]
struct SharedAnalysisCache {
    generation: Option<u64>,
    visible_files: Arc<rustc_hash::FxHashSet<FileId>>,
}

impl Drop for SharedAnalyzerRuntimeSession {
    fn drop(&mut self) {
        {
            let _write = self.access.write(Some(self.id));
            if let Ok(mut world) = self.world.lock() {
                world.unregister_session(self.id);
            }
        }
        if let Some(lease) = &self.registry_lease
            && let Some(registry) = lease.registry.upgrade()
        {
            registry.unregister(&lease.key);
        }
    }
}

impl std::fmt::Debug for SharedAnalyzerRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedAnalyzerRuntime")
            .field("session", &self.session_id())
            .finish_non_exhaustive()
    }
}

impl SharedAnalyzerRuntime {
    fn new(world: Arc<Mutex<SharedWorld>>, view: &WorkspaceView) -> Self {
        Self::new_with_registry(
            world,
            view.workspace_indexes().collect(),
            view.excluded_paths().to_vec(),
            None,
        )
    }

    fn new_registered(
        world: Arc<Mutex<SharedWorld>>,
        view: &WorkspaceView,
        registry: Weak<SharedAnalyzerRegistry>,
        key: SharedAnalyzerBackendKey,
    ) -> Self {
        Self::new_with_registry(
            world,
            view.workspace_indexes().collect(),
            view.excluded_paths().to_vec(),
            Some(SharedAnalyzerRegistryLease { registry, key }),
        )
    }

    fn new_with_registry(
        world: Arc<Mutex<SharedWorld>>,
        workspace_indexes: Vec<usize>,
        excluded_paths: Vec<String>,
        registry_lease: Option<SharedAnalyzerRegistryLease>,
    ) -> Self {
        let (id, activity, input_generation, access) = world
            .lock()
            .expect("shared world mutex poisoned")
            .register_session();
        let session = Arc::new(SharedAnalyzerRuntimeSession {
            world: Arc::clone(&world),
            access,
            id,
            activity,
            input_generation,
            config_generation_seen: AtomicU64::new(u64::MAX),
            edit_generation: AtomicU64::new(0),
            workspace_indexes,
            excluded_paths,
            line_endings: Mutex::new(SharedLineEndings::default()),
            file_mappings: Mutex::new(SharedFileMappings::default()),
            analysis_cache: Mutex::new(SharedAnalysisCache::default()),
            registry_lease,
        });

        let runtime = Self { world, session };
        runtime.refresh_session_cache_from_world();
        runtime
    }

    fn session_id(&self) -> u64 {
        self.session.id
    }

    pub(crate) fn set_busy(&self, busy: bool) {
        self.session.activity.store(busy, Ordering::SeqCst);
    }

    pub(crate) fn config_generation_changed(&self) -> bool {
        let generation = self.session.input_generation.load(Ordering::SeqCst);
        self.session
            .config_generation_seen
            .swap(generation, Ordering::SeqCst)
            != generation
    }

    fn workspace_indexes(&self) -> &[usize] {
        &self.session.workspace_indexes
    }

    fn refresh_session_cache_from_world(&self) {
        let world = self
            .world
            .lock()
            .expect("shared world mutex poisoned");
        self.refresh_session_cache(&world);
    }

    fn refresh_session_cache(&self, world: &SharedWorld) {
        let line_endings =
            world.line_endings_for_session(self.session_id(), self.workspace_indexes());
        let file_mappings =
            world.file_mappings_for_session(self.session_id(), self.workspace_indexes());
        *self
            .session
            .line_endings
            .lock()
            .expect("shared analyzer line endings mutex poisoned") = line_endings;
        *self
            .session
            .file_mappings
            .lock()
            .expect("shared analyzer file mappings mutex poisoned") = file_mappings;
    }

    pub(crate) fn analysis(&self) -> Analysis {
        let read_permit = self.session.access.read(self.session_id());
        let world = self
            .world
            .lock()
            .expect("shared world mutex poisoned");
        let generation = self.session.input_generation.load(Ordering::SeqCst);
        let mut cache = self
            .session
            .analysis_cache
            .lock()
            .expect("shared analyzer analysis cache mutex poisoned");
        if cache.generation != Some(generation) {
            cache.visible_files = Arc::new(world.visible_crate_roots_for_session(
                self.session_id(),
                self.workspace_indexes(),
                &self.session.excluded_paths,
            ));
            self.refresh_session_cache(&world);
            cache.generation = Some(generation);
        }
        let visible_files = Arc::clone(&cache.visible_files);
        drop(cache);
        let analysis = world
            .host
            .analyzed_analysis_with_visible_files(visible_files);
        analysis.analyzed_with_guard(read_permit)
    }

    pub(crate) fn url_to_file_id(&self, url: &Url) -> anyhow::Result<Option<FileId>> {
        let path = crate::lsp::from_proto::vfs_path(url)?;
        self.vfs_path_to_file_id(&path)
    }

    pub(crate) fn base_url_to_file_id(&self, url: &Url) -> anyhow::Result<Option<FileId>> {
        let path = crate::lsp::from_proto::vfs_path(url)?;
        self.base_vfs_path_to_file_id(&path)
    }

    pub(crate) fn vfs_path_to_file_id(&self, path: &VfsPath) -> anyhow::Result<Option<FileId>> {
        let path = normalize_vfs_path(path);
        Ok(self
            .session
            .file_mappings
            .lock()
            .expect("shared analyzer file mappings mutex poisoned")
            .file_id(&path))
    }

    pub(crate) fn base_vfs_path_to_file_id(
        &self,
        path: &VfsPath,
    ) -> anyhow::Result<Option<FileId>> {
        let path = normalize_vfs_path(path);
        Ok(self
            .session
            .file_mappings
            .lock()
            .expect("shared analyzer file mappings mutex poisoned")
            .base_file_id(&path))
    }

    pub(crate) fn file_id_to_url(&self, file_id: FileId) -> Option<Url> {
        let path = self.file_id_to_vfs_path(file_id)?;
        let path = path.as_path()?;
        Some(crate::lsp::to_proto::url_from_abs_path(path))
    }

    pub(crate) fn file_id_to_vfs_path(&self, file_id: FileId) -> Option<VfsPath> {
        self.session
            .file_mappings
            .lock()
            .expect("shared analyzer file mappings mutex poisoned")
            .path(file_id)
    }

    pub(crate) fn line_endings(&self, file_id: FileId) -> Option<crate::line_index::LineEndings> {
        self.session
            .line_endings
            .lock()
            .expect("shared analyzer line endings mutex poisoned")
            .get(file_id)
    }

    pub(crate) fn file_exists(&self, file_id: FileId) -> Option<bool> {
        self.session
            .file_mappings
            .lock()
            .expect("shared analyzer file mappings mutex poisoned")
            .exists(file_id)
    }

    pub(crate) fn ratoml_files(
        &self,
    ) -> Vec<(VfsPath, SourceRootId, bool, String)> {
        let world = self
            .world
            .lock()
            .expect("shared world mutex poisoned");
        let db = world.host.raw_database();
        let mut files = Vec::new();

        for workspace in world.loaded_workspaces_in(self.workspace_indexes()) {
            for (file_id, path) in workspace._vfs.iter() {
                if !workspace._vfs.exists(file_id)
                    || path.name_and_extension() != Some(("rust-analyzer", Some("toml")))
                {
                    continue;
                }
                let source_root_id = db.file_source_root(file_id).source_root_id(db);
                let source_root = db.source_root(source_root_id).source_root(db);
                let text = db.file_text(file_id).text(db).to_string();
                files.push((path.clone(), source_root_id, source_root.is_library, text));
            }
        }

        if let Some(overlay) = world.session_overlays.get(&self.session_id()) {
            for file in overlay.files_by_path.values() {
                if file.display_path.name_and_extension() == Some(("rust-analyzer", Some("toml"))) {
                    files.push((
                        file.display_path.clone(),
                        file.base_source_root,
                        false,
                        file.text.clone(),
                    ));
                }
            }
        }

        files
    }

    pub(crate) fn source_root_parent_map(&self) -> FxHashMap<SourceRootId, SourceRootId> {
        self.world
            .lock()
            .expect("shared world mutex poisoned")
            .source_root_parent_map(self.workspace_indexes())
    }

    pub(crate) fn source_root_for_path(&self, path: &VfsPath) -> Option<(SourceRootId, bool)> {
        self.world
            .lock()
            .expect("shared world mutex poisoned")
            .source_root_for_path(self.workspace_indexes(), path)
    }

    pub(crate) fn sync_open_files(
        &self,
        files: Vec<(
            VfsPath,
            VfsPath,
            String,
            crate::line_index::LineEndings,
        )>,
    ) -> anyhow::Result<SharedOverlaySync> {
        let _write = self.session.access.write(Some(self.session_id()));
        let mut world = self
            .world
            .lock()
            .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?;
        let sync = world.sync_session_overlay(self.session_id(), self.workspace_indexes(), files)?;
        if sync.changed {
            self.session.edit_generation.fetch_add(1, Ordering::SeqCst);
            self.session
                .analysis_cache
                .lock()
                .expect("shared analyzer analysis cache mutex poisoned")
                .generation = None;
        }
        self.refresh_session_cache(&world);
        Ok(sync)
    }

    pub(crate) fn overlay_needed(
        &self,
        files: &[(VfsPath, String, crate::line_index::LineEndings)],
    ) -> anyhow::Result<bool> {
        let world = self
            .world
            .lock()
            .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?;
        if world
            .session_overlays
            .get(&self.session_id())
            .is_some_and(|overlay| !overlay.files_by_path.is_empty())
        {
            return Ok(true);
        }

        let db = world.host.raw_database();
        for (path, text, _) in files {
            let Some(base_file) = world
                .base_file_for_vfs_path_in(self.workspace_indexes(), &normalize_vfs_path(path))
            else {
                continue;
            };
            if db.file_text(base_file).text(db).as_ref() != text.as_str() {
                return Ok(true);
            }
        }

        Ok(false)
    }

    pub(crate) fn prepare_overlay_files(
        &self,
        files: Vec<(VfsPath, String, crate::line_index::LineEndings)>,
    ) -> anyhow::Result<Vec<(VfsPath, VfsPath, String, crate::line_index::LineEndings)>> {
        let world = self
            .world
            .lock()
            .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?;
        world.prepare_session_overlay_files(self.workspace_indexes(), files)
    }
}

pub(crate) fn normalize_vfs_path(path: &VfsPath) -> VfsPath {
    let Some(path) = path.as_path() else {
        return path.clone();
    };

    VfsPath::from(AbsPathBuf::assert_utf8(normalize_fs_path(path.as_ref())))
}

// Paths reach the shared world in mixed forms: cargo metadata and clients
// report real paths while overlay-only files never hit the disk. Comparisons
// only work if every form normalizes to the same string, so the deepest
// existing ancestor is canonicalized (resolving symlinks and drive letter
// case) and the in-memory remainder is appended verbatim. canonicalize on
// Windows returns \\?\ verbatim paths, which no client-supplied path ever
// carries, so the prefix is stripped back off.
fn normalize_fs_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut remainder = Vec::new();
    let mut current = path;

    loop {
        if let Ok(canonical) = std::fs::canonicalize(current) {
            let mut normalized = strip_verbatim_prefix(canonical);
            normalized.extend(remainder.iter().rev());
            return normalized;
        }

        let (Some(parent), Some(name)) = (current.parent(), current.file_name()) else {
            return path.to_path_buf();
        };
        remainder.push(name.to_owned());
        current = parent;
    }
}

#[cfg(windows)]
fn strip_verbatim_prefix(path: std::path::PathBuf) -> std::path::PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return std::path::PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return std::path::PathBuf::from(rest.to_owned());
    }

    path
}

#[cfg(not(windows))]
fn strip_verbatim_prefix(path: std::path::PathBuf) -> std::path::PathBuf {
    path
}

fn path_key(path: &VfsPath) -> String {
    normalize_vfs_path(path).to_string()
}

fn allocate_shared_file_id() -> FileId {
    const MAX_ANALYZED_FILE_ID: u32 = 0x007F_FFFF;
    static NEXT_FILE_ID: AtomicU32 = AtomicU32::new(0);

    let file_id = NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed);
    assert!(
        file_id <= MAX_ANALYZED_FILE_ID,
        "shared analyzer file id overflowed"
    );

    FileId::from_raw(file_id)
}

#[derive(Clone, Default)]
struct SharedLineEndings {
    workspaces: Vec<Arc<BTreeMap<FileId, crate::line_index::LineEndings>>>,
    overlay: BTreeMap<FileId, crate::line_index::LineEndings>,
}

impl SharedLineEndings {
    fn get(&self, file_id: FileId) -> Option<crate::line_index::LineEndings> {
        for line_endings in &self.workspaces {
            if let Some(line_endings) = line_endings.get(&file_id) {
                return Some(*line_endings);
            }
        }

        self.overlay.get(&file_id).copied()
    }
}

#[derive(Clone, Default)]
struct SharedFileMappings {
    workspaces: Vec<Arc<LoadedWorkspaceFiles>>,
    overlay_by_path: BTreeMap<String, FileId>,
    overlay_by_file: BTreeMap<FileId, VfsPath>,
}

impl SharedFileMappings {
    fn file_id(&self, path: &VfsPath) -> Option<FileId> {
        let key = path_key(path);
        if let Some(file_id) = self.overlay_by_path.get(&key).copied() {
            return Some(file_id);
        }

        self.base_file_id(path)
    }

    fn base_file_id(&self, path: &VfsPath) -> Option<FileId> {
        self.workspaces
            .iter()
            .find_map(|workspace| workspace.file_id(path).map(|(file_id, _)| file_id))
    }

    fn path(&self, file_id: FileId) -> Option<VfsPath> {
        if let Some(path) = self.overlay_by_file.get(&file_id) {
            return Some(path.clone());
        }

        self.workspaces
            .iter()
            .find_map(|workspace| workspace.path(file_id).cloned())
    }

    fn exists(&self, file_id: FileId) -> Option<bool> {
        if self.overlay_by_file.contains_key(&file_id) {
            return Some(true);
        }

        self.workspaces.iter().find_map(|workspace| {
            workspace
                .contains_file(file_id)
                .then(|| workspace.exists(file_id))
        })
    }
}

fn common_path_prefix_len(left: &str, right: &str) -> usize {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut index = 0;
    let mut last_separator = 0;
    let end = left.len().min(right.len());

    while index < end && left[index] == right[index] {
        if left[index] == b'/' {
            last_separator = index + 1;
        }
        index += 1;
    }

    if index == end
        && (left.len() == right.len()
            || left.get(index) == Some(&b'/')
            || right.get(index) == Some(&b'/'))
    {
        return index;
    }

    last_separator
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct PackageInstanceKey {
    pub root_file: String,
    pub edition: String,
    pub origin: String,
    pub display_name: String,
    pub version: String,
    pub cfg_options: String,
    pub env: String,
    pub is_proc_macro: bool,
    pub proc_macro_cwd: String,
}

#[derive(Clone, Debug)]
pub struct PackageInstance {
    key: PackageInstanceKey,
    crates: Vec<ide::Crate>,
}

impl PackageInstance {
    fn new(key: PackageInstanceKey) -> Self {
        Self {
            key,
            crates: Vec::new(),
        }
    }

    fn push_crate(&mut self, krate: ide::Crate) {
        if !self.crates.contains(&krate) {
            self.crates.push(krate);
        }
    }

    pub fn key(&self) -> &PackageInstanceKey {
        &self.key
    }

    pub fn crates(&self) -> &[ide::Crate] {
        &self.crates
    }
}

#[derive(Clone, Debug, Default)]
pub struct SessionOverlay {
    files: Vec<SessionOverlayFile>,
    crates: Vec<SessionOverlayCrate>,
}

impl SessionOverlay {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn files(&self) -> &[SessionOverlayFile] {
        &self.files
    }

    pub fn crates(&self) -> &[SessionOverlayCrate] {
        &self.crates
    }

    pub fn push_file(&mut self, file: SessionOverlayFile) {
        if !self.files.iter().any(|it| it.base_file == file.base_file) {
            self.files.push(file);
        }
    }

    pub fn push_crate(&mut self, krate: SessionOverlayCrate) {
        if !self.crates.iter().any(|it| it.base_crate == krate.base_crate) {
            self.crates.push(krate);
        }
    }

    pub fn materialize_files(&mut self) {
        for file in &mut self.files {
            if file.session_file.is_none() {
                file.session_file = Some(allocate_shared_file_id());
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct SessionOverlayFile {
    base_file: FileId,
    session_file: Option<FileId>,
    path: VfsPath,
}

impl SessionOverlayFile {
    pub fn new(base_file: FileId, session_file: FileId, path: VfsPath) -> Self {
        Self {
            base_file,
            session_file: Some(session_file),
            path,
        }
    }

    pub fn pending(base_file: FileId, path: VfsPath) -> Self {
        Self {
            base_file,
            session_file: None,
            path,
        }
    }

    pub fn base_file(&self) -> FileId {
        self.base_file
    }

    pub fn session_file(&self) -> Option<FileId> {
        self.session_file
    }

    pub fn path(&self) -> &VfsPath {
        &self.path
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SessionOverlayCrate {
    base_crate: ide::Crate,
    session_crate: Option<ide::Crate>,
}

impl SessionOverlayCrate {
    pub fn new(base_crate: ide::Crate, session_crate: ide::Crate) -> Self {
        Self {
            base_crate,
            session_crate: Some(session_crate),
        }
    }

    pub fn pending(base_crate: ide::Crate) -> Self {
        Self {
            base_crate,
            session_crate: None,
        }
    }

    pub fn shared(krate: ide::Crate) -> Self {
        Self::new(krate, krate)
    }

    pub fn base_crate(&self) -> ide::Crate {
        self.base_crate
    }

    pub fn session_crate(&self) -> Option<ide::Crate> {
        self.session_crate
    }

    pub fn is_shared(&self) -> bool {
        self.session_crate == Some(self.base_crate)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SharedOverlaySync {
    pub(crate) changed: bool,
    pub(crate) removed_files: Vec<FileId>,
}

#[derive(Clone, Debug, Default)]
struct ActiveSessionOverlay {
    workspaces: Vec<usize>,
    open_files: BTreeMap<String, OpenOverlayFile>,
    files_by_path: BTreeMap<String, ActiveOverlayFile>,
    path_by_file: BTreeMap<FileId, String>,
    crates: BTreeMap<ide::Crate, FileId>,
}

impl ActiveSessionOverlay {
    fn file_ids(&self) -> impl Iterator<Item = FileId> + '_ {
        self.path_by_file.keys().copied()
    }

    fn file_mappings(&self) -> (BTreeMap<String, FileId>, BTreeMap<FileId, VfsPath>) {
        let by_path = self
            .files_by_path
            .iter()
            .map(|(key, file)| (key.clone(), file.overlay_file))
            .collect();
        let by_file = self
            .files_by_path
            .values()
            .map(|file| (file.overlay_file, file.display_path.clone()))
            .collect();

        (by_path, by_file)
    }
}

#[derive(Clone, Debug)]
struct OpenOverlayFile {
    overlay_file: FileId,
    text: String,
}

#[derive(Clone, Debug)]
struct ActiveOverlayFile {
    overlay_file: FileId,
    base_source_root: SourceRootId,
    path: VfsPath,
    display_path: VfsPath,
    text: String,
    line_endings: crate::line_index::LineEndings,
}

struct LoadedWorkspaceInput {
    source_roots: Vec<SourceRoot>,
    crate_graph: CrateGraphBuilder,
    proc_macros: Vec<AnalyzedProcMacroLoad>,
}

struct LoadedWorkspaceFile {
    path: VfsPath,
    exists: bool,
}

struct LoadedWorkspaceFiles {
    files_by_id: BTreeMap<FileId, LoadedWorkspaceFile>,
    file_ids_by_path: BTreeMap<String, FileId>,
}

impl LoadedWorkspaceFiles {
    fn from_vfs(vfs: &Vfs, file_id_map: &FxHashMap<FileId, FileId>) -> Self {
        let mut files_by_id = BTreeMap::new();
        let mut file_ids_by_path = BTreeMap::new();

        for (old_file_id, path) in vfs.iter() {
            let Some(&file_id) = file_id_map.get(&old_file_id) else {
                continue;
            };
            files_by_id.insert(
                file_id,
                LoadedWorkspaceFile {
                    path: path.clone(),
                    exists: vfs.exists(old_file_id),
                },
            );
            file_ids_by_path.insert(path_key(path), file_id);
        }

        Self {
            files_by_id,
            file_ids_by_path,
        }
    }

    fn iter(&self) -> impl Iterator<Item = (FileId, &VfsPath)> {
        self.files_by_id
            .iter()
            .map(|(&file_id, file)| (file_id, &file.path))
    }

    fn file_id(&self, path: &VfsPath) -> Option<(FileId, ())> {
        self.file_ids_by_path
            .get(&path_key(path))
            .copied()
            .map(|file_id| (file_id, ()))
    }

    fn exists(&self, file_id: FileId) -> bool {
        self.files_by_id
            .get(&file_id)
            .is_some_and(|file| file.exists)
    }

    fn contains_file(&self, file_id: FileId) -> bool {
        self.files_by_id.contains_key(&file_id)
    }

    fn path(&self, file_id: FileId) -> Option<&VfsPath> {
        self.files_by_id
            .get(&file_id)
            .map(|file| &file.path)
    }
}

struct LoadedWorkspace {
    summary: WorkspaceSummary,
    workspace: ProjectWorkspace,
    input: LoadedWorkspaceInput,
    _vfs: Arc<LoadedWorkspaceFiles>,
    line_endings: Arc<BTreeMap<FileId, crate::line_index::LineEndings>>,
    source_root_parent_map: FxHashMap<SourceRootId, SourceRootId>,
    _proc_macro_client: Option<ProcMacroClient>,
}

impl LoadedWorkspace {
    fn summary(&self) -> &WorkspaceSummary {
        &self.summary
    }
}

struct PreparedWorkspaceLoad {
    root_key: String,
    summary: WorkspaceSummary,
    workspace: ProjectWorkspace,
    loaded: AnalyzedWorkspaceLoad,
    line_endings: BTreeMap<FileId, crate::line_index::LineEndings>,
}

pub struct SharedWorld {
    access: Arc<SharedWorldAccess>,
    host: AnalysisHost,
    loaded_workspaces: Vec<LoadedWorkspace>,
    workspace_indexes: BTreeMap<String, usize>,
    package_instances: BTreeMap<PackageInstanceKey, PackageInstance>,
    base_crates: Vec<ide::Crate>,
    base_max_source_root: Option<u32>,
    session_overlays: BTreeMap<u64, ActiveSessionOverlay>,
    session_activity: BTreeMap<u64, Arc<AtomicBool>>,
    input_generation: Arc<AtomicU64>,
    applied_source_roots: Vec<SourceRoot>,
    applied_local_roots: rustc_hash::FxHashSet<SourceRootId>,
    applied_library_roots: rustc_hash::FxHashSet<SourceRootId>,
    applied_overlay_files: rustc_hash::FxHashSet<FileId>,
    next_session_id: u64,
}

impl SharedWorld {
    pub fn new() -> Self {
        Self {
            access: Arc::new(SharedWorldAccess::default()),
            host: AnalysisHost::with_database(RootDatabase::new(None)),
            loaded_workspaces: Vec::new(),
            workspace_indexes: BTreeMap::new(),
            package_instances: BTreeMap::new(),
            base_crates: Vec::new(),
            base_max_source_root: None,
            session_overlays: BTreeMap::new(),
            session_activity: BTreeMap::new(),
            input_generation: Arc::new(AtomicU64::new(0)),
            applied_source_roots: Vec::new(),
            applied_local_roots: rustc_hash::FxHashSet::default(),
            applied_library_roots: rustc_hash::FxHashSet::default(),
            applied_overlay_files: rustc_hash::FxHashSet::default(),
            next_session_id: 1,
        }
    }

    fn workspace_index(&self, load_key: &str) -> Option<usize> {
        self.workspace_indexes.get(load_key).copied()
    }

    fn access(&self) -> Arc<SharedWorldAccess> {
        Arc::clone(&self.access)
    }

    fn prepare_workspace_load(
        source: SharedAnalyzerWorkspaceLoadSource,
        config: &SharedAnalyzerConfig,
    ) -> anyhow::Result<PreparedWorkspaceLoad> {
        match source {
            SharedAnalyzerWorkspaceLoadSource::Project(project) => {
                let load_key = shared_project_key(&project);
                let workspace = match project {
                    crate::config::LinkedProject::ProjectManifest(manifest) => {
                        ProjectWorkspace::load(manifest, &config.cargo_config, &|_| {})?
                    }
                    crate::config::LinkedProject::InlineProjectJson(project) => {
                        ProjectWorkspace::load_inline(project, &config.cargo_config, &|_| {})
                    }
                };
                Self::prepare_loaded_workspace(load_key, workspace, config)
            }
            SharedAnalyzerWorkspaceLoadSource::DetachedFile(file) => {
                let load_key = shared_detached_file_key(&file);
                let workspace = ProjectWorkspace::load_detached_files(
                    vec![file],
                    &config.cargo_config,
                )
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::format_err!("detached file did not produce a workspace"))??;
                Self::prepare_loaded_workspace(load_key, workspace, config)
            }
        }
    }

    fn prepare_loaded_workspace(
        load_key: String,
        mut workspace: ProjectWorkspace,
        config: &SharedAnalyzerConfig,
    ) -> anyhow::Result<PreparedWorkspaceLoad> {
        let manifest_path = workspace
            .manifest()
            .map(ToString::to_string)
            .unwrap_or_else(|| workspace.workspace_root().to_string());
        let summary_root = workspace.workspace_root().to_string();
        let packages = workspace.n_packages();
        if config.load.key.load_out_dirs_from_check {
            let build_scripts = workspace.run_build_scripts(&config.cargo_config, &|_| {})?;
            workspace.set_build_scripts(build_scripts);
        }
        let workspace_for_session = workspace.clone();
        let loaded = analyzed_load_workspace_change(
            workspace,
            &config.cargo_config.extra_env,
            &config.load.to_load_cargo_config(),
            |_| allocate_shared_file_id(),
        )?;
        let files = loaded.vfs.iter().count();
        let line_endings = loaded
            .file_texts
            .iter()
            .map(|(file_id, text)| {
                let (_, line_endings) =
                    crate::line_index::LineEndings::normalize(text.clone());
                (*file_id, line_endings)
            })
            .collect();
        let proc_macro_server = loaded.proc_macro_server.is_some();

        Ok(PreparedWorkspaceLoad {
            root_key: load_key,
            summary: WorkspaceSummary {
                root: summary_root,
                manifest: manifest_path,
                packages,
                files,
                proc_macro_server,
            },
            workspace: workspace_for_session,
            loaded,
            line_endings,
        })
    }

    fn commit_workspace(
        &mut self,
        loaded: PreparedWorkspaceLoad,
        reload: bool,
    ) -> anyhow::Result<usize> {
        let result = self.commit_workspace_inner(loaded, reload);
        if result.is_ok() {
            self.input_generation.fetch_add(1, Ordering::SeqCst);
        }
        result
    }

    fn commit_workspace_inner(
        &mut self,
        loaded: PreparedWorkspaceLoad,
        reload: bool,
    ) -> anyhow::Result<usize> {
        if let Some(&index) = self.workspace_indexes.get(&loaded.root_key) {
            if !reload {
                return Ok(index);
            }

            let (workspace, file_texts) = self.remap_workspace_load(loaded);
            let removed_files = self.loaded_workspaces[index]
                ._vfs
                .iter()
                .map(|(file_id, _)| file_id)
                .collect::<Vec<_>>();
            self.loaded_workspaces[index] = workspace;
            let (source_roots, mut change) = self.base_input_change(file_texts);
            for file_id in removed_files {
                change.change_file(file_id, None);
            }

            self.host.raw_database_mut().enable_proc_attr_macros();
            self.apply_source_roots(source_roots);
            self.host.apply_change(change);
            self.refresh_base_inputs();
            let removed_overlay_files = self.recone_session_overlays()?;
            self.rebuild_overlay_inputs(removed_overlay_files)?;
            self.refresh_package_instances()?;
            return Ok(index);
        }

        let root_key = loaded.root_key.clone();
        let (workspace, file_texts) = self.remap_workspace_load(loaded);
        let index = self.loaded_workspaces.len();
        self.workspace_indexes.insert(root_key, index);
        self.loaded_workspaces.push(workspace);
        let (source_roots, change) = self.base_input_change(file_texts);

        self.host.raw_database_mut().enable_proc_attr_macros();
        self.apply_source_roots(source_roots);
        self.host.apply_change(change);
        self.refresh_base_inputs();
        self.refresh_package_instances()?;
        Ok(index)
    }

    fn remap_workspace_load(
        &self,
        loaded: PreparedWorkspaceLoad,
    ) -> (LoadedWorkspace, Vec<(FileId, String)>) {
        let files = LoadedWorkspaceFiles::from_vfs(&loaded.loaded.vfs, &loaded.loaded.file_id_map);
        let file_texts = loaded.loaded.file_texts.clone();
        let line_endings = Arc::new(loaded.line_endings.into_iter().collect());
        let source_root_parent_map = loaded.loaded.source_root_parent_map.into_iter().collect();

        (
            LoadedWorkspace {
                summary: loaded.summary,
                workspace: loaded.workspace,
                input: LoadedWorkspaceInput {
                    source_roots: loaded.loaded.source_roots,
                    crate_graph: loaded.loaded.crate_graph,
                    proc_macros: loaded.loaded.proc_macros,
                },
                _vfs: Arc::new(files),
                line_endings,
                source_root_parent_map,
                _proc_macro_client: loaded.loaded.proc_macro_server,
            },
            file_texts,
        )
    }

    fn base_input_change(
        &self,
        file_texts: Vec<(FileId, String)>,
    ) -> (Vec<SourceRoot>, ChangeWithProcMacros) {
        let mut change = ChangeWithProcMacros::default();
        let mut source_roots = Vec::new();
        let mut crate_graph = CrateGraphBuilder::default();
        let mut proc_macros = ProcMacrosBuilder::default();

        for input in self
            .loaded_workspaces
            .iter()
            .map(|workspace| &workspace.input)
        {
            source_roots.extend(input.source_roots.iter().cloned());
            let mut proc_macro_paths = ProcMacroPaths::default();
            let crate_id_map = crate_graph.extend(input.crate_graph.clone(), &mut proc_macro_paths);
            for (crate_id, proc_macro) in &input.proc_macros {
                if let Some(crate_id) = crate_id_map.get(crate_id).copied() {
                    proc_macros.insert(crate_id, proc_macro.clone());
                }
            }
        }

        change.set_crate_graph(crate_graph);
        change.set_proc_macros(proc_macros);
        for (file_id, text) in file_texts {
            change.change_file(file_id, Some(text));
        }

        (source_roots, change)
    }

    fn apply_source_roots(&mut self, roots: Vec<SourceRoot>) {
        let db = self.host.raw_database_mut();
        let mut local_roots = rustc_hash::FxHashSet::default();
        let mut library_roots = rustc_hash::FxHashSet::default();
        for (index, root) in roots.iter().enumerate() {
            let root_id = SourceRootId(index as u32);
            if root.is_library {
                library_roots.insert(root_id);
            } else {
                local_roots.insert(root_id);
            }
        }

        for (index, root) in roots.into_iter().enumerate() {
            let root_id = SourceRootId(index as u32);
            let previous = self.applied_source_roots.get(index);
            if previous == Some(&root) {
                continue;
            }

            let durability = if root.is_library {
                Durability::MEDIUM
            } else {
                Durability::LOW
            };
            for file_id in root.iter() {
                if previous.is_none_or(|previous| previous.path_for_file(&file_id).is_none()) {
                    db.set_file_source_root_with_durability(file_id, root_id, durability);
                }
            }
            db.set_source_root_with_durability(
                root_id,
                triomphe::Arc::new(root.clone()),
                durability,
            );
            if index < self.applied_source_roots.len() {
                self.applied_source_roots[index] = root;
            } else {
                self.applied_source_roots.push(root);
            }
        }

        if self.applied_local_roots != local_roots {
            LocalRoots::get(db).set_roots(db).to(local_roots.clone());
            self.applied_local_roots = local_roots;
        }
        if self.applied_library_roots != library_roots {
            LibraryRoots::get(db).set_roots(db).to(library_roots.clone());
            self.applied_library_roots = library_roots;
        }
    }

    fn register_session(&mut self) -> (u64, Arc<AtomicBool>, Arc<AtomicU64>, Arc<SharedWorldAccess>) {
        let id = self.next_session_id;
        self.next_session_id += 1;
        self.session_overlays
            .insert(id, ActiveSessionOverlay::default());
        let activity = Arc::new(AtomicBool::new(true));
        self.session_activity.insert(id, Arc::clone(&activity));
        (id, activity, Arc::clone(&self.input_generation), self.access())
    }

    fn unregister_session(&mut self, session_id: u64) {
        self.session_activity.remove(&session_id);
        let old_files = self
            .session_overlays
            .remove(&session_id)
            .into_iter()
            .flat_map(|overlay| overlay.file_ids().collect::<Vec<_>>())
            .collect::<Vec<_>>();
        if !old_files.is_empty()
            && let Err(error) = self.rebuild_overlay_inputs(old_files)
        {
            tracing::error!("failed to unregister shared analyzer session {session_id}: {error}");
        }
    }

    pub fn workspace_summary(&self, index: usize) -> Option<&WorkspaceSummary> {
        self.loaded_workspaces.get(index).map(LoadedWorkspace::summary)
    }

    pub fn workspaces(&self, view: &WorkspaceView) -> Vec<ProjectWorkspace> {
        view.workspace_indexes()
            .filter_map(|index| self.loaded_workspaces.get(index))
            .map(|workspace| workspace.workspace.clone())
            .collect()
    }

    fn workspace_summaries(&self, view: &WorkspaceView) -> Vec<WorkspaceSummary> {
        view.workspace_indexes()
            .filter_map(|index| self.loaded_workspaces.get(index))
            .map(|workspace| workspace.summary().clone())
            .collect()
    }

    fn loaded_workspaces_in<'a>(
        &'a self,
        workspaces: &'a [usize],
    ) -> impl Iterator<Item = &'a LoadedWorkspace> + 'a {
        workspaces
            .iter()
            .filter_map(|&index| self.loaded_workspaces.get(index))
    }

    fn synthetic_write(&mut self) {
        self.host
            .raw_database_mut()
            .synthetic_write(Durability::LOW);
    }

    fn any_session_busy(&self) -> bool {
        self.session_activity
            .values()
            .any(|activity| activity.load(Ordering::SeqCst))
    }

    pub fn workspace_file(&self, path: impl AsRef<Path>) -> anyhow::Result<(FileId, VfsPath)> {
        self.workspace_file_in(0..self.loaded_workspaces.len(), path)
    }

    fn workspace_file_in(
        &self,
        workspaces: impl IntoIterator<Item = usize>,
        path: impl AsRef<Path>,
    ) -> anyhow::Result<(FileId, VfsPath)> {
        let path = VfsPath::from(AbsPathBuf::assert_utf8(std::fs::canonicalize(path)?));

        for workspace in workspaces {
            let Some(workspace) = self.loaded_workspaces.get(workspace) else {
                continue;
            };
            for (file_id, vfs_path) in workspace._vfs.iter() {
                if *vfs_path == path {
                    return Ok((file_id, vfs_path.clone()));
                }
            }
        }

        anyhow::bail!("workspace file is not loaded: {path}")
    }

    fn line_endings_for_session(
        &self,
        session_id: u64,
        workspaces: &[usize],
    ) -> SharedLineEndings {
        let workspaces = self
            .loaded_workspaces_in(workspaces)
            .map(|workspace| Arc::clone(&workspace.line_endings))
            .collect();
        let mut overlay_line_endings = BTreeMap::new();

        if let Some(session_overlay) = self.session_overlays.get(&session_id) {
            overlay_line_endings.extend(session_overlay.files_by_path.values().map(|file| {
                (file.overlay_file, file.line_endings)
            }));
        }

        SharedLineEndings {
            workspaces,
            overlay: overlay_line_endings,
        }
    }

    fn file_mappings_for_session(
        &self,
        session_id: u64,
        workspaces: &[usize],
    ) -> SharedFileMappings {
        let workspaces = self
            .loaded_workspaces_in(workspaces)
            .map(|workspace| Arc::clone(&workspace._vfs))
            .collect();
        let (overlay_by_path, overlay_by_file) = self
            .session_overlays
            .get(&session_id)
            .map(ActiveSessionOverlay::file_mappings)
            .unwrap_or_default();

        SharedFileMappings {
            workspaces,
            overlay_by_path,
            overlay_by_file,
        }
    }

    pub(crate) fn source_root_parent_map(
        &self,
        workspaces: &[usize],
    ) -> FxHashMap<SourceRootId, SourceRootId> {
        let mut map = FxHashMap::default();
        let mut offset = 0;
        for (index, workspace) in self.loaded_workspaces.iter().enumerate() {
            if workspaces.contains(&index) {
                map.extend(workspace.source_root_parent_map.iter().map(
                    |(&source_root, &parent)| {
                        (
                            SourceRootId(source_root.0 + offset),
                            SourceRootId(parent.0 + offset),
                        )
                    },
                ));
            }
            offset += workspace.input.source_roots.len() as u32;
        }

        map
    }

    pub(crate) fn source_root_for_path(
        &self,
        workspaces: &[usize],
        path: &VfsPath,
    ) -> Option<(SourceRootId, bool)> {
        let path = normalize_vfs_path(path);
        if let Some(file_id) = self.base_file_for_vfs_path_in(workspaces, &path) {
            let db = self.host.raw_database();
            let source_root_id = db.file_source_root(file_id).source_root_id(db);
            let source_root = db.source_root(source_root_id).source_root(db);
            return Some((source_root_id, source_root.is_library));
        }

        let path = path_key(&path);
        let mut best = None::<(usize, SourceRootId, bool)>;
        for workspace in self.loaded_workspaces_in(workspaces) {
            for (file_id, file_path) in workspace._vfs.iter() {
                let len = common_path_prefix_len(&path, &path_key(file_path));
                if len == 0 {
                    continue;
                }

                let replace = match best {
                    Some((best_len, _, _)) => len > best_len,
                    None => true,
                };
                if replace {
                    let db = self.host.raw_database();
                    let source_root_id = db.file_source_root(file_id).source_root_id(db);
                    let source_root = db.source_root(source_root_id).source_root(db);
                    best = Some((len, source_root_id, source_root.is_library));
                }
            }
        }

        best.map(|(_, source_root_id, is_library)| (source_root_id, is_library))
    }

    pub(crate) fn sync_session_overlay(
        &mut self,
        session_id: u64,
        workspaces: &[usize],
        files: Vec<(
            VfsPath,
            VfsPath,
            String,
            crate::line_index::LineEndings,
        )>,
    ) -> anyhow::Result<SharedOverlaySync> {
        let open_files = files
            .into_iter()
            .filter_map(|(path, display_path, text, line_endings)| {
                let key = path_key(&path);
                self.base_file_for_vfs_path_in(workspaces, &normalize_vfs_path(&path))
                    .and_then(|base_file| {
                        let db = self.host.raw_database();
                        let base_text = db.file_text(base_file).text(db);
                        if base_text.as_ref() == text.as_str() {
                            return None;
                        }
                        Some(
                        (
                            key,
                            (path, display_path, base_file, text, line_endings),
                        )
                        )
                    })
            })
            .collect::<BTreeMap<_, _>>();

        let old_overlay = self.session_overlays.remove(&session_id).unwrap_or_default();
        let same_open_files = old_overlay.open_files.len() == open_files.len()
            && open_files
                .iter()
                .all(|(key, (_, _, _, text, _))| {
                    old_overlay
                        .open_files
                        .get(key)
                        .is_some_and(|old| old.text == *text)
            });
        if same_open_files {
            self.session_overlays.insert(session_id, old_overlay);
            return Ok(SharedOverlaySync {
                changed: false,
                removed_files: Vec::new(),
            });
        }

        let same_file_set = old_overlay.open_files.len() == open_files.len()
            && open_files
                .keys()
                .all(|key| old_overlay.open_files.contains_key(key));
        if same_file_set {
            let mut overlay = old_overlay;
            let mut change = ChangeWithProcMacros::default();
            for (key, (_, _, _, text, line_endings)) in open_files {
                let open = overlay
                    .open_files
                    .get_mut(&key)
                    .expect("overlay file set was checked");
                if open.text == text {
                    continue;
                }
                open.text = text.clone();
                let file = overlay
                    .files_by_path
                    .get_mut(&key)
                    .expect("overlay file set was checked");
                file.text = text.clone();
                file.line_endings = line_endings;
                change.change_file(open.overlay_file, Some(text));
            }
            self.session_overlays.insert(session_id, overlay);
            self.host.apply_change(change);
            return Ok(SharedOverlaySync {
                changed: true,
                removed_files: Vec::new(),
            });
        }

        let kept_keys = open_files.keys().cloned().collect::<BTreeSet<_>>();
        let removed_file_ids = old_overlay
            .open_files
            .iter()
            .filter_map(|(key, file)| (!kept_keys.contains(key)).then_some(file.overlay_file))
            .collect::<Vec<_>>();
        let mut overlay = ActiveSessionOverlay {
            workspaces: workspaces.to_vec(),
            ..ActiveSessionOverlay::default()
        };

        for (key, (path, display_path, base_file, text, line_endings)) in open_files {
            let overlay_file = old_overlay
                .open_files
                .get(&key)
                .map(|file| file.overlay_file)
            .unwrap_or_else(|| self.allocate_overlay_file_id());
            if old_overlay
                .open_files
                .get(&key)
                .is_some_and(|old| old.text != text)
            {
                self.applied_overlay_files.remove(&overlay_file);
            }
            overlay.open_files.insert(
                key.clone(),
                OpenOverlayFile {
                    overlay_file,
                    text: text.clone(),
                },
            );
            overlay.path_by_file.insert(overlay_file, key.clone());
            overlay.files_by_path.insert(
                key,
                ActiveOverlayFile {
                    overlay_file,
                    base_source_root: self.source_root_for_file(base_file)?,
                    path,
                    display_path,
                    text,
                    line_endings,
                },
            );
            self.populate_overlay_crates(&mut overlay, base_file)?;
        }

        self.session_overlays.insert(session_id, overlay);
        self.rebuild_overlay_inputs(removed_file_ids.clone())?;

        Ok(SharedOverlaySync {
            changed: true,
            removed_files: removed_file_ids,
        })
    }

    pub(crate) fn prepare_session_overlay_files(
        &self,
        workspaces: &[usize],
        files: Vec<(VfsPath, String, crate::line_index::LineEndings)>,
    ) -> anyhow::Result<Vec<(VfsPath, VfsPath, String, crate::line_index::LineEndings)>> {
        let open_files = files
            .into_iter()
            .map(|(path, text, line_endings)| {
                let source_path = normalize_vfs_path(&path);
                (path_key(&source_path), (path, text, line_endings))
            })
            .collect::<BTreeMap<_, _>>();
        let mut required_files = BTreeMap::<String, VfsPath>::new();
        let db = self.host.raw_database();

        for (path, _, _) in open_files.values() {
            let source_path = normalize_vfs_path(path);
            let Some(base_file) = self.base_file_for_vfs_path_in(workspaces, &source_path) else {
                continue;
            };

            for krate in self.host.analysis().crates_for(base_file)? {
                let root_file = krate.data(db).root_file_id;
                let source_root_id = self.source_root_for_file(root_file)?;
                let source_root = db.source_root(source_root_id).source_root(db);

                for file_id in source_root.iter() {
                    let Some(path) = source_root.path_for_file(&file_id).cloned() else {
                        continue;
                    };
                    required_files.entry(path_key(&path)).or_insert(path);
                }
            }
        }

        let mut prepared = Vec::new();
        for (key, path) in required_files {
            if let Some((display_path, text, line_endings)) = open_files.get(&key) {
                prepared.push((path, display_path.clone(), text.clone(), *line_endings));
                continue;
            }

            let Some(base_file) = self.base_file_for_vfs_path_in(workspaces, &path) else {
                continue;
            };
            let text = db.file_text(base_file).text(db).to_string();
            let line_endings = self
                .loaded_workspaces_in(workspaces)
                .find_map(|workspace| workspace.line_endings.get(&base_file).copied())
                .unwrap_or_else(|| {
                    let (_, line_endings) =
                        crate::line_index::LineEndings::normalize(text.clone());
                    line_endings
                });
            prepared.push((path.clone(), path, text, line_endings));
        }

        Ok(prepared)
    }

    fn populate_overlay_crates(
        &self,
        overlay: &mut ActiveSessionOverlay,
        base_file: FileId,
    ) -> anyhow::Result<()> {
        let db = self.host.raw_database();

        for krate in self.host.analysis().crates_for(base_file)? {
            let root_file = krate.data(db).root_file_id;
            let source_root_id = self.source_root_for_file(root_file)?;
            let source_root = db.source_root(source_root_id).source_root(db);

            let Some(root_key) = source_root.path_for_file(&root_file).map(path_key) else {
                continue;
            };
            let Some(root_overlay_file) = overlay
                .files_by_path
                .get(&root_key)
                .map(|file| file.overlay_file)
            else {
                continue;
            };
            overlay.crates.insert(krate, root_overlay_file);
        }

        Ok(())
    }

    fn rebuild_overlay_inputs(&mut self, removed_file_ids: Vec<FileId>) -> anyhow::Result<()> {
        let source_roots = self.overlay_source_roots()?;
        let mut change = ChangeWithProcMacros::default();
        change.set_crate_graph(self.overlay_crate_graph()?);

        for file_id in removed_file_ids {
            self.applied_overlay_files.remove(&file_id);
            change.change_file(file_id, None);
        }

        let mut added_files = Vec::new();
        for overlay in self.session_overlays.values() {
            for file in overlay.files_by_path.values() {
                if !self.applied_overlay_files.contains(&file.overlay_file) {
                    added_files.push(file.overlay_file);
                    change.change_file(file.overlay_file, Some(file.text.clone()));
                }
            }
        }
        self.applied_overlay_files.extend(added_files);

        self.apply_source_roots(source_roots);
        self.host.apply_change(change);
        Ok(())
    }

    fn recone_session_overlays(&mut self) -> anyhow::Result<Vec<FileId>> {
        let session_ids = self.session_overlays.keys().copied().collect::<Vec<_>>();
        let mut removed_files = Vec::new();

        for session_id in session_ids {
            let old_overlay = self
                .session_overlays
                .remove(&session_id)
                .unwrap_or_default();
            let mut overlay = ActiveSessionOverlay {
                workspaces: old_overlay.workspaces.clone(),
                ..ActiveSessionOverlay::default()
            };

            for (key, file) in &old_overlay.files_by_path {
                let base_file = self.base_file_for_vfs_path_in(
                    &old_overlay.workspaces,
                    &normalize_vfs_path(&file.path),
                );
                let Some(base_file) = base_file else {
                    removed_files.push(file.overlay_file);
                    continue;
                };
                let base_text = {
                    let db = self.host.raw_database();
                    db.file_text(base_file).text(db)
                };
                if base_text.as_ref() == file.text.as_str() {
                    removed_files.push(file.overlay_file);
                    continue;
                }

                if let Some(open) = old_overlay.open_files.get(key) {
                    overlay.open_files.insert(
                        key.clone(),
                        OpenOverlayFile {
                            overlay_file: open.overlay_file,
                            text: open.text.clone(),
                        },
                    );
                }
                overlay.path_by_file.insert(file.overlay_file, key.clone());
                overlay.files_by_path.insert(
                    key.clone(),
                    ActiveOverlayFile {
                        overlay_file: file.overlay_file,
                        base_source_root: self.source_root_for_file(base_file)?,
                        path: file.path.clone(),
                        display_path: file.display_path.clone(),
                        text: file.text.clone(),
                        line_endings: file.line_endings,
                    },
                );
                self.populate_overlay_crates(&mut overlay, base_file)?;
            }

            self.session_overlays.insert(session_id, overlay);
        }

        Ok(removed_files)
    }

    fn overlay_source_roots(&self) -> anyhow::Result<Vec<SourceRoot>> {
        let db = self.host.raw_database();
        let mut roots = match self.base_max_source_root {
            Some(max_base_root) => (0..=max_base_root)
                .map(|index| {
                    db.source_root(SourceRootId(index))
                        .source_root(db)
                        .as_ref()
                        .clone()
                })
                .collect::<Vec<_>>(),
            None => Vec::new(),
        };

        for overlay in self.session_overlays.values() {
            let mut files_by_root = BTreeMap::<SourceRootId, FileSet>::new();
            for file in overlay.files_by_path.values() {
                files_by_root
                    .entry(file.base_source_root)
                    .or_default()
                    .insert(file.overlay_file, file.path.clone());
            }

            for (base_source_root, file_set) in files_by_root {
                let base = db.source_root(base_source_root).source_root(db);
                let root = if base.is_library {
                    SourceRoot::new_library(file_set)
                } else {
                    SourceRoot::new_local(file_set)
                };
                roots.push(root);
            }
        }

        Ok(roots)
    }

    fn overlay_crate_graph(&self) -> anyhow::Result<CrateGraphBuilder> {
        let db = self.host.raw_database();
        let mut graph = CrateGraphBuilder::default();
        let mut base_builders = BTreeMap::new();
        for krate in &self.base_crates {
            let data = krate.data(db);
            let extra = krate.extra_data(db);
            let builder = graph.add_crate_root(
                data.root_file_id,
                data.edition,
                extra.display_name.clone(),
                extra.version.clone(),
                krate.cfg_options(db).clone(),
                extra.potential_cfg_options.clone(),
                krate.env(db).clone(),
                data.origin.clone(),
                data.crate_attrs.iter().map(|it| it.to_string()).collect(),
                data.is_proc_macro,
                data.proc_macro_cwd.clone(),
                krate.workspace_data(db).clone(),
            );
            base_builders.insert(*krate, builder);
        }

        let mut overlay_builders = BTreeMap::new();
        for (session_id, overlay) in &self.session_overlays {
            for (base_crate, root_file_id) in &overlay.crates {
                let data = base_crate.data(db);
                let extra = base_crate.extra_data(db);
                let builder = graph.add_crate_root(
                    *root_file_id,
                    data.edition,
                    extra.display_name.clone(),
                    extra.version.clone(),
                    base_crate.cfg_options(db).clone(),
                    extra.potential_cfg_options.clone(),
                    base_crate.env(db).clone(),
                    data.origin.clone(),
                    data.crate_attrs.iter().map(|it| it.to_string()).collect(),
                    data.is_proc_macro,
                    data.proc_macro_cwd.clone(),
                    base_crate.workspace_data(db).clone(),
                );
                overlay_builders.insert((*session_id, *base_crate), builder);
            }
        }

        for krate in &self.base_crates {
            let Some(from) = base_builders.get(krate).copied() else {
                continue;
            };
            for dependency in &krate.data(db).dependencies {
                if let Some(to) = base_builders.get(&dependency.crate_id).copied() {
                    graph.add_dep(
                        from,
                        DependencyBuilder::with_prelude(
                            dependency.name.clone(),
                            to,
                            dependency.is_prelude(),
                            dependency.is_sysroot(),
                        ),
                    )
                    .map_err(|error| anyhow::format_err!("{error:?}"))?;
                }
            }
        }

        for ((session_id, base_crate), from) in &overlay_builders {
            for dependency in &base_crate.data(db).dependencies {
                let to = overlay_builders
                    .get(&(*session_id, dependency.crate_id))
                    .or_else(|| base_builders.get(&dependency.crate_id))
                    .copied();
                if let Some(to) = to {
                    graph.add_dep(
                        *from,
                        DependencyBuilder::with_prelude(
                            dependency.name.clone(),
                            to,
                            dependency.is_prelude(),
                            dependency.is_sysroot(),
                        ),
                    )
                    .map_err(|error| anyhow::format_err!("{error:?}"))?;
                }
            }
        }

        graph.shrink_to_fit();
        Ok(graph)
    }

    fn allocate_overlay_file_id(&mut self) -> FileId {
        allocate_shared_file_id()
    }

    fn refresh_base_inputs(&mut self) {
        let db = self.host.raw_database();
        let overlay_files = self
            .session_overlays
            .values()
            .flat_map(ActiveSessionOverlay::file_ids)
            .collect::<BTreeSet<_>>();

        self.base_crates = all_crates(db)
            .iter()
            .copied()
            .filter(|krate| !overlay_files.contains(&krate.data(db).root_file_id))
            .collect();

        self.base_max_source_root = None;
        for workspace in &self.loaded_workspaces {
            for (file_id, _) in workspace._vfs.iter() {
                let source_root = db.file_source_root(file_id).source_root_id(db);
                self.base_max_source_root = Some(
                    self.base_max_source_root
                        .map_or(source_root.0, |max| max.max(source_root.0)),
                );
            }
        }
    }

    fn visible_crate_roots_for_session(
        &self,
        session_id: u64,
        workspaces: &[usize],
        excluded_paths: &[String],
    ) -> rustc_hash::FxHashSet<FileId> {
        let db = self.host.raw_database();
        let overlay = self.session_overlays.get(&session_id);
        let overlay_base_crates = overlay
            .map(|overlay| overlay.crates.keys().copied().collect::<BTreeSet<_>>())
            .unwrap_or_default();
        let view_workspaces = self
            .loaded_workspaces_in(workspaces)
            .collect::<Vec<_>>();
        let mut visible_files = rustc_hash::FxHashSet::default();

        for krate in &self.base_crates {
            let root_file = krate.data(db).root_file_id;
            if view_workspaces
                .iter()
                .any(|workspace| workspace._vfs.contains_file(root_file))
                && !overlay_base_crates.contains(krate)
                && !self.file_is_excluded(root_file, excluded_paths)
            {
                visible_files.insert(root_file);
            }
        }

        if let Some(overlay) = overlay {
            visible_files.extend(overlay.crates.values().copied());
        }

        visible_files
    }

    fn file_is_excluded(&self, file_id: FileId, excluded_paths: &[String]) -> bool {
        if excluded_paths.is_empty() {
            return false;
        }

        let Some(path) = self
            .loaded_workspaces
            .iter()
            .find_map(|workspace| workspace._vfs.path(file_id))
        else {
            return false;
        };
        let path = path_key(path);
        excluded_paths
            .iter()
            .any(|excluded| path.starts_with(excluded))
    }

    fn base_file_for_vfs_path_in(&self, workspaces: &[usize], path: &VfsPath) -> Option<FileId> {
        self.loaded_workspaces_in(workspaces)
            .find_map(|workspace| workspace._vfs.file_id(path).map(|(file_id, _)| file_id))
    }

    fn source_root_for_file(&self, file_id: FileId) -> anyhow::Result<SourceRootId> {
        Ok(self
            .host
            .raw_database()
            .file_source_root(file_id)
            .source_root_id(self.host.raw_database()))
    }

    pub fn crate_root_file(&self, krate: ide::Crate) -> anyhow::Result<(FileId, VfsPath)> {
        let db = self.host.raw_database();
        let file_id = krate.data(db).root_file_id;
        let path = path_for_file(db, file_id)?;

        Ok((file_id, VfsPath::new_real_path(path)))
    }

    pub fn package_instances(&self) -> impl Iterator<Item = &PackageInstance> {
        self.package_instances.values()
    }

    pub fn active_overlay_sessions(&self) -> usize {
        self.session_overlays.len()
    }

    pub fn overlay_files(&self) -> usize {
        self.session_overlays
            .values()
            .flat_map(ActiveSessionOverlay::file_ids)
            .count()
    }

    pub fn crates_for_file(&self, file_id: FileId) -> anyhow::Result<Vec<ide::Crate>> {
        Ok(self.host.analysis().crates_for(file_id)?)
    }

    pub fn shared_dependencies(&self, krate: ide::Crate) -> anyhow::Result<Vec<ide::Crate>> {
        let db = self.host.raw_database();
        let mut dependencies = Vec::new();

        for dependency in &krate.data(db).dependencies {
            dependencies.push(self.interned_crate(dependency.crate_id)?);
        }

        Ok(dependencies)
    }

    fn interned_crate(&self, krate: ide::Crate) -> anyhow::Result<ide::Crate> {
        let db = self.host.raw_database();
        let key = package_instance_key(db, krate)?;
        let package = self
            .package_instances
            .get(&key)
            .ok_or_else(|| anyhow::format_err!("package instance is not interned: {:?}", key))?;

        if package.crates().contains(&krate) {
            Ok(krate)
        } else {
            anyhow::bail!("crate is not interned in package instance: {:?}", key)
        }
    }

    fn refresh_package_instances(&mut self) -> anyhow::Result<()> {
        let db = self.host.raw_database();
        self.package_instances.clear();

        for krate in self.base_crates.iter().copied() {
            let key = package_instance_key(db, krate)?;
            match self.package_instances.entry(key.clone()) {
                Entry::Occupied(mut entry) => entry.get_mut().push_crate(krate),
                Entry::Vacant(entry) => {
                    let mut package = PackageInstance::new(key);
                    package.push_crate(krate);
                    entry.insert(package);
                }
            }
        }

        Ok(())
    }
}

impl Default for SharedWorld {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
pub struct WorkspaceView {
    workspaces: Vec<usize>,
    excluded_paths: Vec<String>,
}

impl WorkspaceView {
    pub fn new(workspaces: Vec<usize>, excluded_paths: Vec<String>) -> Self {
        Self {
            workspaces,
            excluded_paths,
        }
    }

    pub fn push_workspace(&mut self, workspace: usize) {
        self.workspaces.push(workspace);
    }

    pub fn workspace_indexes(&self) -> impl Iterator<Item = usize> + '_ {
        self.workspaces.iter().copied()
    }

    pub fn excluded_paths(&self) -> &[String] {
        &self.excluded_paths
    }

    pub fn workspace_summaries<'a>(
        &'a self,
        world: &'a SharedWorld,
    ) -> impl Iterator<Item = &'a WorkspaceSummary> {
        self.workspaces
            .iter()
            .filter_map(|index| world.workspace_summary(*index))
    }

    pub fn workspace_file(
        &self,
        world: &SharedWorld,
        path: impl AsRef<Path>,
    ) -> anyhow::Result<(FileId, VfsPath)> {
        world.workspace_file_in(self.workspaces.iter().copied(), path)
    }

    pub fn overlay_cone(
        &self,
        world: &SharedWorld,
        path: impl AsRef<Path>,
    ) -> anyhow::Result<SessionOverlay> {
        let (base_file, vfs_path) = self.workspace_file(world, path)?;
        let mut overlay = SessionOverlay::new();
        overlay.push_file(SessionOverlayFile::pending(base_file, vfs_path));

        for krate in world.crates_for_file(base_file)? {
            overlay.push_crate(SessionOverlayCrate::pending(krate));
            let (root_file, root_path) = world.crate_root_file(krate)?;
            overlay.push_file(SessionOverlayFile::pending(root_file, root_path));

            for dependency in world.shared_dependencies(krate)? {
                overlay.push_crate(SessionOverlayCrate::shared(dependency));
            }
        }

        overlay.materialize_files();

        Ok(overlay)
    }
}

fn path_for_file(db: &RootDatabase, file_id: FileId) -> anyhow::Result<String> {
    let root = db.file_source_root(file_id).source_root_id(db);
    db.source_root(root)
        .source_root(db)
        .path_for_file(&file_id)
        .map(ToString::to_string)
        .ok_or_else(|| anyhow::format_err!("file path is unavailable for {file_id:?}"))
}

fn package_instance_key(db: &RootDatabase, krate: ide::Crate) -> anyhow::Result<PackageInstanceKey> {
    let data = krate.data(db);

    Ok(PackageInstanceKey {
        root_file: path_for_file(db, data.root_file_id)?,
        edition: format!("{:?}", data.edition),
        origin: format!("{:?}", data.origin),
        display_name: format!("{:?}", krate.extra_data(db).display_name),
        version: format!("{:?}", krate.extra_data(db).version),
        cfg_options: format!("{:?}", krate.cfg_options(db)),
        env: format!("{:?}", krate.env(db)),
        is_proc_macro: data.is_proc_macro,
        proc_macro_cwd: format!("{:?}", data.proc_macro_cwd),
    })
}
