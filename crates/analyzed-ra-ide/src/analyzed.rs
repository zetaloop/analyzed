use std::sync::Arc;

use ide_db::FileId;
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
