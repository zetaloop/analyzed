use std::{any::Any, fmt, panic::RefUnwindSafe, sync::Arc};

use ide_db::{FileId, RootDatabase, base_db::Crate};
use rustc_hash::FxHashSet;

use crate::{Analysis, AnalysisHost};

pub(crate) struct AnalysisGuard {
    _guard: Box<dyn Any + Send + Sync + RefUnwindSafe>,
}

impl fmt::Debug for AnalysisGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AnalysisGuard")
    }
}

impl Analysis {
    pub fn with_guard(
        mut self,
        guard: impl Any + Send + Sync + RefUnwindSafe + 'static,
    ) -> Analysis {
        self.guard = Some(AnalysisGuard { _guard: Box::new(guard) });
        self
    }
}

impl AnalysisHost {
    pub fn analysis_with_visible_files(
        &self,
        visible_files: Arc<FxHashSet<FileId>>,
    ) -> Analysis {
        Analysis {
            db: self.db.clone().with_visible_files(visible_files),
            guard: None,
        }
    }
}

pub(crate) fn all_crates(db: &RootDatabase) -> Vec<Crate> {
    db.visible_base_crates(ide_db::base_db::all_crates(db).iter().copied())
}
