use std::time::Duration;

use ide::{Cancellable, FileId};
use ide_db::base_db::{AnchoredPathBuf, Crate};
use lsp_types::Url;
use vfs::VfsPath;

use crate::{
    global_state::{GlobalState, GlobalStateSnapshot},
    line_index::LineIndex,
    lsp::to_proto::url_from_abs_path,
    target_spec::TargetSpec,
};

impl GlobalState {
    pub(crate) fn new(
        sender: crossbeam_channel::Sender<lsp_server::Message>,
        config: crate::config::Config,
    ) -> Self {
        let registry = crate::analyzed_bridge::shared_analyzer_registry();
        let provider = crate::analyzed_bridge::SharedAnalyzerProvider::new(
            move |key, config, reload_path| registry.register(key, config, reload_path),
        );
        let (key, shared_config) = crate::analyzed_bridge::shared_analyzer_context_from_config(&config)
            .expect("global state config must describe a shared analyzer context");
        let session = provider
            .resolve(key, shared_config)
            .expect("shared analyzer context must resolve");
        Self::new_with_shared(sender, config, provider, session.runtime(), Vec::new())
    }

    pub(crate) fn process_changes(&mut self) -> (bool, Option<Duration>) {
        let _p = tracing::span!(tracing::Level::INFO, "GlobalState::process_changes").entered();
        self.process_shared_changes()
    }
}

impl GlobalStateSnapshot {
    pub(crate) fn url_to_file_id(&self, url: &Url) -> anyhow::Result<Option<FileId>> {
        self.shared.url_to_file_id(url)
    }

    pub(crate) fn file_id_to_url(&self, id: FileId) -> Url {
        self.shared.file_id_to_url(id).expect("shared analyzer file id must have a url")
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
        let endings = self.shared.line_endings(id).expect("shared line endings");
        let index = self.analysis.file_line_index(id)?;
        let encoding = self.config.caps().negotiated_encoding();
        Ok(LineIndex { index, endings, encoding })
    }

    pub(crate) fn file_version(&self, id: FileId) -> Option<i32> {
        let path = self.file_id_to_file_path(id);
        Some(self.mem_docs.get(&path)?.version)
    }

    pub(crate) fn anchored_path(&self, anchored: &AnchoredPathBuf) -> Url {
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
