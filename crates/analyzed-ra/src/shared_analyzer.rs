use std::{
    collections::{BTreeMap, BTreeSet, VecDeque, btree_map::Entry},
    env, fmt,
    path::PathBuf,
    sync::{
        Arc, Condvar, LazyLock, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
};

use hir::{ChangeWithProcMacros, ProcMacrosBuilder};
use ide::{Analysis, AnalysisHost, FileId};
use ide_db::{
    FxHashMap, FxHashSet,
    base_db::{
        CrateGraphBuilder, DependencyBuilder, FileSet, LibraryRoots, LocalRoots,
        ProcMacroLoadingError, ProcMacroPaths, SourceDatabase, SourceRoot, SourceRootId,
        all_crates,
        salsa::{Durability, Revision, Setter as _},
    },
};
use load_cargo::{
    LoadCargoConfig, ProcMacroLoad, ProcMacroLoadState, ProcMacroServerChoice, SourceRootConfig,
    WorkspaceLoad, collect_proc_macros, load_workspace_change, source_root_for_path,
    source_roots_for_files, workspace_source_root_config,
};
use lsp_types::Uri;
use proc_macro_api::ProcMacroClient;
use project_model::{
    CargoConfig, ManifestPath, ProjectWorkspace, ProjectWorkspaceKind, WorkspaceBuildScripts,
};
use vfs::{AbsPathBuf, Vfs, VfsPath};

pub static RUST_ANALYZER_VERSION: LazyLock<String> = LazyLock::new(|| {
    let commit = env!("ANALYZED_RA_COMMIT_HASH");
    format!(
        "{} {}",
        env!("ANALYZED_RA_RELEASE_VERSION"),
        &commit[..8]
    )
});

pub fn run_shared_rust_analyzer_lsp_session(
    connection: lsp_server::Connection,
) -> anyhow::Result<()> {
    crate::main_loop::session::run_shared_lsp_session(connection)
}

pub fn run_shared_rust_analyzer_lsp_session_with_config(
    config: crate::config::Config,
    connection: lsp_server::Connection,
) -> anyhow::Result<()> {
    crate::main_loop::session::run_shared_lsp_session_with_config(config, connection)
}

pub fn shared_analyzer_registry() -> Arc<SharedAnalyzerRegistry> {
    static REGISTRY: OnceLock<Arc<SharedAnalyzerRegistry>> = OnceLock::new();
    Arc::clone(REGISTRY.get_or_init(|| Arc::new(SharedAnalyzerRegistry::new())))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendSnapshotState {
    Ready,
    Busy,
    Poisoned,
}

#[derive(Debug)]
pub struct SharedAnalyzerBackendSnapshot {
    pub key: SharedAnalyzerBackendKey,
    pub state: BackendSnapshotState,
    pub client_sessions: usize,
    pub overlay_sessions: Option<usize>,
    pub overlay_files: Option<usize>,
    pub workspace_loads: Option<Vec<WorkspaceSummary>>,
}

pub struct SharedAnalyzerRegistry {
    state: Mutex<SharedAnalyzerRegistryState>,
    gc: Arc<SharedAnalyzerGcCoordinator>,
}

#[derive(Default)]
struct SharedAnalyzerRegistryState {
    worlds: BTreeMap<SharedAnalyzerWorldKey, SharedAnalyzerWorldEntry>,
    views: BTreeMap<SharedAnalyzerBackendKey, SharedAnalyzerViewEntry>,
    loads: BTreeMap<SharedAnalyzerWorkspaceLoadKey, Arc<SharedAnalyzerWorkspaceLoad>>,
    operations: BTreeMap<
        SharedAnalyzerWorldKey,
        VecDeque<(SharedAnalyzerBackendKey, Arc<SharedAnalyzerReload>)>,
    >,
    normal_operations: BTreeMap<SharedAnalyzerWorldKey, Vec<Arc<SharedAnalyzerReload>>>,
}

struct SharedAnalyzerWorldEntry {
    client_sessions: usize,
    world: Arc<Mutex<SharedWorld>>,
}

#[derive(Default)]
struct SharedBaseFileIds(Mutex<BTreeMap<String, FileId>>);

impl SharedBaseFileIds {
    fn resolve(&self, path: &VfsPath) -> FileId {
        *self
            .0
            .lock()
            .expect("shared base file ID map is poisoned")
            .entry(path_key(path))
            .or_insert_with(allocate_shared_file_id)
    }
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

enum SharedAnalyzerWorkspaceLoadEvent {
    Progress(String),
    Finished,
}

struct SharedAnalyzerWorkspaceLoad {
    result: Mutex<Option<Result<usize, String>>>,
    listeners: Mutex<Vec<crossbeam_channel::Sender<SharedAnalyzerWorkspaceLoadEvent>>>,
    ready: Condvar,
}

struct SharedAnalyzerWorkspaceLoadGuard<'a> {
    registry: &'a SharedAnalyzerRegistry,
    key: &'a SharedAnalyzerWorkspaceLoadKey,
    load: &'a SharedAnalyzerWorkspaceLoad,
    completed: bool,
}

pub(crate) struct SharedAnalyzerReload {
    pending_loads: Vec<Arc<SharedAnalyzerWorkspaceLoad>>,
    pending_normal_operations: Vec<Arc<SharedAnalyzerReload>>,
    phase: Mutex<()>,
    keys: Mutex<Vec<ProcMacroSpawnKey>>,
    generation: AtomicU64,
    turn_ready: AtomicBool,
    result: Mutex<Option<Result<Vec<usize>, String>>>,
    ready: Condvar,
}

#[derive(Clone)]
pub(crate) struct SharedAnalyzerOperationToken {
    session_id: u64,
    operation: Option<Arc<SharedAnalyzerReload>>,
    generation: u64,
}

impl fmt::Debug for SharedAnalyzerOperationToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SharedAnalyzerOperationToken")
            .field("session_id", &self.session_id)
            .field("active", &self.operation.is_some())
            .field("generation", &self.generation)
            .finish()
    }
}

pub(crate) struct SharedAnalyzerOperationWaiter {
    registry: Weak<SharedAnalyzerRegistry>,
    world: SharedAnalyzerWorldKey,
    operation: Arc<SharedAnalyzerReload>,
    kind: SharedAnalyzerOperationKind,
}

#[derive(Clone, Copy)]
enum SharedAnalyzerOperationKind {
    Explicit,
    Normal,
}

struct SharedAnalyzerRegistryLease {
    registry: Weak<SharedAnalyzerRegistry>,
    key: SharedAnalyzerBackendKey,
}

struct SharedAnalyzerGcCoordinator {
    state: Mutex<SharedAnalyzerGcState>,
    access: Arc<SharedWorldAccess>,
}

#[derive(Default)]
struct SharedAnalyzerGcState {
    busy_sessions: usize,
    dirty: bool,
    collecting: bool,
}

#[derive(Default)]
struct SharedWorldAccess {
    state: Mutex<SharedWorldAccessState>,
    ready: Condvar,
    overlay_update: Mutex<()>,
}

#[derive(Default)]
struct SharedWorldAccessState {
    readers: BTreeMap<u64, usize>,
    pending_writers: BTreeMap<Option<u64>, usize>,
    foreign_epochs: BTreeMap<u64, u64>,
}

pub(crate) struct SharedAnalyzerReadPermit {
    access: Arc<SharedWorldAccess>,
    session_id: u64,
    foreign_epoch: u64,
}

struct SharedAnalyzerWritePermit {
    access: Arc<SharedWorldAccess>,
    owner: Option<u64>,
}

impl SharedWorldAccess {
    fn read(self: &Arc<Self>, session_id: u64) -> SharedAnalyzerReadPermit {
        let mut state = self
            .state
            .lock()
            .expect("shared world access mutex poisoned");
        while state.read_is_blocked(session_id) {
            state = self
                .ready
                .wait(state)
                .expect("shared world access mutex poisoned");
        }
        *state.readers.entry(session_id).or_default() += 1;
        let foreign_epoch = *state.foreign_epochs.entry(session_id).or_default();
        SharedAnalyzerReadPermit {
            access: Arc::clone(self),
            session_id,
            foreign_epoch,
        }
    }

    fn write(self: &Arc<Self>, owner: Option<u64>) -> SharedAnalyzerWritePermit {
        let mut state = self
            .state
            .lock()
            .expect("shared world access mutex poisoned");
        *state.pending_writers.entry(owner).or_default() += 1;
        while state.write_is_blocked(owner) {
            state = self
                .ready
                .wait(state)
                .expect("shared world access mutex poisoned");
        }
        SharedAnalyzerWritePermit {
            access: Arc::clone(self),
            owner,
        }
    }

    fn write_overlay(
        self: &Arc<Self>,
        session_id: u64,
        cancel: impl FnOnce(),
    ) -> SharedAnalyzerWritePermit {
        let owner = Some(session_id);
        let mut state = self
            .state
            .lock()
            .expect("shared world access mutex poisoned");
        *state.pending_writers.entry(owner).or_default() += 1;
        let permit = SharedAnalyzerWritePermit {
            access: Arc::clone(self),
            owner,
        };
        let has_foreign_readers = state
            .readers
            .iter()
            .any(|(&reader, &count)| reader != session_id && count > 0);
        for (&session, epoch) in &mut state.foreign_epochs {
            if session != session_id {
                *epoch += 1;
            }
        }
        if has_foreign_readers {
            drop(state);
            cancel();
            state = self
                .state
                .lock()
                .expect("shared world access mutex poisoned");
        }
        while state.write_is_blocked(owner) {
            state = self
                .ready
                .wait(state)
                .expect("shared world access mutex poisoned");
        }
        permit
    }

    fn unregister_session(&self, session_id: u64) {
        self.state
            .lock()
            .expect("shared world access mutex poisoned")
            .foreign_epochs
            .remove(&session_id);
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

    fn foreign_epoch(&self) -> u64 {
        self.foreign_epoch
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
        Self {
            state: Mutex::new(SharedAnalyzerRegistryState::default()),
            gc: SharedAnalyzerGcCoordinator::new(),
        }
    }

    fn state(&self) -> anyhow::Result<std::sync::MutexGuard<'_, SharedAnalyzerRegistryState>> {
        self.state.lock().map_err(|error| {
            anyhow::format_err!("shared analyzer registry mutex is poisoned: {error}")
        })
    }

    pub(crate) fn register(
        self: &Arc<Self>,
        key: SharedAnalyzerBackendKey,
        config: Arc<SharedAnalyzerConfig>,
        reload_path: Option<AbsPathBuf>,
        reload: bool,
        load: bool,
        continuation: Option<SharedAnalyzerOperationToken>,
        progress: &(dyn Fn(String) + Sync),
    ) -> anyhow::Result<SharedAnalyzerSession> {
        let world = self.world(&key.shared_world)?;
        self.retain_world(&key.shared_world)?;

        let reload = reload || reload_path.is_some() || continuation.is_some();
        let result = (|| {
            let (workspaces, reload_operation) = if !load {
                (Vec::new(), None)
            } else if reload {
                let (workspaces, operation) = self.reload_workspaces(
                    key.clone(),
                    Arc::clone(&world),
                    &config,
                    continuation.as_ref(),
                    progress,
                )?;
                (
                    workspaces
                        .into_iter()
                        .map(Ok)
                        .collect(),
                    operation,
                )
            } else {
                (
                    self.load_workspaces(
                        key.shared_world.clone(),
                        Arc::clone(&world),
                        &config,
                        progress,
                    ),
                    None,
                )
            };
            let view = WorkspaceView::new(
                workspaces
                    .iter()
                    .filter_map(|workspace| workspace.as_ref().ok().copied())
                    .collect(),
                config.excluded_paths().to_vec(),
            );
            {
                let mut state = self.state()?;
                match state.views.entry(key.clone()) {
                    Entry::Occupied(mut entry) => {
                        entry.get_mut().client_sessions += 1;
                        entry.get_mut().view = view.clone();
                    }
                    Entry::Vacant(entry) => {
                        entry.insert(SharedAnalyzerViewEntry {
                            client_sessions: 1,
                            view: view.clone(),
                        });
                    }
                }
            }

            Ok(SharedAnalyzerSession::new(
                world,
                view,
                Arc::downgrade(self),
                Arc::clone(&self.gc),
                key.clone(),
                Arc::clone(&config),
                reload_operation,
                workspaces,
            ))
        })();

        if result.is_err() {
            self.release_world(&key.shared_world);
        }

        result
    }

    fn load_workspaces(
        &self,
        world_key: SharedAnalyzerWorldKey,
        world: Arc<Mutex<SharedWorld>>,
        config: &SharedAnalyzerConfig,
        progress: &(dyn Fn(String) + Sync),
    ) -> Vec<SharedWorkspaceResult> {
        let mut workspaces = Vec::new();

        for (load_key, source) in config.workspace_sources() {
            workspaces.push(
                self.ensure_workspace_loaded(
                    world_key.clone(),
                    Arc::clone(&world),
                    load_key,
                    source,
                    config,
                    progress,
                )
                .map_err(|error| format!("{error:#}")),
            );
        }

        workspaces
    }

    fn prepare_reload_workspaces(
        &self,
        world: &Arc<Mutex<SharedWorld>>,
        config: &SharedAnalyzerConfig,
        progress: &(dyn Fn(String) + Sync),
    ) -> anyhow::Result<(Vec<PreparedWorkspaceLoad>, Vec<ProcMacroSpawnKey>)> {
        let sources = config.workspace_sources().collect::<Vec<_>>();
        let load_keys = sources
            .iter()
            .map(|(load_key, _)| load_key.clone())
            .collect::<Vec<_>>();
        let (mut keys, clients, base_file_ids) = {
            let world = world
                .lock()
                .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?;
            (
                world.proc_macro_reload_keys(&load_keys, config),
                world.proc_macro_clients(),
                Arc::clone(&world.base_file_ids),
            )
        };
        let mut rejected = clients
            .iter()
            .filter(|(key, _)| keys.iter().any(|reload| reload == key))
            .map(|(_, client)| Arc::clone(client))
            .collect::<Vec<_>>();
        let mut loaded = Vec::new();
        for (_, source) in sources {
            let (load_key, workspace) = SharedWorld::load_workspace(source, config, progress)?;
            if let Some(Ok(key)) = proc_macro_spawn_key(
                &workspace,
                &config.cargo_config.extra_env,
                &config.load.to_load_cargo_config(),
            ) && !keys.iter().any(|old| old == &key)
            {
                rejected.extend(
                    clients
                        .iter()
                        .filter(|(client_key, _)| client_key == &key)
                        .map(|(_, client)| Arc::clone(client)),
                );
                keys.push(key);
            }
            loaded.push(SharedWorld::prepare_loaded_workspace(
                load_key,
                workspace,
                config,
                &rejected,
                metadata_proc_macro_state(config),
                &base_file_ids,
            )?);
        }
        Ok((loaded, keys))
    }

    fn reload_continuation(
        &self,
        key: &SharedAnalyzerBackendKey,
        token: Option<&SharedAnalyzerOperationToken>,
    ) -> anyhow::Result<Option<Arc<SharedAnalyzerReload>>> {
        let Some(token) = token else {
            return Ok(None);
        };
        let Some(operation) = token
            .operation
            .as_ref()
            .filter(|operation| !operation.finished())
        else {
            return Ok(None);
        };
        self.continue_operation(key, operation, token.generation)
            .map(|continued| continued.then(|| Arc::clone(operation)))
    }

    fn reload_workspaces(
        &self,
        key: SharedAnalyzerBackendKey,
        world: Arc<Mutex<SharedWorld>>,
        config: &SharedAnalyzerConfig,
        continuation: Option<&SharedAnalyzerOperationToken>,
        progress: &(dyn Fn(String) + Sync),
    ) -> anyhow::Result<(Vec<usize>, Option<Arc<SharedAnalyzerReload>>)> {
        let continuation = self.reload_continuation(&key, continuation)?;
        let continued = continuation.is_some();
        let reload = match continuation {
            Some(reload) => reload,
            None => self.enqueue_operation(key.clone())?,
        };
        if let Err(error) = self.wait_operation_ready(&key.shared_world, &reload) {
            self.cancel_operation(&key, &reload, &error);
            return Err(error);
        }
        reload.turn_ready.store(true, Ordering::SeqCst);

        let result = (|| {
            let (access, base_file_access) = {
                let world = world.lock().map_err(|error| {
                    anyhow::format_err!("shared world mutex is poisoned: {error}")
                })?;
                (world.access(), world.base_file_access())
            };
            let _base_files = base_file_access.lock().map_err(|error| {
                anyhow::format_err!("shared base-file mutex is poisoned: {error}")
            })?;
            let (loaded, keys) = self.prepare_reload_workspaces(&world, config, progress)?;
            let _phase = self.begin_operation_phase(&key, &reload)?;
            reload.set_keys(keys.clone());
            self.commit_workspace_batch_load(&world, &access, loaded, &keys, config)
        })();
        if let Err(error) = &result {
            if !continued {
                self.finish_operation(&key, &reload, &Err(anyhow::format_err!("{error:#}")));
            }
            return Err(anyhow::format_err!("{error:#}"));
        }
        Ok((result?, (!continued).then_some(reload)))
    }

    fn enqueue_operation(
        &self,
        key: SharedAnalyzerBackendKey,
    ) -> anyhow::Result<Arc<SharedAnalyzerReload>> {
        let mut state = self.state()?;
        let world_key = key.shared_world.clone();
        let pending_loads = state
            .loads
            .iter()
            .filter(|(load_key, _)| load_key.world == world_key)
            .map(|(_, load)| Arc::clone(load))
            .collect();
        let pending_normal_operations = state
            .normal_operations
            .get(&world_key)
            .cloned()
            .unwrap_or_default();
        let operation = Arc::new(SharedAnalyzerReload::new(
            pending_loads,
            pending_normal_operations,
        ));
        state
            .operations
            .entry(world_key)
            .or_default()
            .push_back((key, Arc::clone(&operation)));
        Ok(operation)
    }

    fn wait_operation_ready(
        &self,
        world_key: &SharedAnalyzerWorldKey,
        operation: &Arc<SharedAnalyzerReload>,
    ) -> anyhow::Result<()> {
        for load in &operation.pending_loads {
            let _ = load.wait();
        }
        for normal_operation in &operation.pending_normal_operations {
            let _ = normal_operation.wait();
        }
        loop {
            let predecessor = {
                let state = self.state()?;
                let queue = state.operations.get(world_key).ok_or_else(|| {
                    anyhow::format_err!("shared operation is no longer registered")
                })?;
                if queue
                    .front()
                    .is_some_and(|(_, active)| Arc::ptr_eq(active, operation))
                {
                    return Ok(());
                }
                if !queue
                    .iter()
                    .any(|(_, queued)| Arc::ptr_eq(queued, operation))
                {
                    anyhow::bail!("shared operation is no longer registered");
                }
                queue.front().map(|(_, active)| Arc::clone(active))
            };
            let predecessor = predecessor
                .ok_or_else(|| anyhow::format_err!("shared operation queue is empty"))?;
            let _ = predecessor.wait();
        }
    }

    fn continue_operation(
        &self,
        key: &SharedAnalyzerBackendKey,
        operation: &Arc<SharedAnalyzerReload>,
        generation: u64,
    ) -> anyhow::Result<bool> {
        let _phase = operation.phase.lock().map_err(|error| {
            anyhow::format_err!("shared operation phase mutex is poisoned: {error}")
        })?;
        let state = self.state()?;
        let active = state.operations.get(&key.shared_world).is_some_and(|queue| {
            queue.front().is_some_and(|(active_key, active)| {
                active_key == key && Arc::ptr_eq(active, operation)
            })
        });
        Ok(active && operation.continue_from(generation))
    }

    fn begin_operation_phase<'a>(
        &self,
        key: &SharedAnalyzerBackendKey,
        operation: &'a Arc<SharedAnalyzerReload>,
    ) -> anyhow::Result<std::sync::MutexGuard<'a, ()>> {
        let phase = operation.phase.lock().map_err(|error| {
            anyhow::format_err!("shared operation phase mutex is poisoned: {error}")
        })?;
        let state = self.state()?;
        if !state.operations.get(&key.shared_world).is_some_and(|queue| {
            queue.front().is_some_and(|(active_key, active)| {
                active_key == key && Arc::ptr_eq(active, operation)
            })
        }) {
            anyhow::bail!("shared operation is no longer registered");
        }
        drop(state);
        Ok(phase)
    }

    fn begin_normal_operation(
        &self,
        key: SharedAnalyzerBackendKey,
    ) -> anyhow::Result<Arc<SharedAnalyzerReload>> {
        let mut state = self.state()?;
        let operation = Arc::new(SharedAnalyzerReload::new(Vec::new(), Vec::new()));
        state
            .normal_operations
            .entry(key.shared_world)
            .or_default()
            .push(Arc::clone(&operation));
        Ok(operation)
    }

    fn normal_operation_ready(
        &self,
        key: &SharedAnalyzerBackendKey,
        operation: &Arc<SharedAnalyzerReload>,
    ) -> anyhow::Result<bool> {
        let state = self.state()?;
        let Some(active) = state
            .operations
            .get(&key.shared_world)
            .and_then(|queue| queue.front())
            .map(|(_, operation)| operation)
        else {
            return Ok(true);
        };
        Ok(active
            .pending_normal_operations
            .iter()
            .any(|pending| Arc::ptr_eq(pending, operation)))
    }

    fn wait_normal_operation_ready(
        &self,
        key: &SharedAnalyzerWorldKey,
        operation: &Arc<SharedAnalyzerReload>,
    ) -> anyhow::Result<()> {
        loop {
            let active = {
                let state = self.state()?;
                if !state.normal_operations.get(key).is_some_and(|operations| {
                    operations.iter().any(|item| Arc::ptr_eq(item, operation))
                }) {
                    anyhow::bail!("shared normal operation is no longer registered");
                }
                state
                    .operations
                    .get(key)
                    .and_then(|queue| queue.front())
                    .map(|(_, active)| Arc::clone(active))
            };
            let Some(active) = active else {
                return Ok(());
            };
            if active
                .pending_normal_operations
                .iter()
                .any(|pending| Arc::ptr_eq(pending, operation))
            {
                return Ok(());
            }
            let _ = active.wait();
        }
    }

    fn finish_normal_operation(
        &self,
        key: &SharedAnalyzerBackendKey,
        operation: &Arc<SharedAnalyzerReload>,
        result: &anyhow::Result<Vec<usize>>,
    ) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        let Some(operations) = state.normal_operations.get_mut(&key.shared_world) else {
            return false;
        };
        let Some(index) = operations
            .iter()
            .position(|active| Arc::ptr_eq(active, operation))
        else {
            return false;
        };
        operation.finish(result);
        operations.remove(index);
        if operations.is_empty() {
            state.normal_operations.remove(&key.shared_world);
        }
        true
    }

    fn active_operation(
        &self,
        key: &SharedAnalyzerBackendKey,
    ) -> anyhow::Result<Option<Arc<SharedAnalyzerReload>>> {
        let state = self.state()?;
        Ok(state
            .operations
            .get(&key.shared_world)
            .and_then(|queue| queue.front())
            .map(|(_, operation)| Arc::clone(operation)))
    }

    fn finish_operation(
        &self,
        key: &SharedAnalyzerBackendKey,
        operation: &Arc<SharedAnalyzerReload>,
        result: &anyhow::Result<Vec<usize>>,
    ) -> bool {
        let Ok(_phase) = operation.phase.lock() else {
            return false;
        };
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        let Some(queue) = state.operations.get_mut(&key.shared_world) else {
            return false;
        };
        if !queue
            .front()
            .is_some_and(|(active_key, active)| active_key == key && Arc::ptr_eq(active, operation))
        {
            return false;
        }
        operation.finish(result);
        queue.pop_front();
        if queue.is_empty() {
            state.operations.remove(&key.shared_world);
        }
        true
    }

    fn cancel_operation(
        &self,
        key: &SharedAnalyzerBackendKey,
        operation: &Arc<SharedAnalyzerReload>,
        error: &anyhow::Error,
    ) {
        let Ok(_phase) = operation.phase.lock() else {
            return;
        };
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let Some(queue) = state.operations.get_mut(&key.shared_world) else {
            return;
        };
        let Some(index) = queue
            .iter()
            .position(|(active_key, active)| active_key == key && Arc::ptr_eq(active, operation))
        else {
            return;
        };
        operation.finish(&Err(anyhow::format_err!("{error:#}")));
        queue.remove(index);
        if queue.is_empty() {
            state.operations.remove(&key.shared_world);
        }
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

    fn world(&self, key: &SharedAnalyzerWorldKey) -> anyhow::Result<Arc<Mutex<SharedWorld>>> {
        let mut state = self.state()?;
        let entry = match state.worlds.entry(key.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let world = SharedWorld::new(&key.database);
                entry.insert(SharedAnalyzerWorldEntry {
                    client_sessions: 0,
                    world: Arc::new(Mutex::new(world)),
                })
            }
        };

        Ok(Arc::clone(&entry.world))
    }

    fn commit_workspace_load(
        &self,
        world: &Arc<Mutex<SharedWorld>>,
        access: &Arc<SharedWorldAccess>,
        loaded: PreparedWorkspaceLoad,
    ) -> anyhow::Result<usize> {
        let _write = access.write(Some(0));
        let mut world = world
            .lock()
            .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?;
        let generation = world.input_generation.load(Ordering::SeqCst);
        let result = world.commit_workspace(loaded);
        if world.input_generation.load(Ordering::SeqCst) != generation {
            self.gc.changed();
        }
        Ok(result)
    }

    fn commit_workspace_batch_load(
        &self,
        world: &Arc<Mutex<SharedWorld>>,
        access: &Arc<SharedWorldAccess>,
        loaded: Vec<PreparedWorkspaceLoad>,
        reload_keys: &[ProcMacroSpawnKey],
        config: &SharedAnalyzerConfig,
    ) -> anyhow::Result<Vec<usize>> {
        let _write = access.write(Some(0));
        let mut world = world
            .lock()
            .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?;
        let generation = world.input_generation.load(Ordering::SeqCst);
        let result = world.commit_workspace_batch(loaded, reload_keys, config);
        if world.input_generation.load(Ordering::SeqCst) != generation {
            self.gc.changed();
        }
        result
    }

    fn ensure_workspace_loaded(
        &self,
        world_key: SharedAnalyzerWorldKey,
        world: Arc<Mutex<SharedWorld>>,
        load_key: String,
        source: SharedAnalyzerWorkspaceLoadSource,
        config: &SharedAnalyzerConfig,
        progress: &(dyn Fn(String) + Sync),
    ) -> anyhow::Result<usize> {
        let registry_load_key = SharedAnalyzerWorkspaceLoadKey {
            world: world_key,
            project: load_key,
        };
        loop {
            let (load, leader, active_operation) = {
                let mut state = self.state()?;
                let active_operation = state
                    .operations
                    .get(&registry_load_key.world)
                    .and_then(|queue| queue.front())
                    .map(|(_, operation)| Arc::clone(operation));
                if let Some(active_operation) = active_operation {
                    (None, false, Some(active_operation))
                } else {
                    let (load, leader) = match state.loads.entry(registry_load_key.clone()) {
                        Entry::Occupied(entry) => (Arc::clone(entry.get()), false),
                        Entry::Vacant(entry) => {
                            let load = Arc::new(SharedAnalyzerWorkspaceLoad::new());
                            entry.insert(Arc::clone(&load));
                            (load, true)
                        }
                    };
                    (Some(load), leader, None)
                }
            };

            if let Some(active_operation) = active_operation {
                let _ = active_operation.wait();
                continue;
            }
            let load = load.expect("shared analyzer load was registered");
            let listener = if leader { None } else { load.subscribe()? };
            if leader {
                let guard = SharedAnalyzerWorkspaceLoadGuard {
                    registry: self,
                    key: &registry_load_key,
                    load: &load,
                    completed: false,
                };
                let existing = world
                    .lock()
                    .map_err(|error| {
                        anyhow::format_err!("shared world mutex is poisoned: {error}")
                    })?
                    .workspace_index(&registry_load_key.project);
                let result = if let Some(index) = existing {
                    Ok(index)
                } else {
                    let access = world
                        .lock()
                        .map_err(|error| {
                            anyhow::format_err!("shared world mutex is poisoned: {error}")
                        })?
                        .access();
                    world
                        .lock()
                        .map_err(|error| {
                            anyhow::format_err!("shared world mutex is poisoned: {error}")
                        })
                        .map(|world| Arc::clone(&world.base_file_ids))
                        .and_then(|base_file_ids| {
                            let report = |message: String| {
                                progress(message.clone());
                                load.report(message);
                            };
                            SharedWorld::prepare_workspace_load(
                                source.clone(),
                                config,
                                &base_file_ids,
                                &report,
                            )
                        })
                        .and_then(|loaded| self.commit_workspace_load(&world, &access, loaded))
                };
                guard.complete(result);
            }
            let result = match listener {
                Some(listener) => load.wait_with_progress(listener, progress),
                None => load.wait(),
            };
            if leader || result.is_ok() {
                return result;
            }
        }
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
                let (state, overlay_sessions, overlay_files, workspace_loads) =
                    match world.try_lock() {
                        Ok(world) => (
                            BackendSnapshotState::Ready,
                            Some(world.active_overlay_sessions()),
                            Some(world.overlay_files()),
                            Some(world.workspace_summaries(&view)),
                        ),
                        Err(std::sync::TryLockError::WouldBlock) => {
                            (BackendSnapshotState::Busy, None, None, None)
                        }
                        Err(std::sync::TryLockError::Poisoned(_)) => {
                            (BackendSnapshotState::Poisoned, None, None, None)
                        }
                    };

                SharedAnalyzerBackendSnapshot {
                    key,
                    state,
                    client_sessions,
                    overlay_sessions,
                    overlay_files,
                    workspace_loads,
                }
            })
            .collect()
    }

    pub(crate) fn request_gc(&self) {
        self.gc.request();
    }

    fn collect_garbage(&self) {
        let state = self
            .state
            .lock()
            .expect("shared analyzer registry mutex poisoned");
        let worlds = state
            .worlds
            .values()
            .map(|entry| Arc::clone(&entry.world))
            .collect::<Vec<_>>();
        let mut worlds = worlds
            .iter()
            .map(|world| world.lock().expect("shared world mutex poisoned"))
            .collect::<Vec<_>>();

        let Some((last, worlds)) = worlds.split_last_mut() else {
            return;
        };
        for world in worlds {
            world.host.trigger_cancellation();
        }
        last.host.trigger_garbage_collection();
    }
}

fn release_world_state(state: &mut SharedAnalyzerRegistryState, key: &SharedAnalyzerWorldKey) {
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
            listeners: Mutex::new(Vec::new()),
            ready: Condvar::new(),
        }
    }

    fn subscribe(
        &self,
    ) -> anyhow::Result<Option<crossbeam_channel::Receiver<SharedAnalyzerWorkspaceLoadEvent>>> {
        let slot = self.result.lock().map_err(|error| {
            anyhow::format_err!("shared analyzer load mutex is poisoned: {error}")
        })?;
        if slot.is_some() {
            return Ok(None);
        }
        let (sender, receiver) = crossbeam_channel::unbounded();
        self.listeners
            .lock()
            .map_err(|error| {
                anyhow::format_err!("shared analyzer load listener mutex is poisoned: {error}")
            })?
            .push(sender);
        Ok(Some(receiver))
    }

    fn report(&self, message: String) {
        if let Ok(mut listeners) = self.listeners.lock() {
            listeners.retain(|listener| {
                listener
                    .send(SharedAnalyzerWorkspaceLoadEvent::Progress(message.clone()))
                    .is_ok()
            });
        }
    }

    fn finish(&self, result: anyhow::Result<usize>) {
        if let Ok(mut slot) = self.result.lock() {
            *slot = Some(result.map_err(|error| format!("{error:#}")));
            if let Ok(mut listeners) = self.listeners.lock() {
                for listener in listeners.drain(..) {
                    _ = listener.send(SharedAnalyzerWorkspaceLoadEvent::Finished);
                }
            }
            self.ready.notify_all();
        }
    }

    fn wait(&self) -> anyhow::Result<usize> {
        let mut slot = self.result.lock().map_err(|error| {
            anyhow::format_err!("shared analyzer load mutex is poisoned: {error}")
        })?;

        loop {
            if let Some(result) = &*slot {
                return result
                    .as_ref()
                    .copied()
                    .map_err(|error| anyhow::format_err!("{error}"));
            }
            slot = self.ready.wait(slot).map_err(|error| {
                anyhow::format_err!("shared analyzer load mutex is poisoned: {error}")
            })?;
        }
    }

    fn wait_with_progress(
        &self,
        receiver: crossbeam_channel::Receiver<SharedAnalyzerWorkspaceLoadEvent>,
        progress: &(dyn Fn(String) + Sync),
    ) -> anyhow::Result<usize> {
        while let Ok(event) = receiver.recv() {
            match event {
                SharedAnalyzerWorkspaceLoadEvent::Progress(message) => progress(message),
                SharedAnalyzerWorkspaceLoadEvent::Finished => break,
            }
        }
        self.wait()
    }
}

impl SharedAnalyzerOperationWaiter {
    pub(crate) fn wait(self) -> anyhow::Result<()> {
        let Some(registry) = self.registry.upgrade() else {
            return Ok(());
        };
        let result = match self.kind {
            SharedAnalyzerOperationKind::Explicit => {
                registry.wait_operation_ready(&self.world, &self.operation)
            }
            SharedAnalyzerOperationKind::Normal => {
                registry.wait_normal_operation_ready(&self.world, &self.operation)
            }
        };
        if result.is_ok() {
            self.operation.turn_ready.store(true, Ordering::SeqCst);
        }
        result
    }
}

impl SharedAnalyzerReload {
    fn new(
        pending_loads: Vec<Arc<SharedAnalyzerWorkspaceLoad>>,
        pending_normal_operations: Vec<Arc<SharedAnalyzerReload>>,
    ) -> Self {
        Self {
            pending_loads,
            pending_normal_operations,
            phase: Mutex::new(()),
            keys: Mutex::new(Vec::new()),
            generation: AtomicU64::new(0),
            turn_ready: AtomicBool::new(false),
            result: Mutex::new(None),
            ready: Condvar::new(),
        }
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    fn turn_ready(&self) -> bool {
        self.turn_ready.load(Ordering::SeqCst)
    }

    fn continue_from(&self, generation: u64) -> bool {
        self.generation
            .compare_exchange(
                generation,
                generation + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    fn set_keys(&self, keys: Vec<ProcMacroSpawnKey>) {
        *self
            .keys
            .lock()
            .expect("shared analyzer reload mutex poisoned") = keys;
    }

    fn active_keys(&self) -> Option<Vec<ProcMacroSpawnKey>> {
        if !self.turn_ready() {
            return None;
        }
        let result = self
            .result
            .lock()
            .expect("shared analyzer reload mutex poisoned");
        if result.is_some() {
            return None;
        }
        Some(
            self.keys
                .lock()
                .expect("shared analyzer reload mutex poisoned")
                .clone(),
        )
    }

    fn finished(&self) -> bool {
        self.result
            .lock()
            .expect("shared analyzer reload mutex poisoned")
            .is_some()
    }

    fn finish(&self, result: &anyhow::Result<Vec<usize>>) {
        if let Ok(mut slot) = self.result.lock() {
            *slot = Some(
                result
                    .as_ref()
                    .map(Vec::clone)
                    .map_err(|error| format!("{error:#}")),
            );
            self.ready.notify_all();
        }
    }

    fn wait(&self) -> anyhow::Result<Vec<usize>> {
        let mut slot = self.result.lock().map_err(|error| {
            anyhow::format_err!("shared analyzer reload mutex is poisoned: {error}")
        })?;

        loop {
            if let Some(result) = &*slot {
                return result
                    .as_ref()
                    .cloned()
                    .map_err(|error| anyhow::format_err!("{error}"));
            }
            slot = self.ready.wait(slot).map_err(|error| {
                anyhow::format_err!("shared analyzer reload mutex is poisoned: {error}")
            })?;
        }
    }
}

impl SharedAnalyzerWorkspaceLoadGuard<'_> {
    fn complete(mut self, result: anyhow::Result<usize>) {
        self.remove();
        self.load.finish(result);
        self.completed = true;
    }

    fn remove(&self) {
        if let Ok(mut state) = self.registry.state.lock() {
            state.loads.remove(self.key);
        }
    }
}

impl Drop for SharedAnalyzerWorkspaceLoadGuard<'_> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        self.remove();
        self.load
            .finish(Err(anyhow::format_err!("workspace load was abandoned")));
    }
}

impl SharedAnalyzerGcCoordinator {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(SharedAnalyzerGcState::default()),
            access: Arc::new(SharedWorldAccess::default()),
        })
    }

    fn read(self: &Arc<Self>) -> SharedAnalyzerReadPermit {
        self.access.read(0)
    }

    fn register_session(&self) {
        self.state
            .lock()
            .expect("shared analyzer gc mutex poisoned")
            .busy_sessions += 1;
    }

    fn set_session_busy(&self, busy: bool) {
        let collect = {
            let mut state = self
                .state
                .lock()
                .expect("shared analyzer gc mutex poisoned");
            if busy {
                state.busy_sessions += 1;
            } else {
                state.busy_sessions -= 1;
            }
            self.start_if_ready(&mut state)
        };
        if collect {
            self.collect();
        }
    }

    fn unregister_session(&self, busy: bool) {
        let collect = {
            let mut state = self
                .state
                .lock()
                .expect("shared analyzer gc mutex poisoned");
            if busy {
                state.busy_sessions -= 1;
            }
            self.start_if_ready(&mut state)
        };
        if collect {
            self.collect();
        }
    }

    fn changed(&self) {
        self.state
            .lock()
            .expect("shared analyzer gc mutex poisoned")
            .dirty = true;
    }

    fn request(&self) {
        let collect = {
            let mut state = self
                .state
                .lock()
                .expect("shared analyzer gc mutex poisoned");
            state.dirty = true;
            self.start_if_ready(&mut state)
        };
        if collect {
            self.collect();
        }
    }

    fn start_if_ready(&self, state: &mut SharedAnalyzerGcState) -> bool {
        if state.dirty && !state.collecting && state.busy_sessions == 0 {
            state.dirty = false;
            state.collecting = true;
            true
        } else {
            false
        }
    }

    fn collect(&self) {
        loop {
            {
                let _write = self.access.write(None);
                shared_analyzer_registry().collect_garbage();
            }

            let mut state = self
                .state
                .lock()
                .expect("shared analyzer gc mutex poisoned");
            state.collecting = false;
            if !self.start_if_ready(&mut state) {
                return;
            }
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SharedAnalyzerBackendKey {
    pub shared_world: SharedAnalyzerWorldKey,
    pub workspace_view: SharedAnalyzerViewKey,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SharedAnalyzerWorldKey {
    pub cargo: SharedAnalyzerCargoConfigKey,
    pub database: SharedAnalyzerDatabaseConfigKey,
    pub load: SharedAnalyzerLoadKey,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SharedAnalyzerDatabaseConfigKey {
    pub lru_parse_query_capacity: Option<u16>,
    pub lru_query_capacities: BTreeMap<Box<str>, u16>,
    pub expand_proc_attr_macros: bool,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
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

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SharedAnalyzerLoadKey {
    pub load_out_dirs_from_check: bool,
    pub proc_macro_server: SharedAnalyzerProcMacroServerKey,
    pub ignored_proc_macros: Vec<(Box<str>, Vec<Box<str>>)>,
    pub proc_macro_processes: u16,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SharedAnalyzerProcMacroServerKey {
    None,
    Sysroot,
    Explicit(String),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SharedAnalyzerViewKey {
    pub projects: Vec<String>,
    pub excluded_paths: Vec<String>,
}

#[derive(Clone)]
enum SharedAnalyzerWorkspaceLoadSource {
    Project(crate::config::LinkedProject),
    DetachedFile(ManifestPath),
}

pub(crate) fn shared_database_config_key(
    config: &crate::config::Config,
) -> SharedAnalyzerDatabaseConfigKey {
    SharedAnalyzerDatabaseConfigKey {
        lru_parse_query_capacity: config.lru_parse_query_capacity(),
        lru_query_capacities: config
            .lru_query_capacities_config()
            .into_iter()
            .flatten()
            .map(|(query, capacity)| (query.clone(), *capacity))
            .collect(),
        expand_proc_attr_macros: config.expand_proc_attr_macros(),
    }
}

pub(crate) fn shared_analyzer_context_from_config(
    config: &crate::config::Config,
) -> anyhow::Result<(SharedAnalyzerBackendKey, Arc<SharedAnalyzerConfig>)> {
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
    let backend_key = SharedAnalyzerBackendKey {
        shared_world: SharedAnalyzerWorldKey {
            cargo: cargo_config_key(&cargo_config),
            database: shared_database_config_key(config),
            load: load.key.clone(),
        },
        workspace_view: SharedAnalyzerViewKey {
            projects: project_keys,
            excluded_paths: excluded_paths.clone(),
        },
    };

    Ok((
        backend_key,
        Arc::new(SharedAnalyzerConfig {
            excluded_paths,
            projects,
            detached_files,
            cargo_config,
            load,
        }),
    ))
}

pub struct SharedAnalyzerConfig {
    excluded_paths: Vec<String>,
    projects: Vec<crate::config::LinkedProject>,
    detached_files: Vec<ManifestPath>,
    pub(crate) cargo_config: CargoConfig,
    pub(crate) load: SharedLoadConfig,
}

impl SharedAnalyzerConfig {
    pub fn excluded_paths(&self) -> &[String] {
        &self.excluded_paths
    }

    fn workspace_sources(
        &self,
    ) -> impl Iterator<Item = (String, SharedAnalyzerWorkspaceLoadSource)> + '_ {
        self.projects
            .iter()
            .map(|project| {
                (
                    shared_project_key(project),
                    SharedAnalyzerWorkspaceLoadSource::Project(project.clone()),
                )
            })
            .chain(self.detached_files.iter().map(|file| {
                (
                    shared_detached_file_key(file),
                    SharedAnalyzerWorkspaceLoadSource::DetachedFile(file.clone()),
                )
            }))
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
    prefill_caches: bool,
    num_worker_threads: usize,
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
            prefill_caches: self.prefill_caches,
            num_worker_threads: self.num_worker_threads,
            proc_macro_processes: self.key.proc_macro_processes as usize,
        }
    }
}

fn shared_load_config_from_config(
    config: &crate::config::Config,
) -> anyhow::Result<SharedLoadConfig> {
    let mut ignored_proc_macros = config
        .ignored_proc_macros(None)
        .iter()
        .map(|(name, macros)| (name.clone(), macros.to_vec()))
        .collect::<Vec<_>>();
    ignored_proc_macros.sort();

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
            ignored_proc_macros,
            proc_macro_processes: u16::try_from(config.proc_macro_num_processes())?,
        },
        prefill_caches: config.prefill_caches(),
        num_worker_threads: config.prime_caches_num_threads(),
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

#[derive(Clone, Debug)]
pub struct WorkspaceSummary {
    pub root: String,
    pub manifest: String,
    pub packages: usize,
    pub files: usize,
    pub proc_macro_server: bool,
}

type SharedWorkspaceResult = Result<usize, String>;

pub(crate) struct SharedAnalyzerSession {
    runtime: SharedAnalyzerRuntime,
    workspaces: Vec<SharedWorkspaceResult>,
}

impl SharedAnalyzerSession {
    fn new(
        world: Arc<Mutex<SharedWorld>>,
        view: WorkspaceView,
        registry: Weak<SharedAnalyzerRegistry>,
        gc: Arc<SharedAnalyzerGcCoordinator>,
        key: SharedAnalyzerBackendKey,
        config: Arc<SharedAnalyzerConfig>,
        reload: Option<Arc<SharedAnalyzerReload>>,
        workspaces: Vec<SharedWorkspaceResult>,
    ) -> Self {
        let runtime = SharedAnalyzerRuntime::new(
            Arc::clone(&world),
            &view,
            registry,
            gc,
            key,
            config,
            reload,
        );

        Self { runtime, workspaces }
    }

    pub(crate) fn workspaces(
        &self,
    ) -> anyhow::Result<(Vec<anyhow::Result<ProjectWorkspace>>, bool)> {
        let world = self
            .runtime
            .session
            .world
            .lock()
            .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?;
        let build_data_loaded = self.workspaces.iter().all(|workspace| {
            workspace
                .as_ref()
                .map_or(true, |index| {
                    world.loaded_workspaces[*index].input.build_data_loaded
                })
        });
        let workspaces = self
            .workspaces
            .iter()
            .map(|workspace| match workspace.as_ref() {
                Ok(index) => Ok(world.loaded_workspaces[*index].workspace.clone()),
                Err(error) => Err(anyhow::format_err!("{error}")),
            })
            .collect();

        Ok((workspaces, build_data_loaded))
    }

    pub(crate) fn runtime(&self) -> SharedAnalyzerRuntime {
        self.runtime.clone()
    }
}

#[derive(Clone)]
pub(crate) struct SharedAnalyzerRuntime {
    session: Arc<SharedAnalyzerRuntimeSession>,
}

pub(crate) struct SharedAnalyzerRuntimeWeak {
    session: Weak<SharedAnalyzerRuntimeSession>,
}

struct SharedAnalyzerRuntimeSession {
    world: Arc<Mutex<SharedWorld>>,
    config: Arc<SharedAnalyzerConfig>,
    access: Arc<SharedWorldAccess>,
    gc: Arc<SharedAnalyzerGcCoordinator>,
    id: u64,
    active: AtomicBool,
    busy: AtomicBool,
    input_generation: Arc<AtomicU64>,
    overlay_generation: AtomicU64,
    config_generation_seen: AtomicU64,
    workspace_updates: crossbeam_channel::Receiver<()>,
    workspace_update_pending: Arc<AtomicBool>,
    workspace_indexes: Vec<usize>,
    excluded_paths: Vec<String>,
    line_endings: Mutex<SharedLineEndings>,
    file_mappings: Mutex<SharedFileMappings>,
    analysis_cache: Mutex<SharedAnalysisCache>,
    registry_lease: SharedAnalyzerRegistryLease,
    reload: Mutex<Option<Arc<SharedAnalyzerReload>>>,
    next_reload: Mutex<Option<Arc<SharedAnalyzerReload>>>,
    rebuild: Mutex<Option<Arc<SharedAnalyzerReload>>>,
    normal: Mutex<Option<Arc<SharedAnalyzerReload>>>,
}

#[derive(Clone)]
pub(crate) struct SharedAnalyzerSnapshotToken {
    session: Arc<SharedAnalyzerRuntimeSession>,
    base_generation: u64,
    overlay_generation: u64,
    foreign_epoch: u64,
}

struct SharedAnalyzerAnalysisGuard {
    _world: SharedAnalyzerReadPermit,
    _gc: SharedAnalyzerReadPermit,
    snapshot: SharedAnalyzerSnapshotToken,
}

#[derive(Clone)]
pub(crate) struct SharedAnalyzerPendingAnalysis {
    runtime: SharedAnalyzerRuntime,
}

impl SharedAnalyzerPendingAnalysis {
    pub(crate) fn activate(&self) -> Analysis {
        self.runtime.analysis()
    }

    pub(crate) fn activate_snapshot(&self) -> (Analysis, SharedAnalyzerSnapshotToken) {
        let analysis = self.runtime.analysis();
        let snapshot = self.runtime.snapshot_token(&analysis);
        (analysis, snapshot)
    }
}

impl SharedAnalyzerSnapshotToken {
    pub(crate) fn can_replay(&self, next: &Self) -> bool {
        self.session.active.load(Ordering::SeqCst)
            && Arc::ptr_eq(&self.session, &next.session)
            && self.base_generation == next.base_generation
            && self.overlay_generation == next.overlay_generation
            && self.foreign_epoch < next.foreign_epoch
    }

    pub(crate) fn replayable(&self) -> bool {
        self.session.active.load(Ordering::SeqCst)
            && self.base_generation == self.session.input_generation.load(Ordering::SeqCst)
            && self.overlay_generation == self.session.overlay_generation.load(Ordering::SeqCst)
            && self
                .session
                .access
                .state
                .lock()
                .expect("shared world access mutex poisoned")
                .foreign_epochs
                .get(&self.session.id)
                .is_some_and(|epoch| *epoch > self.foreign_epoch)
    }

    pub(crate) fn active(&self) -> bool {
        self.session.active.load(Ordering::SeqCst)
    }
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

impl SharedAnalyzerRuntimeSession {
    fn cancel_operations(&self, registry: &SharedAnalyzerRegistry, error: &anyhow::Error) {
        for slot in [&self.reload, &self.next_reload, &self.rebuild] {
            if let Ok(mut slot) = slot.lock()
                && let Some(operation) = slot.take()
                && !operation.finished()
            {
                registry.cancel_operation(&self.registry_lease.key, &operation, error);
            }
        }
        if let Ok(mut normal) = self.normal.lock()
            && let Some(operation) = normal.take()
            && !operation.finished()
        {
            registry.finish_normal_operation(
                &self.registry_lease.key,
                &operation,
                &Err(anyhow::format_err!("{error:#}")),
            );
        }
    }
}

impl Drop for SharedAnalyzerRuntimeSession {
    fn drop(&mut self) {
        self.active.store(false, Ordering::SeqCst);
        let lease = &self.registry_lease;
        if let Some(registry) = lease.registry.upgrade() {
            self.cancel_operations(
                &registry,
                &anyhow::format_err!("shared analyzer session dropped"),
            );
            registry.unregister(&lease.key);
        }
        {
            let _write = self.access.write(Some(self.id));
            if let Ok(mut world) = self.world.lock()
                && world.unregister_session(self.id)
            {
                self.gc.changed();
            }
        }
        self.access.unregister_session(self.id);
        self.gc
            .unregister_session(self.busy.load(Ordering::SeqCst));
    }
}

impl std::fmt::Debug for SharedAnalyzerRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedAnalyzerRuntime")
            .field("session", &self.session_id())
            .finish_non_exhaustive()
    }
}

impl SharedAnalyzerRuntimeWeak {
    pub(crate) fn upgrade(&self) -> Option<SharedAnalyzerRuntime> {
        let session = self.session.upgrade()?;
        Some(SharedAnalyzerRuntime { session })
    }
}

impl SharedAnalyzerRuntime {
    pub(crate) fn priming_scope(
        &self,
        state: &crate::global_state::GlobalState,
    ) -> triomphe::Arc<[ide::Crate]> {
        self.session
            .world
            .lock()
            .expect("shared world mutex poisoned")
            .priming_scope(
                self.session_id(),
                &self.session.workspace_indexes,
                &self.session.excluded_paths,
                state,
            )
    }

    fn new(
        world: Arc<Mutex<SharedWorld>>,
        view: &WorkspaceView,
        registry: Weak<SharedAnalyzerRegistry>,
        gc: Arc<SharedAnalyzerGcCoordinator>,
        key: SharedAnalyzerBackendKey,
        config: Arc<SharedAnalyzerConfig>,
        reload: Option<Arc<SharedAnalyzerReload>>,
    ) -> Self {
        gc.register_session();
        let workspace_indexes = view.workspace_indexes().collect::<Vec<_>>();
        let excluded_paths = view.excluded_paths().to_vec();
        let (id, input_generation, access, workspace_updates, workspace_update_pending) = world
            .lock()
            .expect("shared world mutex poisoned")
            .register_session(&workspace_indexes);
        let session = Arc::new(SharedAnalyzerRuntimeSession {
            world: Arc::clone(&world),
            config,
            access,
            gc,
            id,
            active: AtomicBool::new(true),
            busy: AtomicBool::new(true),
            input_generation,
            overlay_generation: AtomicU64::new(0),
            config_generation_seen: AtomicU64::new(u64::MAX),
            workspace_updates,
            workspace_update_pending,
            workspace_indexes,
            excluded_paths,
            line_endings: Mutex::new(SharedLineEndings::default()),
            file_mappings: Mutex::new(SharedFileMappings::default()),
            analysis_cache: Mutex::new(SharedAnalysisCache::default()),
            registry_lease: SharedAnalyzerRegistryLease { registry, key },
            reload: Mutex::new(reload),
            next_reload: Mutex::new(None),
            rebuild: Mutex::new(None),
            normal: Mutex::new(None),
        });

        let runtime = Self { session };
        runtime.refresh_session_cache_from_world();
        runtime
    }

    pub(crate) fn session_id(&self) -> u64 {
        self.session.id
    }

    pub(crate) fn is_same_session(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.session, &other.session)
    }

    pub(crate) fn retire(&self) {
        self.session.active.store(false, Ordering::SeqCst);
        self.set_busy(false);
    }

    pub(crate) fn downgrade(&self) -> SharedAnalyzerRuntimeWeak {
        SharedAnalyzerRuntimeWeak {
            session: Arc::downgrade(&self.session),
        }
    }

    pub(crate) fn begin_reload(
        &self,
    ) -> anyhow::Result<Option<(SharedAnalyzerOperationToken, SharedAnalyzerOperationWaiter)>> {
        let lease = &self.session.registry_lease;
        let Some(registry) = lease.registry.upgrade() else {
            return Ok(None);
        };
        let operation = registry.enqueue_operation(lease.key.clone())?;
        let mut reload = self
            .session
            .reload
            .lock()
            .map_err(|error| anyhow::format_err!("shared reload mutex is poisoned: {error}"))?;
        let slot = if reload.is_none() {
            &mut *reload
        } else {
            drop(reload);
            let mut next =
                self.session.next_reload.lock().map_err(|error| {
                    anyhow::format_err!("shared reload mutex is poisoned: {error}")
                })?;
            if next.is_some() {
                registry.cancel_operation(
                    &lease.key,
                    &operation,
                    &anyhow::format_err!("shared reload request was coalesced"),
                );
                return Ok(None);
            }
            *next = Some(Arc::clone(&operation));
            let token = SharedAnalyzerOperationToken {
                session_id: self.session_id(),
                operation: Some(Arc::clone(&operation)),
                generation: operation.generation(),
            };
            let waiter = SharedAnalyzerOperationWaiter {
                registry: lease.registry.clone(),
                world: lease.key.shared_world.clone(),
                operation,
                kind: SharedAnalyzerOperationKind::Explicit,
            };
            return Ok(Some((token, waiter)));
        };
        *slot = Some(Arc::clone(&operation));
        let token = SharedAnalyzerOperationToken {
            session_id: self.session_id(),
            operation: Some(Arc::clone(&operation)),
            generation: operation.generation(),
        };
        let waiter = SharedAnalyzerOperationWaiter {
            registry: lease.registry.clone(),
            world: lease.key.shared_world.clone(),
            operation,
            kind: SharedAnalyzerOperationKind::Explicit,
        };
        Ok(Some((token, waiter)))
    }

    pub(crate) fn activate_reload(&self, token: &SharedAnalyzerOperationToken) -> bool {
        let Some(expected) = &token.operation else {
            return false;
        };
        let mut reload = self
            .session
            .reload
            .lock()
            .expect("shared reload mutex poisoned");
        if reload
            .as_ref()
            .is_some_and(|operation| Arc::ptr_eq(operation, expected) && operation.turn_ready())
        {
            return true;
        }
        if reload.is_some() {
            return false;
        }
        let mut next = self
            .session
            .next_reload
            .lock()
            .expect("shared reload mutex poisoned");
        if next
            .as_ref()
            .is_some_and(|operation| Arc::ptr_eq(operation, expected) && operation.turn_ready())
        {
            *reload = next.take();
            return true;
        }
        false
    }

    pub(crate) fn reload_registered(&self) -> bool {
        self.session
            .reload
            .lock()
            .expect("shared reload mutex poisoned")
            .is_some()
            || self
                .session
                .next_reload
                .lock()
                .expect("shared reload mutex poisoned")
                .is_some()
    }

    pub(crate) fn rebuild_registered(&self) -> bool {
        self.session
            .rebuild
            .lock()
            .expect("shared rebuild mutex poisoned")
            .is_some()
    }

    pub(crate) fn begin_rebuild(&self) -> anyhow::Result<bool> {
        let lease = &self.session.registry_lease;
        let Some(registry) = lease.registry.upgrade() else {
            return Ok(false);
        };
        let mut rebuild =
            self.session.rebuild.lock().map_err(|error| {
                anyhow::format_err!("shared rebuild mutex is poisoned: {error}")
            })?;
        if rebuild.is_some() {
            return Ok(false);
        }
        *rebuild = Some(registry.enqueue_operation(lease.key.clone())?);
        Ok(true)
    }

    pub(crate) fn rebuild_waiter(&self) -> Option<SharedAnalyzerOperationWaiter> {
        let lease = &self.session.registry_lease;
        let operation = self
            .session
            .rebuild
            .lock()
            .expect("shared rebuild mutex poisoned")
            .clone()?;
        Some(SharedAnalyzerOperationWaiter {
            registry: lease.registry.clone(),
            world: lease.key.shared_world.clone(),
            operation,
            kind: SharedAnalyzerOperationKind::Explicit,
        })
    }

    pub(crate) fn begin_normal_operation(&self) -> anyhow::Result<bool> {
        let lease = &self.session.registry_lease;
        let Some(registry) = lease.registry.upgrade() else {
            return Ok(true);
        };
        let mut normal = self.session.normal.lock().map_err(|error| {
            anyhow::format_err!("shared normal operation mutex is poisoned: {error}")
        })?;
        if normal.is_some() {
            return Ok(true);
        }
        let operation = registry.begin_normal_operation(lease.key.clone())?;
        let ready = registry.normal_operation_ready(&lease.key, &operation)?;
        *normal = Some(operation);
        Ok(ready)
    }

    pub(crate) fn normal_waiter(&self) -> Option<SharedAnalyzerOperationWaiter> {
        let lease = &self.session.registry_lease;
        let operation = self
            .session
            .normal
            .lock()
            .expect("shared normal operation mutex poisoned")
            .clone()?;
        Some(SharedAnalyzerOperationWaiter {
            registry: lease.registry.clone(),
            world: lease.key.shared_world.clone(),
            operation,
            kind: SharedAnalyzerOperationKind::Normal,
        })
    }

    pub(crate) fn finish_normal_operation(&self, result: anyhow::Result<()>) -> bool {
        let lease = &self.session.registry_lease;
        let Some(registry) = lease.registry.upgrade() else {
            return false;
        };
        let Ok(mut normal) = self.session.normal.lock() else {
            return false;
        };
        let Some(operation) = normal.take() else {
            return false;
        };
        let notify = result.is_ok();
        let result = result.map(|_| self.session.workspace_indexes.clone());
        let finished = registry.finish_normal_operation(&lease.key, &operation, &result);
        if !finished && !operation.finished() {
            *normal = Some(operation);
        } else if finished && notify {
            self.notify_workspace_updates();
        }
        finished
    }

    fn reload_keys(&self) -> Option<Vec<ProcMacroSpawnKey>> {
        self.session
            .reload
            .lock()
            .expect("shared reload mutex poisoned")
            .as_ref()
            .and_then(|reload| reload.active_keys())
    }

    fn rebuild_keys(&self) -> Option<Vec<ProcMacroSpawnKey>> {
        self.session
            .rebuild
            .lock()
            .expect("shared rebuild mutex poisoned")
            .as_ref()
            .and_then(|rebuild| rebuild.active_keys())
    }

    fn operation_keys(&self) -> Option<Vec<ProcMacroSpawnKey>> {
        self.reload_keys().or_else(|| self.rebuild_keys())
    }

    fn set_operation_keys(&self, rebuild: bool, keys: Vec<ProcMacroSpawnKey>) {
        let operation = if rebuild {
            &self.session.rebuild
        } else {
            &self.session.reload
        };
        if let Some(operation) = operation
            .lock()
            .expect("shared operation mutex poisoned")
            .as_ref()
        {
            operation.set_keys(keys);
        }
    }

    pub(crate) fn reload_operation_token(&self) -> Option<SharedAnalyzerOperationToken> {
        let operation = self
            .session
            .reload
            .lock()
            .expect("shared reload mutex poisoned")
            .clone()
            .filter(|operation| operation.turn_ready() && !operation.finished())?;
        let generation = operation.generation();
        Some(SharedAnalyzerOperationToken {
            session_id: self.session_id(),
            operation: Some(operation),
            generation,
        })
    }

    pub(crate) fn rebuild_operation_token(&self) -> Option<SharedAnalyzerOperationToken> {
        let operation = self
            .session
            .rebuild
            .lock()
            .expect("shared rebuild mutex poisoned")
            .clone()
            .filter(|operation| !operation.finished())?;
        let generation = operation.generation();
        Some(SharedAnalyzerOperationToken {
            session_id: self.session_id(),
            operation: Some(operation),
            generation,
        })
    }

    pub(crate) fn operation_scope_matches(&self, target: &Self) -> bool {
        self.session.registry_lease.key == target.session.registry_lease.key
    }

    pub(crate) fn operation_token(&self) -> SharedAnalyzerOperationToken {
        let operation = self
            .session
            .normal
            .lock()
            .expect("shared operation mutex poisoned")
            .clone()
            .or_else(|| {
                [&self.session.reload, &self.session.rebuild]
                    .into_iter()
                    .find_map(|slot| {
                        slot.lock()
                            .expect("shared operation mutex poisoned")
                            .clone()
                            .filter(|operation| operation.turn_ready())
                    })
            });
        let generation = operation
            .as_ref()
            .map_or(0, |operation| operation.generation());
        SharedAnalyzerOperationToken {
            session_id: self.session.id,
            operation,
            generation,
        }
    }

    pub(crate) fn operation_token_matches(&self, token: &SharedAnalyzerOperationToken) -> bool {
        let current = self.operation_token();
        match (&token.operation, &current.operation) {
            (Some(expected), Some(actual)) => {
                Arc::ptr_eq(expected, actual)
                    && token.generation == current.generation
                    && !expected.finished()
            }
            (None, None) => token.session_id == current.session_id,
            _ => false,
        }
    }

    pub(crate) fn normal_operation_matches(&self, token: &SharedAnalyzerOperationToken) -> bool {
        let normal = self
            .session
            .normal
            .lock()
            .expect("shared normal operation mutex poisoned")
            .clone();
        token
            .operation
            .as_ref()
            .zip(normal.as_ref())
            .is_some_and(|(expected, current)| {
                Arc::ptr_eq(expected, current)
                    && token.generation == current.generation()
                    && !expected.finished()
            })
    }

    pub(crate) fn cancel_operations(&self, reason: &str) {
        let Some(registry) = self.session.registry_lease.registry.upgrade() else {
            return;
        };
        self.session
            .cancel_operations(&registry, &anyhow::format_err!("{reason}"));
    }

    pub(crate) fn transfer_operations(&self, target: &Self) {
        if Arc::ptr_eq(&self.session, &target.session) {
            return;
        }
        Self::transfer_operation(&self.session.reload, &target.session.reload, "reload");
        Self::transfer_operation(
            &self.session.next_reload,
            &target.session.next_reload,
            "next reload",
        );
        Self::transfer_operation(&self.session.rebuild, &target.session.rebuild, "rebuild");
        Self::transfer_operation(&self.session.normal, &target.session.normal, "normal");
    }

    fn transfer_operation(
        source: &Mutex<Option<Arc<SharedAnalyzerReload>>>,
        target: &Mutex<Option<Arc<SharedAnalyzerReload>>>,
        name: &str,
    ) {
        let mut source = source
            .lock()
            .unwrap_or_else(|_| panic!("shared {name} operation mutex poisoned"));
        if source
            .as_ref()
            .is_some_and(|operation| operation.finished())
        {
            source.take();
        } else if source.is_some() {
            let mut target = target
                .lock()
                .unwrap_or_else(|_| panic!("shared {name} operation mutex poisoned"));
            assert!(
                target.is_none(),
                "shared {name} operation already transferred"
            );
            *target = source.take();
        }
    }

    fn operation_ready(&self) -> anyhow::Result<bool> {
        let lease = &self.session.registry_lease;
        let Some(registry) = lease.registry.upgrade() else {
            return Ok(true);
        };
        let reload = self
            .session
            .reload
            .lock()
            .expect("shared reload mutex poisoned")
            .clone();
        let rebuild = self
            .session
            .rebuild
            .lock()
            .expect("shared rebuild mutex poisoned")
            .clone();
        let normal = self
            .session
            .normal
            .lock()
            .expect("shared normal operation mutex poisoned")
            .clone();
        let Some(active) = registry.active_operation(&lease.key)? else {
            return Ok(reload.is_none() && rebuild.is_none());
        };
        Ok(reload.as_ref().is_some_and(|own| Arc::ptr_eq(own, &active))
            || rebuild
                .as_ref()
                .is_some_and(|own| Arc::ptr_eq(own, &active))
            || normal.as_ref().is_some_and(|own| {
                active
                    .pending_normal_operations
                    .iter()
                    .any(|pending| Arc::ptr_eq(pending, own))
            }))
    }

    fn commit_operation<T>(
        &self,
        expected: &SharedAnalyzerOperationToken,
        commit: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<Option<T>> {
        if self
            .session
            .normal
            .lock()
            .expect("shared normal operation mutex poisoned")
            .is_some()
        {
            return self
                .operation_token_matches(expected)
                .then(commit)
                .transpose();
        }
        let reload = self
            .session
            .reload
            .lock()
            .expect("shared reload mutex poisoned")
            .clone();
        let rebuild = self
            .session
            .rebuild
            .lock()
            .expect("shared rebuild mutex poisoned")
            .clone();
        if reload.is_none() && rebuild.is_none() {
            return self
                .operation_token_matches(expected)
                .then(commit)
                .transpose();
        }
        let lease = &self.session.registry_lease;
        let Some(registry) = lease.registry.upgrade() else {
            return Ok(None);
        };
        let Some(active) = registry.active_operation(&lease.key)? else {
            return Ok(None);
        };
        let Some(operation) = [reload, rebuild]
            .into_iter()
            .flatten()
            .find(|operation| Arc::ptr_eq(operation, &active))
        else {
            return Ok(None);
        };
        let _phase = registry.begin_operation_phase(&lease.key, &operation)?;
        self.operation_token_matches(expected)
            .then(commit)
            .transpose()
    }

    pub(crate) fn finish_rebuild(&self, result: anyhow::Result<()>) -> bool {
        self.finish_explicit_operation(&self.session.rebuild, result, false)
    }

    fn finish_explicit_operation(
        &self,
        slot: &Mutex<Option<Arc<SharedAnalyzerReload>>>,
        result: anyhow::Result<()>,
        notify: bool,
    ) -> bool {
        let lease = &self.session.registry_lease;
        let Some(registry) = lease.registry.upgrade() else {
            return false;
        };
        let Ok(mut slot) = slot.lock() else {
            return false;
        };
        let Some(operation) = slot.take() else {
            return false;
        };
        let succeeded = result.is_ok();
        let result = result.map(|_| self.session.workspace_indexes.clone());
        let finished = registry.finish_operation(&lease.key, &operation, &result);
        if !finished && !operation.finished() {
            *slot = Some(operation);
        } else if finished && succeeded && notify {
            self.notify_workspace_updates();
        }
        finished
    }

    pub(crate) fn set_busy(&self, busy: bool) {
        if self.session.busy.swap(busy, Ordering::SeqCst) != busy {
            self.session.gc.set_session_busy(busy);
        }
    }

    pub(crate) fn config_generation_changed(&self) -> bool {
        let generation = self.session.input_generation.load(Ordering::SeqCst);
        self.session
            .config_generation_seen
            .swap(generation, Ordering::SeqCst)
            != generation
    }

    pub(crate) fn workspace_updates(&self) -> crossbeam_channel::Receiver<()> {
        self.session.workspace_updates.clone()
    }

    pub(crate) fn workspace_update_pending(&self) -> bool {
        self.session.workspace_update_pending.load(Ordering::SeqCst)
    }

    pub(crate) fn take_workspace_update(&self, key: &SharedAnalyzerBackendKey) -> bool {
        self.session.registry_lease.key == *key
            && self
                .session
                .workspace_update_pending
                .swap(false, Ordering::SeqCst)
    }

    fn notify_workspace_updates(&self) {
        if let Ok(world) = self.session.world.lock() {
            world.notify_workspace_updates(self.session_id(), &self.session.workspace_indexes);
        }
    }

    pub(crate) fn proc_macro_clients(
        &self,
    ) -> triomphe::Arc<[Option<anyhow::Result<ProcMacroClient>>]> {
        let world = self
            .session
            .world
            .lock()
            .expect("shared world mutex poisoned");
        triomphe::Arc::from_iter(world.proc_macro_clients_for(&self.session.workspace_indexes))
    }

    pub(crate) fn finish_reload(&self, result: anyhow::Result<()>) -> bool {
        self.finish_explicit_operation(&self.session.reload, result, true)
    }

    pub(crate) fn update_build_data(
        &self,
        workspaces: &[ProjectWorkspace],
        build_scripts: &[anyhow::Result<WorkspaceBuildScripts>],
        expected_generation: u64,
        operation: &SharedAnalyzerOperationToken,
    ) -> anyhow::Result<bool> {
        let scope = self
            .reload_keys()
            .map_or(BuildDataScope::Normal, BuildDataScope::Reload);
        self.apply_build_data(
            workspaces,
            build_scripts,
            expected_generation,
            operation,
            scope,
        )
    }

    pub(crate) fn reload_active(&self) -> bool {
        self.session
            .reload
            .lock()
            .expect("shared reload mutex poisoned")
            .as_ref()
            .is_some_and(|reload| reload.turn_ready() && !reload.finished())
    }

    pub(crate) fn workspace_generation(&self) -> u64 {
        self.session
            .world
            .lock()
            .expect("shared world mutex poisoned")
            .workspace_generation()
    }

    pub(crate) fn build_data_pending(&self) -> bool {
        if !self.operation_ready().unwrap_or(false) {
            return false;
        }
        self.session
            .world
            .lock()
            .expect("shared world mutex poisoned")
            .build_data_pending(&self.session.workspace_indexes)
    }

    pub(crate) fn proc_macros_pending(&self) -> bool {
        if !self.operation_ready().unwrap_or(false) {
            return false;
        }
        let config = Some(self.session.config.as_ref());
        let world = self
            .session
            .world
            .lock()
            .expect("shared world mutex poisoned");
        let operation_keys = self.operation_keys();
        world.proc_macros_pending(
            &self.session.workspace_indexes,
            operation_keys.as_deref(),
            config,
        )
    }

    pub(crate) fn proc_macro_load_request(&self) -> Option<SharedProcMacroLoadRequest> {
        if !self.operation_ready().unwrap_or(false) {
            return None;
        }
        let config = self.session.config.as_ref();
        let world = self
            .session
            .world
            .lock()
            .expect("shared world mutex poisoned");
        let operation_keys = self.operation_keys();
        world.proc_macro_load_request(
            &self.session.workspace_indexes,
            &config.load.key.ignored_proc_macros,
            operation_keys.as_deref(),
            Some(config),
        )
    }

    pub(crate) fn commit_proc_macro_load(
        &self,
        response: SharedProcMacroLoadResponse,
        operation: &SharedAnalyzerOperationToken,
    ) -> anyhow::Result<bool> {
        self.commit_operation(operation, || {
            let _write = self.session.access.write(None);
            let mut world = self.session.world.lock().map_err(|error| {
                anyhow::format_err!("shared world mutex is poisoned: {error}")
            })?;
            let generation = world.input_generation.load(Ordering::SeqCst);
            let result = world.commit_proc_macro_load(response);
            if world.input_generation.load(Ordering::SeqCst) != generation {
                self.session.gc.changed();
            }
            result
        })
        .map(|result| result.unwrap_or(false))
    }

    pub(crate) fn update_rebuild_build_data(
        &self,
        workspaces: &[ProjectWorkspace],
        build_scripts: &[anyhow::Result<WorkspaceBuildScripts>],
        expected_generation: u64,
        operation: &SharedAnalyzerOperationToken,
    ) -> anyhow::Result<bool> {
        self.apply_build_data(
            workspaces,
            build_scripts,
            expected_generation,
            operation,
            BuildDataScope::Rebuild,
        )
    }

    fn apply_build_data(
        &self,
        workspaces: &[ProjectWorkspace],
        build_scripts: &[anyhow::Result<WorkspaceBuildScripts>],
        expected_generation: u64,
        operation: &SharedAnalyzerOperationToken,
        scope: BuildDataScope,
    ) -> anyhow::Result<bool> {
        let config = &self.session.config;
        if !self.operation_ready()? {
            return Ok(false);
        }
        let snapshot = self
            .session
            .world
            .lock()
            .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?
            .build_data_snapshot(
                &self.session.workspace_indexes,
                workspaces,
                build_scripts,
                config,
                expected_generation,
                scope,
            )?;
        let Some(snapshot) = snapshot else {
            return Ok(false);
        };
        let mut prepared = snapshot.prepare(config);
        let Some(committed) = self.commit_operation(operation, || {
            let _write = self.session.access.write(None);
            let mut world = self.session.world.lock().map_err(|error| {
                anyhow::format_err!("shared world mutex is poisoned: {error}")
            })?;
            let generation = world.input_generation.load(Ordering::SeqCst);
            let result = world.commit_build_data(&mut prepared);
            if world.input_generation.load(Ordering::SeqCst) != generation {
                self.session.gc.changed();
            }
            result
        })?
        else {
            return Ok(false);
        };
        if committed
            && let Some((rebuild, keys)) = prepared.operation_keys
        {
            self.set_operation_keys(rebuild, keys);
        }
        Ok(committed)
    }

    fn workspace_indexes(&self) -> &[usize] {
        &self.session.workspace_indexes
    }

    fn refresh_session_cache_from_world(&self) {
        let world = self
            .session
            .world
            .lock()
            .expect("shared world mutex poisoned");
        self.refresh_session_cache(&world);
    }

    fn refresh_session_cache(&self, world: &SharedWorld) {
        let line_endings = world.session_line_endings(self.session_id(), self.workspace_indexes());
        let file_mappings =
            world.session_file_mappings(self.session_id(), self.workspace_indexes());
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

    fn analysis_snapshot(
        &self,
        foreign_epoch: u64,
    ) -> (
        Analysis,
        SharedAnalyzerSnapshotToken,
    ) {
        let snapshot = SharedAnalyzerSnapshotToken {
            session: Arc::clone(&self.session),
            base_generation: self.session.input_generation.load(Ordering::SeqCst),
            overlay_generation: self.session.overlay_generation.load(Ordering::SeqCst),
            foreign_epoch,
        };
        let world = self
            .session
            .world
            .lock()
            .expect("shared world mutex poisoned");
        let generation = snapshot.base_generation;
        let mut cache = self
            .session
            .analysis_cache
            .lock()
            .expect("shared analyzer analysis cache mutex poisoned");
        if cache.generation != Some(generation) {
            cache.visible_files = Arc::new(world.session_visible_crate_roots(
                self.session_id(),
                self.workspace_indexes(),
                &self.session.excluded_paths,
            ));
            self.refresh_session_cache(&world);
            cache.generation = Some(generation);
        }
        let visible_files = Arc::clone(&cache.visible_files);
        drop(cache);
        (world.host.analysis_with_visible_files(visible_files), snapshot)
    }

    pub(crate) fn analysis(&self) -> Analysis {
        let gc = self.session.gc.read();
        let read = self.session.access.read(self.session_id());
        let (analysis, snapshot) = self.analysis_snapshot(read.foreign_epoch());
        analysis.with_guard(SharedAnalyzerAnalysisGuard {
            _world: read,
            _gc: gc,
            snapshot,
        })
    }

    pub(crate) fn pending_analysis(&self) -> SharedAnalyzerPendingAnalysis {
        SharedAnalyzerPendingAnalysis {
            runtime: self.clone(),
        }
    }

    pub(crate) fn snapshot_token(&self, analysis: &Analysis) -> SharedAnalyzerSnapshotToken {
        let snapshot = &analysis
            .guard::<SharedAnalyzerAnalysisGuard>()
            .expect("shared analyzer analysis must retain its access permits")
            .snapshot;
        assert!(Arc::ptr_eq(&self.session, &snapshot.session));
        snapshot.clone()
    }

    pub(crate) fn url_to_file_id(&self, url: &Uri) -> anyhow::Result<Option<FileId>> {
        let path = crate::lsp::from_proto::vfs_path(url)?;
        self.vfs_path_to_file_id(&path)
    }

    pub(crate) fn base_url_to_file_id(&self, url: &Uri) -> anyhow::Result<Option<FileId>> {
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

    pub(crate) fn file_id_to_url(&self, file_id: FileId) -> Option<Uri> {
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

    pub(crate) fn ratoml_files(&self) -> Vec<(VfsPath, SourceRootId, bool, String)> {
        let world = self
            .session
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
            for key in overlay.open_files.keys() {
                let Some(file) = overlay.files_by_path.get(key) else {
                    continue;
                };
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
        self.session
            .world
            .lock()
            .expect("shared world mutex poisoned")
            .source_root_parent_map(self.workspace_indexes())
    }

    pub(crate) fn source_root_for_path(&self, path: &VfsPath) -> Option<(SourceRootId, bool)> {
        self.session
            .world
            .lock()
            .expect("shared world mutex poisoned")
            .source_root_for_path(self.workspace_indexes(), path)
    }

    pub(crate) fn apply_base_file_changes(
        &self,
        files: Vec<SharedBaseFileChange>,
    ) -> anyhow::Result<()> {
        if files.is_empty() {
            return Ok(());
        }

        let base_file_access = self
            .session
            .world
            .lock()
            .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?
            .base_file_access();
        let _base_files = base_file_access
            .lock()
            .map_err(|error| anyhow::format_err!("shared base-file mutex is poisoned: {error}"))?;
        let _write = self.session.access.write(Some(self.session_id()));
        let mut world = self
            .session
            .world
            .lock()
            .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?;
        let changed = world.apply_base_file_changes(self.workspace_indexes(), files)?;
        if changed {
            self.session.gc.changed();
        }
        self.refresh_session_cache(&world);
        Ok(())
    }

    pub(crate) fn sync_open_files(
        &self,
        files: Vec<(VfsPath, String, crate::line_index::LineEndings)>,
        force_rebuild: bool,
    ) -> anyhow::Result<SharedOverlaySync> {
        let _overlay_update = self
            .session
            .access
            .overlay_update
            .lock()
            .expect("shared overlay update mutex poisoned");
        let _read = self.session.access.read(self.session_id());
        let files = {
            let world = self
                .session
                .world
                .lock()
                .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?;
            let overlay_needed = world
                .session_overlays
                .get(&self.session_id())
                .is_some_and(|overlay| !overlay.files_by_path.is_empty())
                || files.iter().any(|(path, text, line_endings)| {
                    let source_path = normalize_vfs_path(path);
                    let Some(base_file) = world
                        .base_file(self.workspace_indexes(), &source_path)
                        .filter(|&file_id| {
                            world.base_file_exists(self.workspace_indexes(), file_id)
                        })
                    else {
                        return world
                            .source_root_for_path(self.workspace_indexes(), &source_path)
                            .is_some();
                    };
                    let db = world.host.raw_database();
                    db.file_text(base_file).text(db).as_ref() != text.as_str()
                        || world.base_line_endings(self.workspace_indexes(), base_file)
                            != Some(*line_endings)
                });
            if !overlay_needed {
                self.refresh_session_cache(&world);
                return Ok(SharedOverlaySync::default());
            }
            if !force_rebuild
                && world.can_update_session_overlay(
                    self.session_id(),
                    self.workspace_indexes(),
                    &files,
                ) {
                PreparedSessionOverlay::Update(files)
            } else {
                let files =
                    world.prepare_session_overlay_files(self.workspace_indexes(), files)?;
                if !force_rebuild
                    && !world.session_overlay_changed(
                        self.session_id(),
                        self.workspace_indexes(),
                        &files,
                    ) {
                    self.refresh_session_cache(&world);
                    return Ok(SharedOverlaySync::default());
                }
                PreparedSessionOverlay::Rebuild(files)
            }
        };
        let _write = self.session.access.write_overlay(self.session_id(), || {
            self.session
                .world
                .lock()
                .expect("shared world mutex poisoned")
                .host
                .trigger_cancellation();
        });
        let mut world = self
            .session
            .world
            .lock()
            .map_err(|error| anyhow::format_err!("shared world mutex is poisoned: {error}"))?;
        let sync = match files {
            PreparedSessionOverlay::Update(files) => {
                world.update_session_overlay(self.session_id(), files)
            }
            PreparedSessionOverlay::Rebuild(files) => {
                world.sync_session_overlay(
                    self.session_id(),
                    self.workspace_indexes(),
                    files,
                    force_rebuild,
                )?
            }
        };
        if sync.changed {
            self.session
                .overlay_generation
                .fetch_add(1, Ordering::SeqCst);
            self.session.gc.changed();
            self.session
                .analysis_cache
                .lock()
                .expect("shared analyzer analysis cache mutex poisoned")
                .generation = None;
        }
        self.refresh_session_cache(&world);
        Ok(sync)
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

pub(crate) fn path_key(path: &VfsPath) -> String {
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
    workspaces: Vec<(
        Arc<LoadedWorkspaceFiles>,
        Arc<BTreeMap<FileId, crate::line_index::LineEndings>>,
    )>,
    overlay: BTreeMap<FileId, crate::line_index::LineEndings>,
}

impl SharedLineEndings {
    fn get(&self, file_id: FileId) -> Option<crate::line_index::LineEndings> {
        for (files, line_endings) in &self.workspaces {
            if files.exists(file_id)
                && let Some(line_endings) = line_endings.get(&file_id)
            {
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

        let mut found = false;
        for workspace in &self.workspaces {
            if workspace.contains_file(file_id) {
                found = true;
                if workspace.exists(file_id) {
                    return Some(true);
                }
            }
        }
        found.then_some(false)
    }
}

fn common_path_prefix_len(left: &str, right: &str) -> usize {
    fn is_path_separator(byte: u8) -> bool {
        matches!(byte, b'/' | b'\\')
    }

    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut index = 0;
    let mut last_separator = 0;
    let end = left.len().min(right.len());

    while index < end && left[index] == right[index] {
        if is_path_separator(left[index]) {
            last_separator = index + 1;
        }
        index += 1;
    }

    if index == end
        && (left.len() == right.len()
            || left.get(index).is_some_and(|byte| is_path_separator(*byte))
            || right
                .get(index)
                .is_some_and(|byte| is_path_separator(*byte)))
    {
        return index;
    }

    last_separator
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SharedOverlaySync {
    pub(crate) changed: bool,
    pub(crate) removed_files: Vec<FileId>,
}

pub(crate) struct SharedBaseFileChange {
    pub(crate) path: VfsPath,
    pub(crate) text: Option<String>,
    pub(crate) exists: bool,
    pub(crate) line_endings: Option<crate::line_index::LineEndings>,
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
    text: String,
}

struct PreparedOverlayFile {
    base_file: Option<FileId>,
    base_source_root: SourceRootId,
    path: VfsPath,
    display_path: VfsPath,
    text: String,
    line_endings: crate::line_index::LineEndings,
    is_open: bool,
}

enum PreparedSessionOverlay {
    Update(Vec<(VfsPath, String, crate::line_index::LineEndings)>),
    Rebuild(Vec<PreparedOverlayFile>),
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

struct WorkspaceUpdate {
    workspace_indexes: Vec<usize>,
    pending: Arc<AtomicBool>,
    sender: crossbeam_channel::Sender<()>,
}

struct LoadedWorkspaceInput {
    source_roots: Vec<SourceRoot>,
    source_root_config: SourceRootConfig,
    crate_graph: CrateGraphBuilder,
    proc_macro_paths: ProcMacroPaths,
    proc_macros: Vec<ProcMacroLoad>,
    proc_macros_loaded: bool,
    build_data_loaded: bool,
}

#[derive(Clone)]
struct LoadedWorkspaceFile {
    path: VfsPath,
    exists: bool,
}

#[derive(Clone)]
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

    fn insert(&mut self, file_id: FileId, path: VfsPath, exists: bool) {
        self.file_ids_by_path.insert(path_key(&path), file_id);
        self.files_by_id
            .insert(file_id, LoadedWorkspaceFile { path, exists });
    }

    fn set_exists(&mut self, file_id: FileId, exists: bool) {
        if let Some(file) = self.files_by_id.get_mut(&file_id) {
            file.exists = exists;
        }
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
        self.files_by_id.get(&file_id).map(|file| &file.path)
    }
}

type ProcMacroSpawnKey = (
    AbsPathBuf,
    Option<semver::Version>,
    FxHashMap<String, Option<String>>,
);
type SharedProcMacroClient = (ProcMacroSpawnKey, Arc<ProcMacroClient>);
type ProcMacroPoolKey = (ProcMacroSpawnKey, usize);

#[derive(Default)]
struct SharedProcMacroPools(Mutex<Vec<(ProcMacroPoolKey, Weak<ProcMacroClient>)>>);

impl SharedProcMacroPools {
    fn client(
        &self,
        key: ProcMacroSpawnKey,
        processes: usize,
        rejected: &[Arc<ProcMacroClient>],
    ) -> Result<Arc<ProcMacroClient>, ProcMacroLoadingError> {
        let pool_key = (key.clone(), processes);
        let mut pools = self.0.lock().map_err(proc_macro_loading_error)?;
        pools.retain(|(_, client)| client.strong_count() != 0);

        if let Some(client) = pools
            .iter()
            .find(|(candidate, _)| candidate == &pool_key)
            .and_then(|(_, client)| client.upgrade())
            .filter(|client| client.exited().is_none())
            .filter(|client| !rejected.iter().any(|old| Arc::ptr_eq(old, client)))
        {
            return Ok(client);
        }

        let (path, toolchain, env) = &key;
        let client = Arc::new(
            ProcMacroClient::spawn(path, env, toolchain.as_ref(), processes)
                .map_err(proc_macro_loading_error)?,
        );
        pools.retain(|(candidate, _)| candidate != &pool_key);
        pools.push((pool_key, Arc::downgrade(&client)));
        Ok(client)
    }
}

struct LoadedWorkspace {
    summary: WorkspaceSummary,
    workspace: ProjectWorkspace,
    input: LoadedWorkspaceInput,
    _vfs: Arc<LoadedWorkspaceFiles>,
    line_endings: Arc<BTreeMap<FileId, crate::line_index::LineEndings>>,
    source_root_parent_map: FxHashMap<SourceRootId, SourceRootId>,
    proc_macro_client: Option<Result<SharedProcMacroClient, ProcMacroLoadingError>>,
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
    loaded: WorkspaceLoad,
    line_endings: BTreeMap<FileId, crate::line_index::LineEndings>,
    proc_macro_client: Option<Result<SharedProcMacroClient, ProcMacroLoadingError>>,
    proc_macros_loaded: bool,
    build_data_loaded: bool,
}

struct BuildDataWorkspace {
    index: usize,
    workspace: ProjectWorkspace,
    files: Arc<LoadedWorkspaceFiles>,
    build_data_loaded: bool,
    proc_macro_client: Option<Result<SharedProcMacroClient, ProcMacroLoadingError>>,
}

struct BuildDataSnapshot {
    generation: u64,
    workspaces: Vec<BuildDataWorkspace>,
    operation_keys: Option<(bool, Vec<ProcMacroSpawnKey>)>,
}

struct PreparedBuildDataWorkspace {
    index: usize,
    workspace: ProjectWorkspace,
    files: Arc<LoadedWorkspaceFiles>,
    source_root_config: SourceRootConfig,
    crate_graph: CrateGraphBuilder,
    proc_macro_paths: ProcMacroPaths,
    proc_macros: Vec<ProcMacroLoad>,
    proc_macros_loaded: bool,
    build_data_loaded: bool,
    proc_macro_client: Option<Result<SharedProcMacroClient, ProcMacroLoadingError>>,
}

struct PreparedBuildData {
    generation: u64,
    workspaces: Vec<PreparedBuildDataWorkspace>,
    operation_keys: Option<(bool, Vec<ProcMacroSpawnKey>)>,
}

enum BuildDataScope {
    Normal,
    Reload(Vec<ProcMacroSpawnKey>),
    Rebuild,
}

#[derive(Debug)]
pub(crate) enum SharedProcMacroProgress {
    Begin,
    Report(String),
    End(SharedProcMacroLoadResponse),
}

#[derive(Debug)]
pub(crate) struct SharedProcMacroLoadResponse {
    pub(crate) generation: u64,
    pub(crate) workspaces: Vec<(usize, Vec<ProcMacroLoad>)>,
}

struct SharedProcMacroWorkspace {
    index: usize,
    client: Option<Result<ProcMacroClient, ProcMacroLoadingError>>,
    paths: ProcMacroPaths,
}

pub(crate) struct SharedProcMacroLoadRequest {
    generation: u64,
    workspaces: Vec<SharedProcMacroWorkspace>,
    ignored_proc_macros: Vec<(Box<str>, Vec<Box<str>>)>,
}

impl SharedProcMacroLoadRequest {
    pub(crate) fn load(self, progress: impl Fn(String)) -> SharedProcMacroLoadResponse {
        let workspaces = self
            .workspaces
            .into_iter()
            .map(|workspace| {
                let proc_macros = collect_proc_macros(
                    &workspace.client,
                    workspace.paths,
                    &self.ignored_proc_macros,
                    ProcMacroLoadState::Ready,
                    &|path| progress(path.to_string()),
                );
                (workspace.index, proc_macros)
            })
            .collect();
        SharedProcMacroLoadResponse {
            generation: self.generation,
            workspaces,
        }
    }
}

impl BuildDataSnapshot {
    fn prepare(self, config: &SharedAnalyzerConfig) -> PreparedBuildData {
        let load_config = config.load.to_load_cargo_config();
        let rebuild = self
            .operation_keys
            .as_ref()
            .is_some_and(|(rebuild, _)| *rebuild);
        let rejected = if rebuild {
            self.workspaces
                .iter()
                .filter_map(|workspace| {
                    workspace
                        .proc_macro_client
                        .as_ref()?
                        .as_ref()
                        .ok()
                        .map(|(_, client)| Arc::clone(client))
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let workspaces = self
            .workspaces
            .into_iter()
            .map(|snapshot| {
                let source_root_config = workspace_source_root_config(&snapshot.workspace);
                let existing_client = (!rebuild).then_some(snapshot.proc_macro_client).flatten();
                let (client_state, proc_macro_server) = match existing_client {
                    Some(Ok((key, client))) => {
                        (
                            Some(Ok((key, Arc::clone(&client)))),
                            Some(Ok(client.as_ref().clone())),
                        )
                    }
                    Some(Err(error)) => (Some(Err(error.clone())), Some(Err(error))),
                    None => match spawn_proc_macro_server(
                        &snapshot.workspace,
                        &config.cargo_config.extra_env,
                        &load_config,
                        &rejected,
                    ) {
                        Some(Ok((key, client))) => {
                            (
                                Some(Ok((key, Arc::clone(&client)))),
                                Some(Ok(client.as_ref().clone())),
                            )
                        }
                        Some(Err(error)) => (Some(Err(error.clone())), Some(Err(error))),
                        None => (None, None),
                    },
                };
                let (crate_graph, proc_macro_paths) = snapshot.workspace.to_crate_graph(
                    &mut |path| {
                        snapshot
                            .files
                            .file_id(&VfsPath::from(path.to_path_buf()))
                            .map(|(file_id, _)| file_id)
                    },
                    &config.cargo_config.extra_env,
                );
                let proc_macro_state = metadata_proc_macro_state(config);
                let proc_macros = collect_proc_macros(
                    &proc_macro_server,
                    proc_macro_paths.clone(),
                    &config.load.key.ignored_proc_macros,
                    proc_macro_state,
                    &|_| {},
                );
                let proc_macros_loaded = proc_macro_state != ProcMacroLoadState::NotYetBuilt
                    || proc_macro_paths.is_empty();
                PreparedBuildDataWorkspace {
                    index: snapshot.index,
                    workspace: snapshot.workspace,
                    files: snapshot.files,
                    source_root_config,
                    crate_graph,
                    proc_macro_paths,
                    proc_macros,
                    proc_macros_loaded,
                    build_data_loaded: snapshot.build_data_loaded,
                    proc_macro_client: client_state,
                }
            })
            .collect();
        PreparedBuildData {
            generation: self.generation,
            workspaces,
            operation_keys: self.operation_keys,
        }
    }
}

// Workspaces referring to the same proc-macro server executable (i.e. the same
// sysroot) with an identical spawn environment share a single client, and thereby
// a single set of server processes.
fn metadata_proc_macro_state(config: &SharedAnalyzerConfig) -> ProcMacroLoadState {
    match &config.load.key.proc_macro_server {
        SharedAnalyzerProcMacroServerKey::None => ProcMacroLoadState::Disabled,
        SharedAnalyzerProcMacroServerKey::Sysroot
        | SharedAnalyzerProcMacroServerKey::Explicit(_) => ProcMacroLoadState::NotYetBuilt,
    }
}

fn proc_macro_spawn_key(
    workspace: &ProjectWorkspace,
    extra_env: &FxHashMap<String, Option<String>>,
    load_config: &LoadCargoConfig,
) -> Option<Result<ProcMacroSpawnKey, ProcMacroLoadingError>> {
    let path = match &load_config.with_proc_macro_server {
        ProcMacroServerChoice::Sysroot => match workspace.find_sysroot_proc_macro_srv()? {
            Ok(path) => path,
            Err(error) => return Some(Err(proc_macro_loading_error(error))),
        },
        ProcMacroServerChoice::Explicit(path) => path.clone(),
        ProcMacroServerChoice::None => return None,
    };

    let env: FxHashMap<_, _> = match &workspace.kind {
        ProjectWorkspaceKind::Cargo { cargo, .. }
        | ProjectWorkspaceKind::DetachedFile {
            cargo: Some((cargo, ..)),
            ..
        } => cargo
            .env()
            .into_iter()
            .map(|(k, v)| (k.clone(), Some(v.clone())))
            .chain(extra_env.iter().map(|(k, v)| (k.clone(), v.clone())))
            .chain(
                workspace
                    .sysroot
                    .root()
                    .filter(|_| {
                        !extra_env.contains_key("RUSTUP_TOOLCHAIN")
                            && env::var_os("RUSTUP_TOOLCHAIN").is_none()
                    })
                    .map(|it| ("RUSTUP_TOOLCHAIN".to_owned(), Some(it.to_string()))),
            )
            .collect(),

        _ => Default::default(),
    };

    Some(Ok((path, workspace.toolchain.clone(), env)))
}

fn spawn_proc_macro_server(
    workspace: &ProjectWorkspace,
    extra_env: &FxHashMap<String, Option<String>>,
    load_config: &LoadCargoConfig,
    rejected: &[Arc<ProcMacroClient>],
) -> Option<Result<SharedProcMacroClient, ProcMacroLoadingError>> {
    static POOLS: OnceLock<SharedProcMacroPools> = OnceLock::new();

    let key = match proc_macro_spawn_key(workspace, extra_env, load_config)? {
        Ok(key) => key,
        Err(error) => return Some(Err(error)),
    };

    Some(
        POOLS
            .get_or_init(SharedProcMacroPools::default)
            .client(
                key.clone(),
                load_config.proc_macro_processes,
                rejected,
            )
            .map(|client| (key, client)),
    )
}

fn proc_macro_loading_error(error: impl ToString) -> ProcMacroLoadingError {
    ProcMacroLoadingError::ProcMacroSrvError(error.to_string().into_boxed_str())
}

struct SharedWorld {
    access: Arc<SharedWorldAccess>,
    base_file_access: Arc<Mutex<()>>,
    base_file_ids: Arc<SharedBaseFileIds>,
    host: AnalysisHost,
    loaded_workspaces: Vec<LoadedWorkspace>,
    workspace_indexes: BTreeMap<String, usize>,
    base_crates: Vec<ide::Crate>,
    base_max_source_root: Option<u32>,
    session_overlays: BTreeMap<u64, ActiveSessionOverlay>,
    workspace_updates: BTreeMap<u64, WorkspaceUpdate>,
    input_generation: Arc<AtomicU64>,
    workspace_generation: u64,
    applied_source_roots: Vec<SourceRoot>,
    applied_local_roots: rustc_hash::FxHashSet<SourceRootId>,
    applied_library_roots: rustc_hash::FxHashSet<SourceRootId>,
    applied_overlay_files: rustc_hash::FxHashSet<FileId>,
    next_session_id: u64,
}

impl SharedWorld {
    fn new(config: &SharedAnalyzerDatabaseConfigKey) -> Self {
        let mut host = AnalysisHost::new(config.lru_parse_query_capacity);
        if !config.lru_query_capacities.is_empty() {
            host.update_lru_capacities(
                &config
                    .lru_query_capacities
                    .iter()
                    .map(|(query, capacity)| (query.clone(), *capacity))
                    .collect(),
            );
        }
        hir::db::set_expand_proc_attr_macros(
            host.raw_database_mut(),
            config.expand_proc_attr_macros,
        );

        Self {
            access: Arc::new(SharedWorldAccess::default()),
            base_file_access: Arc::new(Mutex::new(())),
            base_file_ids: Arc::new(SharedBaseFileIds::default()),
            host,
            loaded_workspaces: Vec::new(),
            workspace_indexes: BTreeMap::new(),
            base_crates: Vec::new(),
            base_max_source_root: None,
            session_overlays: BTreeMap::new(),
            workspace_updates: BTreeMap::new(),
            input_generation: Arc::new(AtomicU64::new(0)),
            workspace_generation: 0,
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

    fn proc_macro_reload_keys(
        &self,
        load_keys: &[String],
        config: &SharedAnalyzerConfig,
    ) -> Vec<ProcMacroSpawnKey> {
        let load_config = config.load.to_load_cargo_config();
        let mut keys = Vec::new();
        for load_key in load_keys {
            let Some(&index) = self.workspace_indexes.get(load_key) else {
                continue;
            };
            if let Some(Ok((key, _))) = self.loaded_workspaces[index].proc_macro_client.as_ref()
                && !keys.iter().any(|old| old == key)
            {
                keys.push(key.clone());
            }
            if let Some(Ok(key)) = proc_macro_spawn_key(
                &self.loaded_workspaces[index].workspace,
                &config.cargo_config.extra_env,
                &load_config,
            ) && !keys.iter().any(|old| old == &key)
            {
                keys.push(key);
            }
        }
        keys
    }

    fn proc_macro_clients(&self) -> Vec<SharedProcMacroClient> {
        self.loaded_workspaces
            .iter()
            .filter_map(|workspace| {
                workspace.proc_macro_client.as_ref()?.as_ref().ok().cloned()
            })
            .collect()
    }

    fn proc_macro_clients_for(
        &self,
        workspace_indexes: &[usize],
    ) -> Vec<Option<anyhow::Result<ProcMacroClient>>> {
        workspace_indexes
            .iter()
            .map(|&index| {
                self.loaded_workspaces[index]
                    .proc_macro_client
                    .as_ref()
                    .map(|result| match result {
                        Ok((_, client)) => Ok(client.as_ref().clone()),
                        Err(error) => Err(anyhow::format_err!("{error}")),
                    })
            })
            .collect()
    }

    fn build_data_snapshot(
        &self,
        workspace_indexes: &[usize],
        workspaces: &[ProjectWorkspace],
        build_scripts: &[anyhow::Result<WorkspaceBuildScripts>],
        config: &SharedAnalyzerConfig,
        expected_generation: u64,
        scope: BuildDataScope,
    ) -> anyhow::Result<Option<BuildDataSnapshot>> {
        if self.workspace_generation != expected_generation {
            return Ok(None);
        }
        if workspace_indexes.len() != workspaces.len() || workspaces.len() != build_scripts.len() {
            anyhow::bail!("shared build-data response is inconsistent");
        }

        let mut updated_workspaces = BTreeMap::new();
        for ((&index, workspace), build_scripts) in
            workspace_indexes.iter().zip(workspaces).zip(build_scripts)
        {
            let mut workspace = workspace.clone();
            workspace.set_build_scripts(build_scripts.as_ref().ok().cloned().unwrap_or_default());
            updated_workspaces.insert(index, workspace);
        }

        let load_config = config.load.to_load_cargo_config();
        let mut keys = match &scope {
            BuildDataScope::Normal | BuildDataScope::Rebuild => Vec::new(),
            BuildDataScope::Reload(keys) => keys.clone(),
        };
        match &scope {
            BuildDataScope::Normal => {}
            BuildDataScope::Reload(_) => {
                for workspace in updated_workspaces.values() {
                    if let Some(Ok(key)) = proc_macro_spawn_key(
                        workspace,
                        &config.cargo_config.extra_env,
                        &load_config,
                    ) && !keys.iter().any(|old| old == &key)
                    {
                        keys.push(key);
                    }
                }
            }
            BuildDataScope::Rebuild => {
                for (&index, workspace) in workspace_indexes.iter().zip(workspaces) {
                    if let Some(Ok((key, _))) =
                        self.loaded_workspaces[index].proc_macro_client.as_ref()
                        && !keys.iter().any(|old| old == key)
                    {
                        keys.push(key.clone());
                    }
                    for workspace in [&self.loaded_workspaces[index].workspace, workspace] {
                        if let Some(Ok(key)) = proc_macro_spawn_key(
                            workspace,
                            &config.cargo_config.extra_env,
                            &load_config,
                        ) && !keys.iter().any(|old| old == &key)
                        {
                            keys.push(key);
                        }
                    }
                }
            }
        }

        let mut targets = workspace_indexes.to_vec();
        if !keys.is_empty() {
            for (index, workspace) in self.loaded_workspaces.iter().enumerate() {
                if targets.contains(&index) {
                    continue;
                }
                let shared = workspace
                    .proc_macro_client
                    .as_ref()
                    .and_then(|result| result.as_ref().ok())
                    .is_some_and(|(key, _)| keys.iter().any(|old| old == key))
                    || proc_macro_spawn_key(
                        &workspace.workspace,
                        &config.cargo_config.extra_env,
                        &load_config,
                    )
                    .and_then(Result::ok)
                    .is_some_and(|key| keys.iter().any(|old| old == &key));
                if shared {
                    targets.push(index);
                }
            }
        }
        targets.sort_unstable();

        let workspaces = targets
            .into_iter()
            .map(|index| {
                let workspace = updated_workspaces.remove(&index);
                BuildDataWorkspace {
                    index,
                    build_data_loaded: workspace.is_some()
                        || self.loaded_workspaces[index].input.build_data_loaded,
                    workspace: workspace
                        .unwrap_or_else(|| self.loaded_workspaces[index].workspace.clone()),
                    files: Arc::clone(&self.loaded_workspaces[index]._vfs),
                    proc_macro_client: self.loaded_workspaces[index].proc_macro_client.clone(),
                }
            })
            .collect();
        let operation_keys = match scope {
            BuildDataScope::Normal => None,
            BuildDataScope::Reload(_) => Some((false, keys)),
            BuildDataScope::Rebuild => Some((true, keys)),
        };
        Ok(Some(BuildDataSnapshot {
            generation: self.workspace_generation,
            workspaces,
            operation_keys,
        }))
    }

    fn commit_build_data(&mut self, prepared: &mut PreparedBuildData) -> anyhow::Result<bool> {
        if self.workspace_generation != prepared.generation
            || prepared.workspaces.iter().any(|prepared| {
                !Arc::ptr_eq(&self.loaded_workspaces[prepared.index]._vfs, &prepared.files)
            })
        {
            return Ok(false);
        }
        let revision = self.host.raw_database().nonce_and_revision().1;
        for prepared in prepared.workspaces.drain(..) {
            let loaded = &mut self.loaded_workspaces[prepared.index];
            loaded.workspace = prepared.workspace;
            loaded.input.source_root_config = prepared.source_root_config;
            loaded.input.source_roots = source_roots_for_files(
                &loaded.input.source_root_config,
                loaded
                    ._vfs
                    .iter()
                    .filter(|(file_id, _)| loaded._vfs.exists(*file_id))
                    .map(|(file_id, path)| (file_id, path.clone())),
            );
            loaded.source_root_parent_map =
                loaded.input.source_root_config.source_root_parent_map();
            loaded.input.crate_graph = prepared.crate_graph;
            loaded.input.proc_macro_paths = prepared.proc_macro_paths;
            loaded.input.proc_macros = prepared.proc_macros;
            loaded.input.proc_macros_loaded = prepared.proc_macros_loaded;
            loaded.input.build_data_loaded = prepared.build_data_loaded;
            loaded.proc_macro_client = prepared.proc_macro_client;
            loaded.summary.proc_macro_server = loaded
                .proc_macro_client
                .as_ref()
                .is_some_and(|result| result.is_ok());
        }

        self.apply_staged_inputs(revision)?;
        Ok(true)
    }

    fn proc_macro_indexes(
        &self,
        workspace_indexes: &[usize],
        reload_keys: Option<&[ProcMacroSpawnKey]>,
        config: Option<&SharedAnalyzerConfig>,
    ) -> Vec<usize> {
        let Some(keys) = reload_keys else {
            return workspace_indexes.to_vec();
        };
        let mut indexes = self
            .loaded_workspaces
            .iter()
            .enumerate()
            .filter_map(|(index, workspace)| {
                let client_key = workspace
                    .proc_macro_client
                    .as_ref()
                    .and_then(|result| result.as_ref().ok())
                    .map(|(key, _)| key);
                let workspace_key = config.and_then(|config| {
                    let load_config = config.load.to_load_cargo_config();
                    proc_macro_spawn_key(
                        &workspace.workspace,
                        &config.cargo_config.extra_env,
                        &load_config,
                    )
                    .and_then(Result::ok)
                });
                (client_key.is_some_and(|key| keys.iter().any(|old| old == key))
                    || workspace_key
                        .as_ref()
                        .is_some_and(|key| keys.iter().any(|old| old == key)))
                .then_some(index)
            })
            .collect::<Vec<_>>();
        for &index in workspace_indexes {
            if !indexes.contains(&index) {
                indexes.push(index);
            }
        }
        indexes.sort_unstable();
        indexes
    }

    fn build_data_pending(&self, workspace_indexes: &[usize]) -> bool {
        workspace_indexes.iter().any(|&index| {
            self.loaded_workspaces
                .get(index)
                .is_some_and(|workspace| !workspace.input.build_data_loaded)
        })
    }

    fn proc_macros_pending(
        &self,
        workspace_indexes: &[usize],
        reload_keys: Option<&[ProcMacroSpawnKey]>,
        config: Option<&SharedAnalyzerConfig>,
    ) -> bool {
        self.proc_macro_indexes(workspace_indexes, reload_keys, config)
            .into_iter()
            .any(|index| {
                let Some(workspace) = self.loaded_workspaces.get(index) else {
                    return false;
                };
                workspace.input.build_data_loaded && !workspace.input.proc_macros_loaded
            })
    }

    fn proc_macro_load_request(
        &self,
        workspace_indexes: &[usize],
        ignored_proc_macros: &[(Box<str>, Vec<Box<str>>)],
        reload_keys: Option<&[ProcMacroSpawnKey]>,
        config: Option<&SharedAnalyzerConfig>,
    ) -> Option<SharedProcMacroLoadRequest> {
        let workspace_indexes = self.proc_macro_indexes(workspace_indexes, reload_keys, config);
        let workspaces = workspace_indexes
            .iter()
            .filter_map(|&index| {
                let workspace = self.loaded_workspaces.get(index)?;
                let pending =
                    workspace.input.build_data_loaded && !workspace.input.proc_macros_loaded;
                pending.then(|| SharedProcMacroWorkspace {
                    index,
                    client: workspace.proc_macro_client.as_ref().map(|client| {
                        client
                            .as_ref()
                            .map(|(_, client)| client.as_ref().clone())
                            .map_err(Clone::clone)
                    }),
                    paths: workspace.input.proc_macro_paths.clone(),
                })
            })
            .collect::<Vec<_>>();
        (!workspaces.is_empty()).then(|| SharedProcMacroLoadRequest {
            generation: self.workspace_generation,
            workspaces,
            ignored_proc_macros: ignored_proc_macros.to_vec(),
        })
    }

    fn commit_proc_macro_load(
        &mut self,
        response: SharedProcMacroLoadResponse,
    ) -> anyhow::Result<bool> {
        if response.generation != self.workspace_generation {
            return Ok(false);
        }
        if response.workspaces.is_empty() {
            return Ok(true);
        }
        let revision = self.host.raw_database().nonce_and_revision().1;
        for (index, proc_macros) in response.workspaces {
            let Some(workspace) = self.loaded_workspaces.get_mut(index) else {
                return Ok(false);
            };
            workspace.input.proc_macros = proc_macros;
            workspace.input.proc_macros_loaded = true;
        }

        self.apply_staged_inputs(revision)?;
        Ok(true)
    }

    fn apply_staged_inputs(&mut self, revision: Revision) -> anyhow::Result<()> {
        self.apply_base_inputs(Vec::new());
        self.recone_session_overlays()?;
        if self.host.raw_database().nonce_and_revision().1 != revision {
            self.input_generation.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    fn access(&self) -> Arc<SharedWorldAccess> {
        Arc::clone(&self.access)
    }

    fn base_file_access(&self) -> Arc<Mutex<()>> {
        Arc::clone(&self.base_file_access)
    }

    fn workspace_generation(&self) -> u64 {
        self.workspace_generation
    }

    fn load_workspace(
        source: SharedAnalyzerWorkspaceLoadSource,
        config: &SharedAnalyzerConfig,
        progress: &(dyn Fn(String) + Sync),
    ) -> anyhow::Result<(String, ProjectWorkspace)> {
        match source {
            SharedAnalyzerWorkspaceLoadSource::Project(project) => {
                let load_key = shared_project_key(&project);
                let workspace = match project {
                    crate::config::LinkedProject::ProjectManifest(manifest) => {
                        ProjectWorkspace::load(manifest, &config.cargo_config, progress)?
                    }
                    crate::config::LinkedProject::InlineProjectJson(project) => {
                        ProjectWorkspace::load_inline(project, &config.cargo_config, progress)
                    }
                };
                Ok((load_key, workspace))
            }
            SharedAnalyzerWorkspaceLoadSource::DetachedFile(file) => {
                let load_key = shared_detached_file_key(&file);
                let workspace =
                    ProjectWorkspace::load_detached_files(vec![file], &config.cargo_config)
                        .into_iter()
                        .next()
                        .ok_or_else(|| {
                            anyhow::format_err!("detached file did not produce a workspace")
                        })??;
                Ok((load_key, workspace))
            }
        }
    }

    fn prepare_workspace_load(
        source: SharedAnalyzerWorkspaceLoadSource,
        config: &SharedAnalyzerConfig,
        base_file_ids: &SharedBaseFileIds,
        progress: &(dyn Fn(String) + Sync),
    ) -> anyhow::Result<PreparedWorkspaceLoad> {
        let (load_key, workspace) = Self::load_workspace(source, config, progress)?;
        Self::prepare_loaded_workspace(
            load_key,
            workspace,
            config,
            &[],
            metadata_proc_macro_state(config),
            base_file_ids,
        )
    }

    fn prepare_loaded_workspace(
        load_key: String,
        workspace: ProjectWorkspace,
        config: &SharedAnalyzerConfig,
        rejected_proc_macro_clients: &[Arc<ProcMacroClient>],
        proc_macro_state: ProcMacroLoadState,
        base_file_ids: &SharedBaseFileIds,
    ) -> anyhow::Result<PreparedWorkspaceLoad> {
        let manifest_path = workspace
            .manifest()
            .map(ToString::to_string)
            .unwrap_or_else(|| workspace.workspace_root().to_string());
        let summary_root = workspace.workspace_root().to_string();
        let packages = workspace.n_packages();
        let load_config = config.load.to_load_cargo_config();
        let (proc_macro_client, proc_macro_server) =
            if proc_macro_state != ProcMacroLoadState::Disabled {
                match spawn_proc_macro_server(
                    &workspace,
                    &config.cargo_config.extra_env,
                    &load_config,
                    rejected_proc_macro_clients,
                ) {
                    Some(Ok((key, client))) => {
                        (
                            Some(Ok((key, Arc::clone(&client)))),
                            Some(Ok(client.as_ref().clone())),
                        )
                    }
                    Some(Err(error)) => (Some(Err(error.clone())), Some(Err(error))),
                    None => (None, None),
                }
            } else {
                (None, None)
            };
        let session_workspace = workspace.clone();
        let loaded = load_workspace_change(
            workspace,
            &config.cargo_config.extra_env,
            &load_config,
            &config.load.key.ignored_proc_macros,
            proc_macro_server,
            proc_macro_state,
            |_, path| base_file_ids.resolve(path),
        )?;
        let files = loaded.vfs.iter().count();
        let line_endings = loaded
            .file_texts
            .iter()
            .map(|(file_id, text)| {
                let (_, line_endings) = crate::line_index::LineEndings::normalize(text.clone());
                (*file_id, line_endings)
            })
            .collect();
        let proc_macro_server = loaded.proc_macro_server.as_ref().is_some_and(Result::is_ok);
        let proc_macros_loaded = proc_macro_state != ProcMacroLoadState::NotYetBuilt
            || loaded.proc_macro_paths.is_empty();

        Ok(PreparedWorkspaceLoad {
            root_key: load_key,
            summary: WorkspaceSummary {
                root: summary_root,
                manifest: manifest_path,
                packages,
                files,
                proc_macro_server,
            },
            workspace: session_workspace,
            loaded,
            line_endings,
            proc_macro_client,
            proc_macros_loaded,
            build_data_loaded: !config.load.key.load_out_dirs_from_check,
        })
    }

    fn invalidate_proc_macro_groups(
        &mut self,
        workspace_indexes: &[usize],
        reload_keys: &[ProcMacroSpawnKey],
        config: &SharedAnalyzerConfig,
    ) {
        let load_config = config.load.to_load_cargo_config();
        let proc_macro_state = metadata_proc_macro_state(config);
        for (index, workspace) in self.loaded_workspaces.iter_mut().enumerate() {
            if workspace_indexes.contains(&index) {
                continue;
            }
            let shared = workspace
                .proc_macro_client
                .as_ref()
                .and_then(|result| result.as_ref().ok())
                .is_some_and(|(key, _)| reload_keys.iter().any(|old| old == key))
                || proc_macro_spawn_key(
                    &workspace.workspace,
                    &config.cargo_config.extra_env,
                    &load_config,
                )
                .and_then(Result::ok)
                .is_some_and(|key| reload_keys.iter().any(|old| old == &key));
            if !shared {
                continue;
            }
            workspace.input.proc_macros = collect_proc_macros(
                &None,
                workspace.input.proc_macro_paths.clone(),
                &config.load.key.ignored_proc_macros,
                proc_macro_state,
                &|_| {},
            );
            workspace.input.proc_macros_loaded = proc_macro_state
                != ProcMacroLoadState::NotYetBuilt
                || workspace.input.proc_macro_paths.is_empty();
            workspace.proc_macro_client = None;
            workspace.summary.proc_macro_server = false;
        }
    }

    fn commit_workspace_batch(
        &mut self,
        loaded: Vec<PreparedWorkspaceLoad>,
        reload_keys: &[ProcMacroSpawnKey],
        config: &SharedAnalyzerConfig,
    ) -> anyhow::Result<Vec<usize>> {
        let revision = self.host.raw_database().nonce_and_revision().1;
        let mut indexes = Vec::with_capacity(loaded.len());
        let mut file_texts = Vec::new();
        for loaded in loaded {
            if let Some(&index) = self.workspace_indexes.get(&loaded.root_key) {
                let old_files = Arc::clone(&self.loaded_workspaces[index]._vfs);
                let old_line_endings = Arc::clone(&self.loaded_workspaces[index].line_endings);
                let (workspace, texts) = self.remap_workspace_load(loaded);
                let mut workspace = workspace;
                self.preserve_workspace_files(&mut workspace, &old_files, &old_line_endings);
                self.loaded_workspaces[index] = workspace;
                file_texts.extend(texts);
                indexes.push(index);
            } else {
                let root_key = loaded.root_key.clone();
                let (workspace, texts) = self.remap_workspace_load(loaded);
                let index = self.loaded_workspaces.len();
                self.workspace_indexes.insert(root_key, index);
                self.loaded_workspaces.push(workspace);
                file_texts.extend(texts);
                indexes.push(index);
            }
        }

        self.invalidate_proc_macro_groups(&indexes, reload_keys, config);
        self.host.raw_database_mut().enable_proc_attr_macros();
        self.apply_base_inputs(file_texts);
        self.recone_session_overlays()?;
        if self.host.raw_database().nonce_and_revision().1 != revision {
            self.input_generation.fetch_add(1, Ordering::SeqCst);
            self.workspace_generation = self.workspace_generation.wrapping_add(1);
        }
        Ok(indexes)
    }

    fn commit_workspace(&mut self, loaded: PreparedWorkspaceLoad) -> usize {
        let revision = self.host.raw_database().nonce_and_revision().1;
        let result = self.commit_workspace_inner(loaded);
        if self.host.raw_database().nonce_and_revision().1 != revision {
            self.input_generation.fetch_add(1, Ordering::SeqCst);
            self.workspace_generation = self.workspace_generation.wrapping_add(1);
        }
        result
    }

    fn commit_workspace_inner(&mut self, loaded: PreparedWorkspaceLoad) -> usize {
        if let Some(&index) = self.workspace_indexes.get(&loaded.root_key) {
            return index;
        }

        let root_key = loaded.root_key.clone();
        let (workspace, file_texts) = self.remap_workspace_load(loaded);
        let index = self.loaded_workspaces.len();
        self.workspace_indexes.insert(root_key, index);
        self.loaded_workspaces.push(workspace);

        self.host.raw_database_mut().enable_proc_attr_macros();
        self.apply_base_inputs(file_texts);
        index
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
                    source_root_config: loaded.loaded.source_root_config,
                    crate_graph: loaded.loaded.crate_graph,
                    proc_macro_paths: loaded.loaded.proc_macro_paths,
                    proc_macros: loaded.loaded.proc_macros,
                    proc_macros_loaded: loaded.proc_macros_loaded,
                    build_data_loaded: loaded.build_data_loaded,
                },
                _vfs: Arc::new(files),
                line_endings,
                source_root_parent_map,
                proc_macro_client: loaded.proc_macro_client,
            },
            file_texts,
        )
    }

    fn preserve_workspace_files(
        &self,
        workspace: &mut LoadedWorkspace,
        old_files: &LoadedWorkspaceFiles,
        old_line_endings: &BTreeMap<FileId, crate::line_index::LineEndings>,
    ) {
        let files = Arc::make_mut(&mut workspace._vfs);
        for (file_id, path) in old_files.iter() {
            if files.file_id(path).is_none() {
                files.insert(file_id, path.clone(), old_files.exists(file_id));
            }
        }

        let line_endings = Arc::make_mut(&mut workspace.line_endings);
        for (&file_id, endings) in old_line_endings {
            line_endings.entry(file_id).or_insert_with(|| *endings);
        }

        workspace.input.source_roots = source_roots_for_files(
            &workspace.input.source_root_config,
            files
                .iter()
                .filter(|(file_id, _)| files.exists(*file_id))
                .map(|(file_id, path)| (file_id, path.clone())),
        );
        workspace.summary.files = files.files_by_id.len();
    }

    fn apply_base_inputs(&mut self, file_texts: Vec<(FileId, String)>) {
        let (source_roots, change) = self.base_input_change(file_texts);
        self.apply_source_roots(source_roots);
        self.host.apply_change(change);
        self.refresh_base_inputs();
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

    fn apply_base_file_changes(
        &mut self,
        workspaces: &[usize],
        files: Vec<SharedBaseFileChange>,
    ) -> anyhow::Result<bool> {
        let revision = self.host.raw_database().nonce_and_revision().1;
        let mut change = ChangeWithProcMacros::default();
        let mut applied = false;
        let mut line_endings_changed = false;
        let mut source_roots_changed = false;
        let mut affected_workspaces = Vec::new();

        for file in files {
            let path = &file.path;
            let normalized_path = normalize_vfs_path(path);
            let file_key = path_key(&normalized_path);
            let file_id = self
                .loaded_workspaces
                .iter()
                .find_map(|workspace| workspace._vfs.file_id(&normalized_path))
                .map(|(file_id, _)| file_id);
            let mut workspace_indexes = self
                .loaded_workspaces
                .iter()
                .enumerate()
                .filter_map(|(index, workspace)| {
                    let classified =
                        source_root_for_path(&workspace.input.source_root_config, path).is_some()
                            || source_root_for_path(
                                &workspace.input.source_root_config,
                                &normalized_path,
                            )
                            .is_some();
                    let workspace_root = path_key(&VfsPath::from(
                        workspace.workspace.workspace_root().to_path_buf(),
                    ));
                    let in_view_root = workspaces.contains(&index)
                        && common_path_prefix_len(&file_key, &workspace_root)
                            == workspace_root.len();
                    (workspace._vfs.file_id(&normalized_path).is_some()
                        || classified
                        || in_view_root)
                        .then_some(index)
                })
                .collect::<Vec<_>>();
            if workspace_indexes.is_empty() {
                continue;
            }
            let file_id = file_id.unwrap_or_else(|| self.base_file_ids.resolve(&normalized_path));

            for index in workspace_indexes.drain(..) {
                let workspace = &mut self.loaded_workspaces[index];
                let files = Arc::make_mut(&mut workspace._vfs);
                if !files.contains_file(file_id) {
                    files.insert(file_id, path.clone(), file.exists);
                } else {
                    files.set_exists(file_id, file.exists);
                }
                if let Some(line_endings) = file.line_endings {
                    line_endings_changed |= Arc::make_mut(&mut workspace.line_endings)
                        .insert(file_id, line_endings)
                        != Some(line_endings);
                }
                if !affected_workspaces.contains(&index) {
                    affected_workspaces.push(index);
                }
            }
            change.change_file(file_id, file.text);
            applied = true;
        }

        for index in affected_workspaces {
            let workspace = &mut self.loaded_workspaces[index];
            let roots = source_roots_for_files(
                &workspace.input.source_root_config,
                workspace
                    ._vfs
                    .iter()
                    .filter(|(file_id, _)| workspace._vfs.exists(*file_id))
                    .map(|(file_id, path)| (file_id, path.clone())),
            );
            if workspace.input.source_roots != roots {
                workspace.input.source_roots = roots;
                workspace.source_root_parent_map =
                    workspace.input.source_root_config.source_root_parent_map();
                source_roots_changed = true;
            }
        }

        if source_roots_changed {
            let roots = self
                .loaded_workspaces
                .iter()
                .flat_map(|workspace| workspace.input.source_roots.iter().cloned())
                .collect::<Vec<_>>();
            self.apply_source_roots(roots.clone());
            change.set_roots(roots);
            applied = true;
        }
        if applied {
            self.host.apply_change(change);
        }
        let changed =
            line_endings_changed || self.host.raw_database().nonce_and_revision().1 != revision;
        if changed {
            if self
                .session_overlays
                .values()
                .any(|overlay| !overlay.files_by_path.is_empty())
            {
                self.recone_session_overlays()?;
            }
            self.input_generation.fetch_add(1, Ordering::SeqCst);
        }
        Ok(changed)
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
            LibraryRoots::get(db)
                .set_roots(db)
                .to(library_roots.clone());
            self.applied_library_roots = library_roots;
        }
    }

    fn register_session(
        &mut self,
        workspace_indexes: &[usize],
    ) -> (
        u64,
        Arc<AtomicU64>,
        Arc<SharedWorldAccess>,
        crossbeam_channel::Receiver<()>,
        Arc<AtomicBool>,
    ) {
        let id = self.next_session_id;
        self.next_session_id += 1;
        let (sender, receiver) = crossbeam_channel::bounded(1);
        let pending = Arc::new(AtomicBool::new(false));
        self.session_overlays
            .insert(id, ActiveSessionOverlay::default());
        self.workspace_updates.insert(
            id,
            WorkspaceUpdate {
                workspace_indexes: workspace_indexes.to_vec(),
                pending: Arc::clone(&pending),
                sender,
            },
        );
        (
            id,
            Arc::clone(&self.input_generation),
            self.access(),
            receiver,
            pending,
        )
    }

    fn notify_workspace_updates(&self, session_id: u64, workspace_indexes: &[usize]) {
        for (&id, update) in &self.workspace_updates {
            if id != session_id
                && update
                    .workspace_indexes
                    .iter()
                    .any(|index| workspace_indexes.contains(index))
                && !update.pending.swap(true, Ordering::SeqCst)
            {
                _ = update.sender.try_send(());
            }
        }
    }

    fn unregister_session(&mut self, session_id: u64) -> bool {
        self.workspace_updates.remove(&session_id);
        let old_files = self
            .session_overlays
            .remove(&session_id)
            .into_iter()
            .flat_map(|overlay| overlay.file_ids().collect::<Vec<_>>())
            .collect::<Vec<_>>();
        if old_files.is_empty() {
            return false;
        }
        if let Err(error) = self.rebuild_overlay_inputs(old_files) {
            tracing::error!("failed to unregister shared analyzer session {session_id}: {error}");
            return false;
        }
        true
    }

    fn workspace_summaries(&self, view: &WorkspaceView) -> Vec<WorkspaceSummary> {
        view.workspace_indexes()
            .filter_map(|index| self.loaded_workspaces.get(index))
            .map(|workspace| workspace.summary().clone())
            .collect()
    }

    fn priming_scope(
        &self,
        session_id: u64,
        workspaces: &[usize],
        excluded_paths: &[String],
        state: &crate::global_state::GlobalState,
    ) -> triomphe::Arc<[ide::Crate]> {
        let db = self.host.raw_database();
        let visible_roots =
            self.session_visible_crate_roots(session_id, workspaces, excluded_paths);
        let mappings = self.session_file_mappings(session_id, workspaces);
        let root_to_crates: FxHashMap<AbsPathBuf, Vec<ide::Crate>> = {
            let mut root_to_crates: FxHashMap<AbsPathBuf, Vec<ide::Crate>> =
                FxHashMap::default();
            for &krate in &*all_crates(db) {
                let root_file = krate.data(db).root_file_id;
                if !visible_roots.contains(&root_file) {
                    continue;
                }
                let Some(path) = mappings
                    .path(root_file)
                    .and_then(|path| path.as_path().map(ToOwned::to_owned))
                else {
                    continue;
                };
                root_to_crates
                    .entry(path)
                    .or_default()
                    .push(krate);
            }
            root_to_crates
        };

        let mut seed: FxHashSet<ide::Crate> = FxHashSet::default();
        state.extend_priming_scope(&root_to_crates, &mut seed);

        crate::priming_scope::compute(db, seed)
    }

    fn loaded_workspaces_in<'a>(
        &'a self,
        workspaces: &'a [usize],
    ) -> impl Iterator<Item = &'a LoadedWorkspace> + 'a {
        workspaces
            .iter()
            .filter_map(|&index| self.loaded_workspaces.get(index))
    }

    fn session_line_endings(
        &self,
        session_id: u64,
        workspaces: &[usize],
    ) -> SharedLineEndings {
        let workspaces = self
            .loaded_workspaces_in(workspaces)
            .map(|workspace| {
                (
                    Arc::clone(&workspace._vfs),
                    Arc::clone(&workspace.line_endings),
                )
            })
            .collect();
        let mut overlay_line_endings = BTreeMap::new();

        if let Some(session_overlay) = self.session_overlays.get(&session_id) {
            overlay_line_endings.extend(
                session_overlay
                    .files_by_path
                    .values()
                    .map(|file| (file.overlay_file, file.line_endings)),
            );
        }

        SharedLineEndings {
            workspaces,
            overlay: overlay_line_endings,
        }
    }

    fn session_file_mappings(&self, session_id: u64, workspaces: &[usize]) -> SharedFileMappings {
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
        let source_path = path;
        let normalized_path = normalize_vfs_path(path);
        if let Some(file_id) = self
            .base_file(workspaces, &normalized_path)
            .filter(|&file_id| self.base_file_exists(workspaces, file_id))
        {
            let db = self.host.raw_database();
            let source_root_id = db.file_source_root(file_id).source_root_id(db);
            let source_root = db.source_root(source_root_id).source_root(db);
            return Some((source_root_id, source_root.is_library));
        }

        let path = path_key(&normalized_path);
        let mut best = None::<(usize, SourceRootId, bool)>;
        for workspace in self.loaded_workspaces_in(workspaces) {
            let Some(is_library) = source_root_for_path(
                &workspace.input.source_root_config,
                source_path,
            )
            .or_else(|| {
                source_root_for_path(&workspace.input.source_root_config, &normalized_path)
            }) else {
                continue;
            };
            for (file_id, file_path) in workspace._vfs.iter() {
                if !workspace._vfs.exists(file_id) {
                    continue;
                }
                let db = self.host.raw_database();
                let source_root_id = db.file_source_root(file_id).source_root_id(db);
                let source_root = db.source_root(source_root_id).source_root(db);
                if source_root.is_library != is_library {
                    continue;
                }
                let len = common_path_prefix_len(&path, &path_key(file_path));
                if len == 0 {
                    continue;
                }

                let replace = match best {
                    Some((best_len, _, _)) => len > best_len,
                    None => true,
                };
                if replace {
                    best = Some((len, source_root_id, source_root.is_library));
                }
            }
        }

        best.map(|(_, source_root_id, is_library)| (source_root_id, is_library))
    }

    fn can_update_session_overlay(
        &self,
        session_id: u64,
        workspaces: &[usize],
        files: &[(VfsPath, String, crate::line_index::LineEndings)],
    ) -> bool {
        let Some(overlay) = self.session_overlays.get(&session_id) else {
            return false;
        };
        overlay.workspaces == workspaces
            && overlay.open_files.len() == files.len()
            && !overlay.open_files.is_empty()
            && files.iter().all(|(path, _, _)| {
                overlay
                    .open_files
                    .contains_key(&path_key(&normalize_vfs_path(path)))
            })
    }

    fn update_session_overlay(
        &mut self,
        session_id: u64,
        files: Vec<(VfsPath, String, crate::line_index::LineEndings)>,
    ) -> SharedOverlaySync {
        let overlay = self
            .session_overlays
            .get_mut(&session_id)
            .expect("shared analyzer session overlay must exist");
        let mut change = ChangeWithProcMacros::default();
        let mut changed = false;
        for (path, text, line_endings) in files {
            let key = path_key(&normalize_vfs_path(&path));
            let open = overlay
                .open_files
                .get_mut(&key)
                .expect("shared analyzer open overlay file must exist");
            let file = overlay
                .files_by_path
                .get_mut(&key)
                .expect("shared analyzer overlay file must exist");
            if file.text != text {
                open.text.clone_from(&text);
                file.text = text.clone();
                change.change_file(file.overlay_file, Some(text));
                changed = true;
            }
            if file.line_endings != line_endings {
                file.line_endings = line_endings;
                changed = true;
            }
        }
        if changed {
            self.host.apply_change(change);
        }
        SharedOverlaySync {
            changed,
            removed_files: Vec::new(),
        }
    }

    fn session_overlay_changed(
        &self,
        session_id: u64,
        workspaces: &[usize],
        files: &[PreparedOverlayFile],
    ) -> bool {
        let db = self.host.raw_database();
        let overlay_needed = files.iter().filter(|file| file.is_open).any(|file| {
            file.base_file.is_none_or(|base_file| {
                db.file_text(base_file).text(db).as_ref() != file.text.as_str()
                    || self.base_line_endings(workspaces, base_file) != Some(file.line_endings)
            })
        });
        let old_overlay = self.session_overlays.get(&session_id);
        if !overlay_needed {
            return old_overlay.is_some_and(|overlay| !overlay.files_by_path.is_empty());
        }

        let expected_open_files = files.iter().filter(|file| file.is_open).count();
        let Some(old_overlay) = old_overlay else {
            return true;
        };
        if old_overlay.files_by_path.len() != files.len()
            || old_overlay.open_files.len() != expected_open_files
        {
            return true;
        }

        files.iter().any(|file| {
            let key = path_key(&file.path);
            let active_changed = old_overlay.files_by_path.get(&key).is_none_or(|active| {
                active.text != file.text
                    || active.line_endings != file.line_endings
                    || active.path != file.path
                    || active.display_path != file.display_path
                    || active.base_source_root != file.base_source_root
            });
            let open_changed = if file.is_open {
                old_overlay
                    .open_files
                    .get(&key)
                    .is_none_or(|open| open.text != file.text)
            } else {
                old_overlay.open_files.contains_key(&key)
            };
            active_changed || open_changed
        })
    }

    fn sync_session_overlay(
        &mut self,
        session_id: u64,
        workspaces: &[usize],
        files: Vec<PreparedOverlayFile>,
        force_rebuild: bool,
    ) -> anyhow::Result<SharedOverlaySync> {
        if !force_rebuild && !self.session_overlay_changed(session_id, workspaces, &files) {
            return Ok(SharedOverlaySync::default());
        }

        let db = self.host.raw_database();
        let overlay_needed = files.iter().filter(|file| file.is_open).any(|file| {
            file.base_file.is_none_or(|base_file| {
                db.file_text(base_file).text(db).as_ref() != file.text.as_str()
                    || self.base_line_endings(workspaces, base_file) != Some(file.line_endings)
            })
        });
        let files = if overlay_needed {
            files
                .into_iter()
                .map(|file| (path_key(&file.path), file))
                .collect::<BTreeMap<_, _>>()
        } else {
            BTreeMap::new()
        };

        let old_overlay = self
            .session_overlays
            .remove(&session_id)
            .unwrap_or_default();
        let kept_keys = files.keys().cloned().collect::<BTreeSet<_>>();
        let removed_file_ids = old_overlay
            .files_by_path
            .iter()
            .filter_map(|(key, file)| (!kept_keys.contains(key)).then_some(file.overlay_file))
            .collect::<Vec<_>>();
        let mut overlay = ActiveSessionOverlay {
            workspaces: workspaces.to_vec(),
            ..ActiveSessionOverlay::default()
        };

        for (key, file) in files {
            let old_file = old_overlay.files_by_path.get(&key);
            let overlay_file = old_file
                .map(|file| file.overlay_file)
                .unwrap_or_else(|| self.allocate_overlay_file_id());
            if old_file.is_some_and(|old| old.text != file.text) {
                self.applied_overlay_files.remove(&overlay_file);
            }
            if file.is_open {
                overlay.open_files.insert(
                    key.clone(),
                    OpenOverlayFile {
                        text: file.text.clone(),
                    },
                );
            }
            overlay.path_by_file.insert(overlay_file, key.clone());
            overlay.files_by_path.insert(
                key,
                ActiveOverlayFile {
                    overlay_file,
                    base_source_root: file.base_source_root,
                    path: file.path,
                    display_path: file.display_path,
                    text: file.text,
                    line_endings: file.line_endings,
                },
            );
        }
        self.populate_overlay_crates(&mut overlay)?;

        self.session_overlays.insert(session_id, overlay);
        self.rebuild_overlay_inputs(removed_file_ids.clone())?;

        Ok(SharedOverlaySync {
            changed: true,
            removed_files: removed_file_ids,
        })
    }

    fn prepare_session_overlay_files(
        &self,
        workspaces: &[usize],
        files: Vec<(VfsPath, String, crate::line_index::LineEndings)>,
    ) -> anyhow::Result<Vec<PreparedOverlayFile>> {
        let open_files = files
            .into_iter()
            .map(|(path, text, line_endings)| {
                let source_path = normalize_vfs_path(&path);
                (path_key(&source_path), (path, text, line_endings))
            })
            .collect::<BTreeMap<_, _>>();
        let mut required_files =
            BTreeMap::<String, (Option<FileId>, SourceRootId, VfsPath)>::new();
        let db = self.host.raw_database();
        let view_workspaces = self.loaded_workspaces_in(workspaces).collect::<Vec<_>>();

        for (path, _, _) in open_files.values() {
            let source_path = normalize_vfs_path(path);
            let base_file = self
                .base_file(workspaces, &source_path)
                .filter(|&file_id| self.base_file_exists(workspaces, file_id));
            let base_source_root = match base_file {
                Some(base_file) => self.source_root_for_file(base_file)?,
                None => {
                    let Some((source_root, _)) =
                        self.source_root_for_path(workspaces, &source_path)
                    else {
                        continue;
                    };
                    source_root
                }
            };
            required_files.entry(path_key(&source_path)).or_insert((
                base_file,
                base_source_root,
                source_path,
            ));

            let seed_crates = self.base_crates.iter().copied().filter(|krate| {
                self.source_root_for_file(krate.data(db).root_file_id).ok()
                    == Some(base_source_root)
            });
            for krate in seed_crates {
                for krate in krate.transitive_rev_deps(db) {
                    let root_file = krate.data(db).root_file_id;
                    if !view_workspaces
                        .iter()
                        .any(|workspace| workspace._vfs.contains_file(root_file))
                    {
                        continue;
                    }
                    let source_root_id = self.source_root_for_file(root_file)?;
                    let source_root = db.source_root(source_root_id).source_root(db);

                    for file_id in source_root.iter() {
                        let Some(path) = source_root.path_for_file(&file_id).cloned() else {
                            continue;
                        };
                        required_files.entry(path_key(&path)).or_insert((
                            Some(file_id),
                            source_root_id,
                            path,
                        ));
                    }
                }
            }
        }

        let mut prepared = Vec::new();
        for (key, (base_file, base_source_root, path)) in required_files {
            if let Some((display_path, text, line_endings)) = open_files.get(&key) {
                prepared.push(PreparedOverlayFile {
                    base_file,
                    base_source_root,
                    path,
                    display_path: display_path.clone(),
                    text: text.clone(),
                    line_endings: *line_endings,
                    is_open: true,
                });
                continue;
            }

            let Some(base_file) = base_file else {
                continue;
            };
            let text = db.file_text(base_file).text(db).to_string();
            let line_endings = self
                .base_line_endings(workspaces, base_file)
                .unwrap_or_else(|| {
                    let (_, line_endings) = crate::line_index::LineEndings::normalize(text.clone());
                    line_endings
                });
            prepared.push(PreparedOverlayFile {
                base_file: Some(base_file),
                base_source_root,
                path: path.clone(),
                display_path: path,
                text,
                line_endings,
                is_open: false,
            });
        }

        Ok(prepared)
    }

    fn populate_overlay_crates(
        &self,
        overlay: &mut ActiveSessionOverlay,
    ) -> anyhow::Result<()> {
        let db = self.host.raw_database();

        for &krate in &self.base_crates {
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

    fn recone_session_overlays(&mut self) -> anyhow::Result<()> {
        let overlays = self
            .session_overlays
            .iter()
            .map(|(&session_id, overlay)| {
                let files = overlay
                    .open_files
                    .iter()
                    .filter_map(|(key, open)| {
                        let file = overlay.files_by_path.get(key)?;
                        Some((
                            file.display_path.clone(),
                            open.text.clone(),
                            file.line_endings,
                        ))
                    })
                    .collect::<Vec<_>>();
                (session_id, overlay.workspaces.clone(), files)
            })
            .collect::<Vec<_>>();

        for (session_id, workspaces, files) in overlays {
            let files = self.prepare_session_overlay_files(&workspaces, files)?;
            self.sync_session_overlay(session_id, &workspaces, files, true)?;
        }

        Ok(())
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
                    graph
                        .add_dep(
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
                    graph
                        .add_dep(
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

    fn session_visible_crate_roots(
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
        let view_workspaces = self.loaded_workspaces_in(workspaces).collect::<Vec<_>>();
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

    fn base_file(&self, workspaces: &[usize], path: &VfsPath) -> Option<FileId> {
        self.loaded_workspaces_in(workspaces)
            .find_map(|workspace| workspace._vfs.file_id(path).map(|(file_id, _)| file_id))
    }

    fn base_file_exists(&self, workspaces: &[usize], file_id: FileId) -> bool {
        self.loaded_workspaces_in(workspaces)
            .any(|workspace| workspace._vfs.exists(file_id))
    }

    fn base_line_endings(
        &self,
        workspaces: &[usize],
        file_id: FileId,
    ) -> Option<crate::line_index::LineEndings> {
        self.loaded_workspaces_in(workspaces).find_map(|workspace| {
            workspace
                ._vfs
                .exists(file_id)
                .then(|| workspace.line_endings.get(&file_id).copied())
                .flatten()
        })
    }

    fn source_root_for_file(&self, file_id: FileId) -> anyhow::Result<SourceRootId> {
        Ok(self
            .host
            .raw_database()
            .file_source_root(file_id)
            .source_root_id(self.host.raw_database()))
    }

    fn active_overlay_sessions(&self) -> usize {
        self.session_overlays.len()
    }

    fn overlay_files(&self) -> usize {
        self.session_overlays
            .values()
            .flat_map(ActiveSessionOverlay::file_ids)
            .count()
    }

}

#[derive(Clone, Debug)]
struct WorkspaceView {
    workspaces: Vec<usize>,
    excluded_paths: Vec<String>,
}

impl WorkspaceView {
    fn new(workspaces: Vec<usize>, excluded_paths: Vec<String>) -> Self {
        Self {
            workspaces,
            excluded_paths,
        }
    }

    fn workspace_indexes(&self) -> impl Iterator<Item = usize> + '_ {
        self.workspaces.iter().copied()
    }

    fn excluded_paths(&self) -> &[String] {
        &self.excluded_paths
    }
}
