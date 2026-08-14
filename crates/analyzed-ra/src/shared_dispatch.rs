use crate::{
    global_state::GlobalStateSnapshot, main_loop::Task,
    shared_global_state::PendingGlobalStateSnapshot,
};

pub(crate) fn on_with_thread_intent(
    world: PendingGlobalStateSnapshot,
    request: lsp_server::Request,
    f: impl FnOnce(GlobalStateSnapshot) -> Task,
) -> impl FnOnce() -> Task {
    move || {
        let world = world.activate();
        let snapshot = world.shared.snapshot_token(&world.analysis);
        let task = f(world);
        if snapshot.replayable() {
            Task::Retry(request)
        } else {
            task
        }
    }
}
