use base_db::SourceDatabase;
use hir::Crate as HirCrate;
use rustc_hash::FxHashMap;

use crate::RootDatabase;

impl super::SearchScope {
    pub(super) fn reverse_dependencies(db: &RootDatabase, of: HirCrate) -> Self {
        let mut entries = FxHashMap::default();
        for rev_dep in db.visible_hir_crates(of.transitive_reverse_dependencies(db)) {
            let root_file = rev_dep.root_file(db);

            let source_root = db.file_source_root(root_file).source_root_id(db);
            let source_root = db.source_root(source_root).source_root(db);
            entries.extend(
                source_root
                    .iter()
                    .map(|id| (super::EditionedFileId::new(db, id, rev_dep.edition(db)), None)),
            );
        }
        Self { entries }
    }
}
