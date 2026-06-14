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

#[cfg(test)]
pub(crate) fn skip_slow_tests() -> bool {
    (std::env::var("CI").is_err() && std::env::var("RUN_SLOW_TESTS").is_err())
        || std::env::var("SKIP_SLOW_TESTS").is_ok()
}
