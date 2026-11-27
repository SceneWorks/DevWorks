use super::{WatchService, finish_in, max_lifetime_default, outcome};
use crate::context::{InvocationContext, PrincipalKind};
use crate::ports::storage::{
    AssignmentPurpose, AssignmentState, ConfigurationRead, RunRead, StorageError, StoredAssignment,
    StoredWatchAssignment, StoredWatcher, WriteTransaction,
};
use crate::ports::storage::{ReadOptions, WriteOptions, read_snapshot, run_write};
use crate::protocol::{AssignmentId, DomainErrorCode, Id};
use crate::services::agents::source_assignment;
use crate::services::kernel::{Failure, Refusal};

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

fn expired(
    watcher: &StoredWatcher,
    settings: &dyn ConfigurationRead,
    now: i64,
) -> Result<bool, StorageError> {
    let deadline = match watcher.expires_at_ms {
        Some(deadline) => deadline,
        // The first published assignment watcher did not store a deadline.
        None => watcher
            .created_at_ms
            .saturating_add(max_lifetime_default(settings, watcher.project_id.as_ref())?),
    };
    Ok(now >= deadline)
}

impl WatchService {
    fn assignment_observation(
        &self,
        watcher: &StoredWatcher,
        source: &StoredWatchAssignment,
    ) -> Result<(Option<StoredAssignment>, bool), StorageError> {
        read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            Ok((
                child_record(read.runs(), source)?,
                expired(watcher, read.configuration(), self.clock.now_ms().get())?,
            ))
        })
    }

    pub(super) fn advance_assignment(&self, watcher: &StoredWatcher) -> Result<bool, StorageError> {
        let Some(source) = &watcher.assignment else {
            return Ok(false);
        };
        let (child, expired) = self.assignment_observation(watcher, source)?;
        if !expired && child.is_none_or(|a| a.state.is_live()) {
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
        let (child, expired) = self.assignment_observation(watcher, source)?;
        if expired || child.as_ref().is_some_and(|child| !child.state.is_live()) {
            self.settle_assignment(&watcher.id)?;
            return Ok(None);
        }
        Ok(child
            .is_none()
            .then_some("the child assignment's state cannot be established"))
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
            let child = child_record(tx.runs(), source)?;
            let now = tx.now_ms();
            let (kind, detail) = match child.filter(|child| !child.state.is_live()) {
                Some(child) => {
                    let kind = match child.state {
                        AssignmentState::Completed => "Succeeded",
                        AssignmentState::Failed => "Failed",
                        AssignmentState::Superseded => "Cancelled",
                        _ => return Ok(false),
                    };
                    (kind, format!("Child assignment {}. Evidence: agents.inspect for assignment {} at revision {}.",
                        child.state.as_str(), child.id.0.as_str(), child.revision))
                }
                None if expired(&watcher, tx.configuration(), now)? => (
                    "TimedOut", "the child assignment was unresolved when its watcher reached its maximum lifetime".to_owned()),
                None => return Ok(false),
            };
            let duration = Some(now.saturating_sub(watcher.created_at_ms).max(0));
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
