//! The queue reads `next_work` (the workflow item, WORK-09/10) allocates from. Nothing here
//! claims or allocates: it lists one queue in its stable order with the facts eligibility needs,
//! read inside the caller's snapshot or unit of work so an allocation can recheck them
//! atomically.
//!
//! * The **project queue** holds features and standalone issues in one priority order (priority
//!   descending, explicit rank, creation, ID); feature cards never compete in it.
//! * A **feature queue** holds that feature's cards in the same order.
//!
//! Each candidate carries its workflow status and semantic, its open prerequisites (not
//! completed with verified acceptance: an unsatisfied prerequisite holds a dependent whatever
//! its priority) and, for a feature, whether it has an active approved plan (automatic
//! selection skips unapproved plans; a pending revision never counts) and its current readiness.
//! Ownership, pauses, budgets and the claim itself stay with run control and the allocator.
use super::model::active_of;
use super::workflow::semantic_of;
use crate::ports::storage::{
    FEATURE, PROJECT_QUEUE, StorageError, StoredWork, WorkFilter, WorkPosition, WorkRead,
    WorkSelection, feature_queue,
};
use crate::protocol::records::ReadinessAssessment;
use crate::protocol::{Id, ProjectId};

/// One queued item and the facts its eligibility depends on.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub work: StoredWork,
    /// Its status semantic (`Backlog`, `Ready`, ... ; empty when the status is unknown).
    pub semantic: String,
    /// Prerequisites not completed with verified acceptance.
    pub open_prerequisites: Vec<Id>,
    /// A feature with an active approved plan (always `false` for a card).
    pub approved: bool,
    pub readiness: Option<ReadinessAssessment>,
}

/// One queue of `project`: the project queue (`feature` `None`) or a feature's card queue, in
/// queue order, with the revision it was read at (an allocation compares it).
pub fn queue(
    read: &dyn WorkRead,
    project: &ProjectId,
    feature: Option<&Id>,
) -> Result<(i64, Vec<Candidate>), StorageError> {
    let (key, selections) = match feature {
        None => (
            PROJECT_QUEUE.to_owned(),
            vec![WorkSelection::Features, WorkSelection::Standalone],
        ),
        Some(f) => (
            feature_queue(f),
            vec![WorkSelection::FeatureTasks(f.clone())],
        ),
    };
    let revision = read.queue_revision(project, &key)?;
    let statuses = read.statuses(project)?;
    let mut items: Vec<StoredWork> = Vec::new();
    for s in selections {
        let filter = WorkFilter {
            project: project.clone(),
            selection: s,
            status_ids: Vec::new(),
            repository_ids: Vec::new(),
            text: None,
        };
        let mut after: Option<WorkPosition> = None;
        loop {
            let page = read.works(&filter, after.as_ref(), 500)?;
            let more = page.len() == 500;
            after = page.last().map(WorkPosition::of);
            items.extend(page);
            if !more {
                break;
            }
        }
    }
    items.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then(a.manual_order.cmp(&b.manual_order))
            .then(a.created_at_ms.cmp(&b.created_at_ms))
            .then(a.id.as_str().cmp(b.id.as_str()))
    });
    let mut out = Vec::with_capacity(items.len());
    for w in items {
        let semantic = w
            .status_id
            .as_ref()
            .and_then(|s| semantic_of(&statuses, s))
            .unwrap_or("")
            .to_owned();
        let (approved, readiness) = if w.kind == FEATURE {
            let slot = read.feature_plan(&w.id)?;
            (
                active_of(slot.as_ref().and_then(|s| s.approved.as_ref())).is_some(),
                super::readiness::assessment(read, &w)?,
            )
        } else {
            (false, None)
        };
        out.push(Candidate {
            open_prerequisites: read.open_prerequisites(&w.id)?,
            semantic,
            approved,
            readiness,
            work: w,
        });
    }
    Ok((revision, out))
}
