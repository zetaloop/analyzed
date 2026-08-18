use std::{sync::Arc as StdArc, time::Duration};

use ide::{Cancellable, FileId};
use ide_db::base_db::{AnchoredPathBuf, Crate};
use lsp_types::Uri;
use vfs::VfsPath;

use crate::{
    global_state::{GlobalState, GlobalStateSnapshot},
    line_index::LineIndex,
    lsp::to_proto::url_from_abs_path,
    target_spec::TargetSpec,
};

pub(crate) struct PendingGlobalStateSnapshot {
    pub(crate) analysis: crate::shared_analyzer::SharedAnalyzerPendingAnalysis,
    snapshot: std::panic::AssertUnwindSafe<
        StdArc<dyn Fn(ide::Analysis) -> GlobalStateSnapshot + Send + Sync>,
    >,
}

impl Clone for PendingGlobalStateSnapshot {
    fn clone(&self) -> Self {
        Self {
            analysis: self.analysis.clone(),
            snapshot: std::panic::AssertUnwindSafe(StdArc::clone(&self.snapshot.0)),
        }
    }
}

impl PendingGlobalStateSnapshot {
    pub(crate) fn activate(&self) -> GlobalStateSnapshot {
        (self.snapshot.0)(self.analysis.activate())
    }
}

#[derive(Clone)]
pub(crate) struct SnapshotReplay {
    snapshot: StdArc<dyn Fn() -> GlobalStateSnapshot + Send + Sync>,
    token: crate::shared_analyzer::SharedAnalyzerSnapshotToken,
}

struct ActiveSession<'a>(&'a mut GlobalState);

impl ActiveSession<'_> {
    fn run(
        &mut self,
        inbox: crossbeam_channel::Receiver<lsp_server::Message>,
    ) -> anyhow::Result<()> {
        self.0.run_loop(inbox)
    }
}

impl Drop for ActiveSession<'_> {
    fn drop(&mut self) {
        self.0.shared.retire();
        self.0
            .shared
            .cancel_operations("shared analyzer session exited");
    }
}

impl SnapshotReplay {
    pub(crate) fn replayable(&self) -> bool {
        self.token.replayable()
    }

    pub(crate) fn next(&self) -> Option<GlobalStateSnapshot> {
        if !self.token.active() || !self.token.replayable() {
            return None;
        }
        let snapshot = (self.snapshot)();
        let token = snapshot.shared.snapshot_token(&snapshot.analysis);
        self.token.can_replay(&token).then_some(snapshot)
    }
}

impl GlobalState {
    pub(crate) fn run(
        mut self,
        inbox: crossbeam_channel::Receiver<lsp_server::Message>,
    ) -> anyhow::Result<()> {
        ActiveSession(&mut self).run(inbox)
    }

    pub(crate) fn new(
        sender: crossbeam_channel::Sender<lsp_server::Message>,
        config: crate::config::Config,
    ) -> Self {
        let (key, shared_config) =
            crate::shared_analyzer::shared_analyzer_context_from_config(&config)
                .expect("global state config must describe a shared analyzer context");
        let session = crate::shared_analyzer::shared_analyzer_registry()
            .register(key, shared_config, None, false, false, None, &|_| {})
            .expect("shared analyzer context must resolve");
        let state = Self::new_with_shared(sender, config, session.runtime(), Vec::new());
        state.listen_workspace_updates();
        state
    }

    pub(crate) fn process_changes(&mut self) -> (bool, Option<Duration>) {
        let _p = tracing::span!(tracing::Level::INFO, "GlobalState::process_changes").entered();
        self.process_shared_changes()
    }

    pub(crate) fn pending_snapshot(&self) -> PendingGlobalStateSnapshot {
        let analysis = self.shared.pending_analysis();
        let config = self.config.clone();
        let check_fixes = self.diagnostics.check_fixes.clone();
        let mem_docs = self.mem_docs.clone();
        let semantic_tokens_cache = self.semantic_tokens_cache.clone();
        let vfs = self.vfs.clone();
        let workspaces = self.workspaces.clone();
        let proc_macros_loaded = !self.config.expand_proc_macros()
            || self
                .fetch_proc_macros_queue
                .last_op_result()
                .copied()
                .unwrap_or(false);
        let flycheck = self.flycheck.clone();
        let minicore = self.minicore.clone();
        let shared = self.shared.clone();
        let snapshot = std::panic::AssertUnwindSafe(StdArc::new(move |analysis| GlobalStateSnapshot {
            config: config.clone(),
            check_fixes: check_fixes.clone(),
            analysis,
            mem_docs: mem_docs.clone(),
            semantic_tokens_cache: semantic_tokens_cache.clone(),
            vfs: vfs.clone(),
            workspaces: workspaces.clone(),
            proc_macros_loaded,
            flycheck: flycheck.clone(),
            minicore: minicore.clone(),
            shared: shared.clone(),
        }) as StdArc<dyn Fn(ide::Analysis) -> GlobalStateSnapshot + Send + Sync>);
        PendingGlobalStateSnapshot { analysis, snapshot }
    }
}

impl GlobalStateSnapshot {
    pub(crate) fn replay(&self) -> SnapshotReplay {
        let token = self.shared.snapshot_token(&self.analysis);
        let config = self.config.clone();
        let check_fixes = self.check_fixes.clone();
        let mem_docs = self.mem_docs.clone();
        let semantic_tokens_cache = self.semantic_tokens_cache.clone();
        let vfs = self.vfs.clone();
        let workspaces = self.workspaces.clone();
        let proc_macros_loaded = self.proc_macros_loaded;
        let flycheck = self.flycheck.clone();
        let minicore = self.minicore.clone();
        let shared = self.shared.clone();
        let snapshot = StdArc::new(move || {
            let shared = shared.clone();
            let analysis = shared.analysis();
            GlobalStateSnapshot {
                config: config.clone(),
                check_fixes: check_fixes.clone(),
                analysis,
                mem_docs: mem_docs.clone(),
                semantic_tokens_cache: semantic_tokens_cache.clone(),
                vfs: vfs.clone(),
                workspaces: workspaces.clone(),
                proc_macros_loaded,
                flycheck: flycheck.clone(),
                minicore: minicore.clone(),
                shared,
            }
        });
        SnapshotReplay { snapshot, token }
    }

    pub(crate) fn url_to_file_id(&self, url: &Uri) -> anyhow::Result<Option<FileId>> {
        self.shared.url_to_file_id(url)
    }

    pub(crate) fn file_id_to_url(&self, id: FileId) -> Uri {
        self.shared
            .file_id_to_url(id)
            .expect("shared analyzer file id must have a url")
    }

    pub(crate) fn vfs_path_to_file_id(&self, vfs_path: &VfsPath) -> anyhow::Result<Option<FileId>> {
        self.shared.vfs_path_to_file_id(vfs_path)
    }

    pub(crate) fn base_vfs_path_to_file_id(
        &self,
        vfs_path: &VfsPath,
    ) -> anyhow::Result<Option<FileId>> {
        self.shared.base_vfs_path_to_file_id(vfs_path)
    }

    pub(crate) fn file_line_index(&self, id: FileId) -> Cancellable<LineIndex> {
        let index = self.analysis.file_line_index(id)?;
        let Some(endings) = self.shared.line_endings(id) else {
            return Err(ide_db::base_db::salsa::Cancelled::Local);
        };
        let encoding = self.config.caps().negotiated_encoding();
        Ok(LineIndex {
            index,
            endings,
            encoding,
        })
    }

    pub(crate) fn file_version(&self, id: FileId) -> Option<i32> {
        let path = self.file_id_to_file_path(id);
        Some(self.mem_docs.get(&path)?.version)
    }

    pub(crate) fn anchored_path(&self, anchored: &AnchoredPathBuf) -> Uri {
        let mut anchor = self.file_id_to_file_path(anchored.anchor);
        anchor.pop();
        url_from_abs_path(anchor.join(&anchored.path).unwrap().as_path().unwrap())
    }

    pub(crate) fn file_id_to_file_path(&self, id: FileId) -> vfs::VfsPath {
        self.shared
            .file_id_to_vfs_path(id)
            .unwrap_or_else(|| panic!("shared analyzer file id {id:?} must have a path"))
    }

    pub(crate) fn file_exists(&self, id: FileId) -> bool {
        self.shared.file_exists(id).unwrap_or(false)
    }

    pub(crate) fn target_spec_for_file(
        &self,
        file_id: FileId,
        crate_id: Crate,
    ) -> Option<TargetSpec> {
        let path = self.file_id_to_file_path(file_id);
        let path = path.as_path()?;
        self.target_spec_from_workspaces(path, crate_id)
    }
}
