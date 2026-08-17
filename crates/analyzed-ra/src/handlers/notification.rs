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
use vfs::VfsPath;

use crate::{
    flycheck::{FlycheckHandle, InvocationStrategy, PackageSpecifier, Target},
    global_state::{GlobalState, GlobalStateSnapshot},
    line_index::LineEndings,
    lsp::from_proto,
    shared_analyzer::SharedBaseFileChange,
    shared_global_state::PendingGlobalStateSnapshot,
};

pub(crate) fn clear_native_diagnostics_for_closed_file(
    state: &mut GlobalState,
    path: VfsPath,
) {
    if let Ok(Some(file_id)) = state.shared.vfs_path_to_file_id(&path) {
        state.diagnostics.clear_native_for(file_id);
    }
}

pub(crate) fn handle_did_save_text_document(
    state: &mut GlobalState,
    params: DidSaveTextDocumentParams,
) -> anyhow::Result<()> {
    if let Ok(vfs_path) = from_proto::vfs_path(&params.text_document.uri)
        && !state.source_root_config.path_is_library(&vfs_path)
        && let Some((text, line_endings)) = state
            .mem_docs
            .get(&vfs_path)
            .and_then(|document| std::str::from_utf8(&document.data).ok())
            .map(|text| LineEndings::normalize(text.to_owned()))
    {
        state
            .shared
            .apply_base_file_changes(vec![SharedBaseFileChange {
                path: vfs_path,
                exists: true,
                text: Some(text),
                line_endings: Some(line_endings),
            }])?;
    }
    crate::handlers::notification::_handle_did_save_text_document(state, params)
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
        let flycheck = snapshot.flycheck.clone();
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
