use std::{fmt, panic};

use ide_db::base_db::DbPanicContext;
use lsp_server::{Response, ResponseError};
use serde::{Serialize, de::DeserializeOwned};
use stdx::thread::ThreadIntent;

use crate::{
    global_state::GlobalStateSnapshot,
    handlers::dispatch::{RequestDispatcher, thread_result_to_response},
    main_loop::Task,
};

impl RequestDispatcher<'_> {
    pub(crate) fn on_with_thread_intent<
        const RUSTFMT: bool,
        const ALLOW_RETRYING: bool,
        R,
    >(
        &mut self,
        intent: ThreadIntent,
        f: fn(GlobalStateSnapshot, R::Params) -> anyhow::Result<R::Result>,
        on_cancelled: fn() -> ResponseError,
    ) -> &mut Self
    where
        R: lsp_types::request::Request + 'static,
        R::Params: DeserializeOwned + panic::UnwindSafe + Send + fmt::Debug,
        R::Result: Serialize,
    {
        let (req, params, panic_context) = match self.parse::<R>() {
            Some(it) => it,
            None => return self,
        };
        let _guard =
            tracing::info_span!("request", method = ?req.method, "request_id" = ?req.id).entered();
        tracing::debug!(?params);

        let world = self.global_state.snapshot();
        let dispatched_edit_generation = world.analyzed_shared.edit_generation();
        let analyzed_shared = world.analyzed_shared.clone();
        if RUSTFMT {
            &mut self.global_state.fmt_pool.handle
        } else {
            &mut self.global_state.task_pool.handle
        }
        .spawn(intent, move || {
            let result = panic::catch_unwind(move || {
                let _pctx = DbPanicContext::enter(panic_context);
                f(world, params)
            });
            match thread_result_to_response::<R>(req.id.clone(), result) {
                Ok(response) => Task::Response(response),
                Err(_cancelled) if ALLOW_RETRYING => Task::Retry(req),
                Err(_cancelled)
                    if analyzed_shared.edit_generation() == dispatched_edit_generation =>
                {
                    Task::Retry(req)
                }
                Err(_cancelled) => {
                    let error = on_cancelled();
                    Task::Response(Response { id: req.id, result: None, error: Some(error) })
                }
            }
        });

        self
    }
}
