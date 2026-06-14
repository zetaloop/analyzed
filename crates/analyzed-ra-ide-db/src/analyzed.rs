use std::sync::Arc;

use base_db::Crate as BaseCrate;
use hir::{Crate as HirCrate, symbols::FileSymbol};
use rustc_hash::FxHashSet;
use vfs::FileId;

use crate::RootDatabase;

impl RootDatabase {
    pub fn analyzed_with_visible_files(
        mut self,
        visible_files: Arc<FxHashSet<FileId>>,
    ) -> RootDatabase {
        self.analyzed_visible_files = Some(visible_files);
        self
    }

    pub fn analyzed_is_file_visible(&self, file_id: FileId) -> bool {
        self.analyzed_visible_files
            .as_ref()
            .is_none_or(|visible_files| visible_files.contains(&file_id))
    }

    pub fn analyzed_is_crate_visible(&self, krate: BaseCrate) -> bool {
        self.analyzed_is_file_visible(krate.data(self).root_file_id)
    }

    pub fn analyzed_is_hir_crate_visible(&self, krate: HirCrate) -> bool {
        self.analyzed_is_file_visible(krate.root_file(self))
    }

    pub fn analyzed_visible_base_crates(
        &self,
        crates: impl IntoIterator<Item = BaseCrate>,
    ) -> Vec<BaseCrate> {
        crates.into_iter().filter(|&krate| self.analyzed_is_crate_visible(krate)).collect()
    }

    pub fn analyzed_visible_hir_crates(
        &self,
        crates: impl IntoIterator<Item = HirCrate>,
    ) -> Vec<HirCrate> {
        crates.into_iter().filter(|&krate| self.analyzed_is_hir_crate_visible(krate)).collect()
    }
}

pub(crate) fn is_symbol_visible(db: &RootDatabase, symbol: &FileSymbol<'_>) -> bool {
    let file_id = symbol.loc.hir_file_id.original_file(db).file_id(db);
    db.analyzed_is_file_visible(file_id)
}
