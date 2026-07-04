use std::sync::Arc;

use base_db::{Crate as BaseCrate, LocalRoots, source_root_crates};
use hir::{Crate as HirCrate, Module, symbols::FileSymbol};
use itertools::Itertools;
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

pub(crate) fn is_symbol_visible(db: &RootDatabase, symbol: &FileSymbol<'_>) -> bool {
    let file_id = symbol.loc.hir_file_id.original_file(db).file_id(db);
    db.is_file_visible(file_id)
}

pub(crate) fn all_crates(db: &RootDatabase) -> Vec<BaseCrate> {
    db.visible_base_crates(base_db::all_crates(db).iter().copied())
}

pub(crate) fn all_hir_crates(db: &RootDatabase) -> Vec<HirCrate> {
    db.visible_hir_crates(HirCrate::all(db))
}

pub(crate) fn resolve_path_to_modules(
    db: &RootDatabase,
    path_filter: &[String],
    anchor_to_crate: bool,
    case_sensitive: bool,
) -> Vec<Module> {
    let [first_segment, rest_segments @ ..] = path_filter else {
        return Vec::new();
    };

    let names_match = |actual: &str, expected: &str| {
        if case_sensitive { actual == expected } else { actual.eq_ignore_ascii_case(expected) }
    };

    let mut candidates = all_hir_crates(db)
        .into_iter()
        .filter(|krate| {
            krate
                .display_name(db)
                .is_some_and(|name| names_match(name.crate_name().as_str(), first_segment))
        })
        .map(|krate| (krate.root_module(db), krate.origin(db).is_local()))
        .collect::<Vec<_>>();

    if !anchor_to_crate {
        for &root in LocalRoots::get(db).roots(db) {
            for krate in db.visible_base_crates(source_root_crates(db, root).iter().copied()) {
                let root_module = HirCrate::from(krate).root_module(db);
                candidates.extend(root_module.children(db).filter_map(|child| {
                    let name = child.name(db)?;
                    names_match(name.as_str(), first_segment).then_some((child, true))
                }));
            }
        }
    }

    for segment in rest_segments {
        candidates = candidates
            .into_iter()
            .flat_map(|(module, local)| {
                module
                    .modules_in_scope(db, !local)
                    .into_iter()
                    .filter(|(name, _)| names_match(name.as_str(), segment))
                    .map(move |(_, module)| (module, local))
            })
            .unique()
            .collect();

        if candidates.is_empty() {
            break;
        }
    }

    candidates.into_iter().map(|(module, _)| module).collect()
}

