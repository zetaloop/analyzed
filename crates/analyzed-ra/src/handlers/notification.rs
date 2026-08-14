use std::{
    cell::RefCell,
    ops::Deref,
    panic::{AssertUnwindSafe, UnwindSafe},
    rc::Rc,
};

use lsp_types::DidSaveTextDocumentParams;
use paths::AbsPathBuf;
use rustc_hash::FxHashSet;
use triomphe::Arc;
use vfs::{ChangeKind, VfsPath};

use crate::{
    flycheck::{FlycheckHandle, InvocationStrategy, PackageSpecifier, Target},
    global_state::{FetchWorkspaceRequest, GlobalState, GlobalStateSnapshot},
    line_index::LineEndings,
    lsp::from_proto,
    reload,
    shared_analyzer::SharedBaseFileChange,
    shared_global_state::PendingGlobalStateSnapshot,
    try_default,
};

pub(crate) fn handle_did_save_text_document(
    state: &mut GlobalState,
    params: DidSaveTextDocumentParams,
) -> anyhow::Result<()> {
    if let Ok(vfs_path) = from_proto::vfs_path(&params.text_document.uri) {
        if state.source_root_config.path_is_library(&vfs_path) {
            if let Some(path) = vfs_path.as_path() {
                state.loader.handle.invalidate(path.to_path_buf());
            }
        } else {
            let saved = state
                .mem_docs
                .get(&vfs_path)
                .and_then(|document| std::str::from_utf8(&document.data).ok())
                .map(|text| LineEndings::normalize(text.to_owned()));
            if let Some((text, line_endings)) = saved {
                state
                    .shared
                    .apply_base_file_changes(vec![SharedBaseFileChange {
                        path: vfs_path.clone(),
                        text,
                        line_endings,
                    }])?;
            }
        }

        let snap = state.snapshot();
        let file_id = try_default!(snap.vfs_path_to_file_id(&vfs_path)?);
        let sr = snap.analysis.source_root_id(file_id)?;
        drop(snap);

        if state.config.script_rebuild_on_save(Some(sr)) && state.build_deps_changed {
            state.build_deps_changed = false;
            state
                .fetch_build_data_queue
                .request_op("build_deps_changed - save notification".to_owned(), ());
        }

        // Re-fetch workspaces if a workspace related file has changed
        if let Some(path) = vfs_path.as_path() {
            let additional_files = &state
                .config
                .discover_workspace_config()
                .map(|cfg| cfg.files_to_watch.iter().map(String::as_str).collect::<Vec<&str>>())
                .unwrap_or_default();

            // FIXME: We should move this check into a QueuedTask and do semantic resolution of
            // the files. There is only so much we can tell syntactically from the path.
            if reload::should_refresh_for_change(path, ChangeKind::Modify, additional_files) {
                state.fetch_workspaces_queue.request_op(
                    format!("workspace vfs file change saved {path}"),
                    FetchWorkspaceRequest {
                        path: Some(path.to_owned()),
                        force_crate_graph_reload: false,
                    },
                );
            } else if state.detached_files.contains(path) {
                state.fetch_workspaces_queue.request_op(
                    format!("detached file saved {path}"),
                    FetchWorkspaceRequest {
                        path: Some(path.to_owned()),
                        force_crate_graph_reload: false,
                    },
                );
            }
        }

        if !state.config.check_on_save(Some(sr)) {
            return Ok(());
        }

        if run_flycheck(state, vfs_path) {
            return Ok(());
        }
    } else if state.config.check_on_save(None) && state.config.flycheck_workspace(None) {
        // No specific flycheck was triggered, so let's trigger all of them.
        state.diagnostics.clear_check_all();
        for flycheck in state.flycheck.iter() {
            flycheck.restart_workspace(None);
        }
    }

    Ok(())
}

pub(crate) fn run_flycheck(state: &mut GlobalState, vfs_path: VfsPath) -> bool {
    let _p = tracing::info_span!("run_flycheck").entered();

    let base_file_id = state.shared.base_vfs_path_to_file_id(&vfs_path);
    let file_id = state.shared.vfs_path_to_file_id(&vfs_path);
    if let (Ok(Some(_)), Ok(Some(file_id))) = (base_file_id, file_id) {
        let world = state.pending_snapshot();
        let invocation_strategy = state.config.flycheck(None).invocation_strategy();
        let may_flycheck_workspace = state.config.flycheck_workspace(None);
        let task: Box<dyn FnOnce() -> ide::Cancellable<()> + Send + UnwindSafe> =
            match invocation_strategy {
                InvocationStrategy::Once => {
                    crate::handlers::notification::run_flycheck_once(world, vfs_path.clone())
                }
                InvocationStrategy::PerWorkspace => {
                    crate::handlers::notification::run_flycheck_per_workspace(
                        world,
                        file_id,
                        vfs_path.clone(),
                        may_flycheck_workspace,
                    )
                }
            };

        state
            .task_pool
            .handle
            .spawn_with_sender(stdx::thread::ThreadIntent::Worker, move |_| {
                if let Err(e) = std::panic::catch_unwind(task) {
                    tracing::error!("flycheck task panicked: {e:?}")
                }
            });
        true
    } else {
        false
    }
}

enum FlycheckRestart {
    Workspace {
        handle: usize,
        saved_file: Option<AbsPathBuf>,
    },
    Package {
        handle: usize,
        package: PackageSpecifier,
        target: Option<Target>,
        workspace_deps: Option<FxHashSet<PackageSpecifier>>,
        saved_file: Option<AbsPathBuf>,
    },
}

struct FlycheckSelection {
    flycheck: Arc<[FlycheckHandle]>,
    restarts: Rc<RefCell<Vec<FlycheckRestart>>>,
}

pub(crate) struct FlycheckSelectionHandle {
    handle: usize,
    id: usize,
    restarts: Rc<RefCell<Vec<FlycheckRestart>>>,
}

pub(crate) struct FlycheckSelectionWorld {
    snapshot: GlobalStateSnapshot,
    pub(crate) flycheck: Vec<FlycheckSelectionHandle>,
}

pub(crate) fn activate_flycheck<F>(
    world: PendingGlobalStateSnapshot,
    f: F,
) -> impl FnOnce() -> ide::Cancellable<()> + Send + UnwindSafe + 'static
where
    F: FnOnce(GlobalStateSnapshot) -> ide::Cancellable<()> + Send + 'static,
{
    AssertUnwindSafe(move || f(world.activate()))
}

pub(crate) fn select_flycheck_per_workspace<F>(
    world: PendingGlobalStateSnapshot,
    f: F,
) -> impl FnOnce() -> ide::Cancellable<()> + Send + UnwindSafe + 'static
where
    F: Fn(FlycheckSelectionWorld) -> ide::Cancellable<()> + Send + 'static,
{
    AssertUnwindSafe(move || {
        let mut snapshot = world.activate();
        loop {
            let replay = AssertUnwindSafe(snapshot.replay());
            let (world, selection) = FlycheckSelectionWorld::new(snapshot);
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| f(world)));
            if replay.replayable()
                && let Some(next) = replay.next()
            {
                snapshot = next;
                continue;
            }
            match result {
                Ok(result) => result?,
                Err(payload) => std::panic::resume_unwind(payload),
            }
            selection.execute();
            return Ok(());
        }
    })
}

impl FlycheckSelectionWorld {
    fn new(snapshot: GlobalStateSnapshot) -> (Self, FlycheckSelection) {
        let flycheck = snapshot.flycheck_handles();
        let restarts = Rc::new(RefCell::new(Vec::new()));
        let handles = flycheck
            .iter()
            .enumerate()
            .map(|(handle, flycheck)| FlycheckSelectionHandle {
                handle,
                id: flycheck.id(),
                restarts: Rc::clone(&restarts),
            })
            .collect();
        (
            Self {
                snapshot,
                flycheck: handles,
            },
            FlycheckSelection { flycheck, restarts },
        )
    }
}

impl Deref for FlycheckSelectionWorld {
    type Target = GlobalStateSnapshot;

    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}

impl FlycheckSelectionHandle {
    pub(crate) fn id(&self) -> usize {
        self.id
    }

    pub(crate) fn restart_workspace(&self, saved_file: Option<AbsPathBuf>) {
        self.restarts.borrow_mut().push(FlycheckRestart::Workspace {
            handle: self.handle,
            saved_file,
        });
    }

    pub(crate) fn restart_for_package(
        &self,
        package: PackageSpecifier,
        target: Option<Target>,
        workspace_deps: Option<FxHashSet<PackageSpecifier>>,
        saved_file: Option<AbsPathBuf>,
    ) {
        self.restarts.borrow_mut().push(FlycheckRestart::Package {
            handle: self.handle,
            package,
            target,
            workspace_deps,
            saved_file,
        });
    }
}

impl FlycheckSelection {
    fn execute(self) {
        for restart in self.restarts.take() {
            match restart {
                FlycheckRestart::Workspace { handle, saved_file } => {
                    self.flycheck[handle].restart_workspace(saved_file);
                }
                FlycheckRestart::Package {
                    handle,
                    package,
                    target,
                    workspace_deps,
                    saved_file,
                } => {
                    self.flycheck[handle].restart_for_package(
                        package,
                        target,
                        workspace_deps,
                        saved_file,
                    );
                }
            }
        }
    }
}
