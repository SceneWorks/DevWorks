//! The run-root and resume-grant rules the authority checks share with the runs service: which
//! work items are run roots, which blockers are active, and what a resume grant covers.
use crate::ports::storage::{
    BlockerFilter, HoldCause, MAX_PAGE, RunRead, StorageError, StoredBlocker, WorkRoot,
};
use crate::protocol::HoldRef;
use serde::{Deserialize, Serialize};

/// Whether a work item is a run root (a feature, or a task/issue without a feature).
pub fn is_run_root(w: &WorkRoot) -> bool {
    w.kind == "feature" || w.parent_feature_id.is_none()
}

/// Every active blocker matching `filter` (bounded pages).
pub fn active_blockers(
    runs: &dyn RunRead,
    filter: &BlockerFilter,
) -> Result<Vec<StoredBlocker>, StorageError> {
    let mut out: Vec<StoredBlocker> = Vec::new();
    loop {
        let page = runs.blockers(filter, out.last().map(|b| &b.id), MAX_PAGE)?;
        let more = page.len() == usize::from(MAX_PAGE);
        out.extend(page);
        if !more {
            return Ok(out);
        }
    }
}
/// What a user-minted resume grant covers (its `binding`): exact holds (the default delegation:
/// the identified current pause only) or, for a standing grant, the causes it may clear, which
/// never include restart or budget holds.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResumeBinding {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub holds: Vec<HoldRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causes: Vec<String>,
}
impl ResumeBinding {
    pub fn covers(&self, b: &StoredBlocker) -> bool {
        if self
            .holds
            .iter()
            .any(|h| h.hold_id.0 == b.id && h.revision.get() == b.revision)
        {
            return true;
        }
        b.cause == HoldCause::HumanPause && self.causes.iter().any(|c| c == "HumanPause")
    }
}
