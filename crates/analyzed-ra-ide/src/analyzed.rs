use std::sync::Arc;

use ide_db::{FileId, RootDatabase, base_db::Crate, base_db::all_crates};
use rustc_hash::FxHashSet;

use crate::{Analysis, AnalysisHost};

impl AnalysisHost {
    pub fn analyzed_analysis_with_visible_files(
        &self,
        visible_files: Arc<FxHashSet<FileId>>,
    ) -> Analysis {
        Analysis { db: self.db.clone().analyzed_with_visible_files(visible_files) }
    }
}

pub(crate) fn visible_crates_for_graph(db: &RootDatabase) -> Vec<Crate> {
    db.analyzed_visible_base_crates(all_crates(db).iter().copied())
}
