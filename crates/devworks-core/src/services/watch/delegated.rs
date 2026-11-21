use super::{WatchService, finish_in, outcome};
use crate::context::{InvocationContext, PrincipalKind};
use crate::ports::storage::{
    AssignmentPurpose, AssignmentState, EventFilter, RunRead, StorageError, StoredAssignment,
    StoredWatchAssignment, StoredWatcher, WriteTransaction,
};
use crate::ports::storage::{ReadOptions, WriteOptions, read_snapshot, run_write};
use crate::protocol::{AssignmentId, DomainErrorCode, Id};
use crate::services::agents::{assignment_checkpoint, source_assignment};
use crate::services::kernel::{Failure, Refusal};
use crate::services::runs::ASSIGNMENT_AGGREGATE;

/// Bind the watched child to the currently authorized parent and to the child's exact session.
pub(super) fn bind(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    id: &AssignmentId,
) -> Result<StoredWatchAssignment, Failure> {
    let missing = || Refusal::new(DomainErrorCode::NotFound, "assignment", Some(id.0.clone()));
    let (Some(project), Some(root)) = (ctx.project_id(), ctx.root_id()) else {
        return Err(missing().into());
    };
    if ctx.principal_kind() != PrincipalKind::Agent {
        return Err(missing().into());
    }
    let parent = source_assignment(tx, ctx, project, root)?.ok_or_else(missing)?;
    let child = tx
        .runs()
        .assignment(id)?
        .filter(|a| {
            a.purpose == AssignmentPurpose::Child
                && a.parent_id.as_ref() == Some(&parent)
                && &a.root_id == root
        })
        .ok_or_else(missing)?;
    Ok(StoredWatchAssignment {
        id: child.id,
        session_id: child.session_id,
    })
}

impl WatchService {
    pub(super) fn advance_assignment(&self, watcher: &StoredWatcher) -> Result<bool, StorageError> {
        let Some(source) = &watcher.assignment else {
            return Ok(false);
        };
        let child = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            child_record(read.runs(), source)
        })?;
        if child.is_none_or(|a| a.state.is_live()) {
            return Ok(false);
        }
        self.settle_assignment(&watcher.id)
    }

    /// Read the assignment directly even if the primary pass has never run.
    pub(super) fn backup_assignment(
        &self,
        watcher: &StoredWatcher,
    ) -> Result<Option<&'static str>, StorageError> {
        let Some(source) = &watcher.assignment else {
            return Ok(None);
        };
        let child = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            child_record(read.runs(), source)
        })?;
        match child {
            Some(child) if child.state.is_live() => Ok(None),
            Some(_) => {
                self.settle_assignment(&watcher.id)?;
                Ok(None)
            }
            None => Ok(Some("the child assignment's state cannot be established")),
        }
    }

    fn settle_assignment(&self, id: &Id) -> Result<bool, StorageError> {
        let id = id.clone();
        run_write(&*self.store, WriteOptions::observed(), move |tx| {
            let Some(watcher) = tx.watchers().watcher(&id)?.filter(|w| w.phase.is_open()) else {
                return Ok(false);
            };
            let Some(source) = &watcher.assignment else {
                return Ok(false);
            };
            let Some(child) = child_record(tx.runs(), source)? else {
                return Ok(false);
            };
            let kind = match child.state {
                AssignmentState::Completed => "Succeeded",
                AssignmentState::Failed => "Failed",
                AssignmentState::Superseded => "Cancelled",
                AssignmentState::Ready | AssignmentState::Active | AssignmentState::Waiting => {
                    return Ok(false);
                }
            };
            let detail = evidence(tx, &child)?;
            let duration = Some(tx.now_ms().saturating_sub(watcher.created_at_ms).max(0));
            finish_in(
                tx,
                watcher,
                outcome(kind, None, None, Some(&detail)),
                duration,
                None,
                false,
                None,
            )?;
            Ok(true)
        })
    }
}

fn child_record(
    read: &dyn RunRead,
    source: &StoredWatchAssignment,
) -> Result<Option<StoredAssignment>, StorageError> {
    Ok(read
        .assignment(&source.id)?
        .filter(|a| a.session_id == source.session_id))
}

fn evidence(
    tx: &mut dyn WriteTransaction,
    child: &StoredAssignment,
) -> Result<String, StorageError> {
    let filter = EventFilter::Aggregate {
        kind: ASSIGNMENT_AGGREGATE.into(),
        id: child.id.0.as_str().into(),
    };
    let mut detail = format!(
        "Child assignment {} at revision {}. Evidence: history.events for root {}",
        child.state.as_str(),
        child.revision,
        child.root_id.0.as_str()
    );
    let mut after = 0;
    loop {
        let page = tx.history().page(&filter, after, 100)?;
        if let Some(event) = page
            .events
            .iter()
            .find(|event| event.aggregate_revision == child.revision)
        {
            detail.push_str(&format!(", event {}", event.id.0.as_str()));
            break;
        }
        if !page.has_more {
            break;
        }
        after = page.next_after;
    }
    detail.push('.');
    if let Some(checkpoint) = assignment_checkpoint(child)? {
        detail.push_str(&format!(
            " Child checkpoint: artifacts.read for artifact {}, version {}.",
            checkpoint.artifact_id.as_str(),
            checkpoint.version_id.as_str()
        ));
    }
    Ok(detail)
}
