use std::{ops::ControlFlow, sync::Arc};

use base_db::{Crate as BaseCrate, SourceRootId};
use hir::{Crate as HirCrate, symbols::FileSymbol};
use rustc_hash::FxHashSet;
use vfs::FileId;

use crate::RootDatabase;

impl RootDatabase {
    pub fn with_visible_files(
        mut self,
        visible_files: Arc<FxHashSet<FileId>>,
    ) -> RootDatabase {
        self.visible_files = Some(visible_files);
        self
    }

    pub fn is_file_visible(&self, file_id: FileId) -> bool {
        self.visible_files
            .as_ref()
            .is_none_or(|visible_files| visible_files.contains(&file_id))
    }

    pub fn is_crate_visible(&self, krate: BaseCrate) -> bool {
        self.is_file_visible(krate.data(self).root_file_id)
    }

    pub fn is_hir_crate_visible(&self, krate: HirCrate) -> bool {
        self.is_file_visible(krate.root_file(self))
    }

    pub fn visible_base_crates(
        &self,
        crates: impl IntoIterator<Item = BaseCrate>,
    ) -> Vec<BaseCrate> {
        crates.into_iter().filter(|&krate| self.is_crate_visible(krate)).collect()
    }

    pub fn visible_hir_crates(
        &self,
        crates: impl IntoIterator<Item = HirCrate>,
    ) -> Vec<HirCrate> {
        crates.into_iter().filter(|&krate| self.is_hir_crate_visible(krate)).collect()
    }
}

pub(crate) trait CrateVisibility {
    fn visible_reverse_dependencies(self, db: &RootDatabase) -> Vec<HirCrate>;
}

impl CrateVisibility for HirCrate {
    fn visible_reverse_dependencies(self, db: &RootDatabase) -> Vec<HirCrate> {
        db.visible_hir_crates(self.transitive_reverse_dependencies(db))
    }
}

pub(crate) fn source_root_crates(db: &RootDatabase, root: SourceRootId) -> Vec<BaseCrate> {
    db.visible_base_crates(base_db::source_root_crates(db, root).iter().copied())
}

pub(crate) fn all_crates(db: &RootDatabase) -> Vec<BaseCrate> {
    db.visible_base_crates(base_db::all_crates(db).iter().copied())
}

pub(crate) fn all_hir_crates(db: &RootDatabase) -> Vec<HirCrate> {
    db.visible_hir_crates(HirCrate::all(db))
}

pub(crate) fn visible_symbols<'db, T>(
    db: &'db RootDatabase,
    mut callback: impl FnMut(&'db FileSymbol<'db>) -> ControlFlow<T>,
) -> impl FnMut(&'db FileSymbol<'db>) -> ControlFlow<T> {
    move |symbol| {
        if db.is_file_visible(symbol.loc.hir_file_id.original_file(db).file_id(db)) {
            callback(symbol)
        } else {
            ControlFlow::Continue(())
        }
    }
}

