//! Blocking links (WORK-02, WORK-05). A directed link `A → B` means **A blocks B**; one stored
//! edge yields both views (Blocked by, Blocks). A replacement changes exactly the target's
//! incoming links (an empty list removes them). Endpoints must be visible items of the same
//! project (cards across features and standalone issues may link; features link to features);
//! self links, duplicates and cycles across the whole project graph are refused, and completed
//! work keeps its prerequisites. [`set_links`] is the one writer every path uses
//! (`tasks.dependency_links`, `features.dependencies`, re-plans and inline or staged atomic
//! plans), so no path skips a rule another enforces. The cycle check reads the graph inside the
//! command's unit of work on the sole write owner, so two individually valid concurrent edits
//! cannot together form a cycle: the second one sees the first. A graph too large to search
//! within the bound is refused visibly, never assumed acyclic. A refusal writes nothing.
use super::workflow::{DONE, semantic_of};
use super::{
    R, WorkService, check_revision, conflict, event, id_set, not_found, open_project, visible_card,
    visible_feature, visible_work,
};
use crate::context::InvocationContext;
use crate::operations::{features as fops, tasks as tops};
use crate::ports::storage::{FEATURE, StoredWork, WorkRead, WriteTransaction};
use crate::protocol::records::DependencyEdge;
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::{DomainErrorCode, Id, ProjectId, RetryAdvice};
use crate::services::sessions::Refusal;
use serde_json::json;
use std::collections::BTreeSet;

/// Most prerequisites (incoming blocking links) one item has: every writer refuses more, so
/// the record's `dependencyIds` projection, bounded the same, is always the complete list a
/// replacement starts from.
pub const MAX_PREREQUISITES: usize = 128;

/// Most items a cycle search visits before it refuses the change.
pub const SEARCH_BOUND: usize = 20_000;

/// Whether `target` reaches any of `prerequisites` through existing "blocks" edges (adding
/// `p → target` then closes a cycle `target → … → p → target`). `Err` when the search bound is
/// exceeded.
pub fn closes_cycle(
    read: &dyn WorkRead,
    target: &Id,
    prerequisites: &[Id],
) -> Result<Option<Id>, crate::ports::storage::StorageError> {
    let wanted: BTreeSet<&str> = prerequisites.iter().map(Id::as_str).collect();
    let mut seen: std::collections::HashSet<Id> = std::collections::HashSet::new();
    let mut stack = vec![target.clone()];
    while let Some(n) = stack.pop() {
        for d in read.dependents(&n)? {
            if wanted.contains(d.as_str()) {
                return Ok(Some(d));
            }
            if d != *target && seen.insert(d.clone()) {
                if seen.len() > SEARCH_BOUND {
                    return Err(crate::ports::storage::StorageError::InvalidRecord(
                        "dependency graph exceeds the search bound".into(),
                    ));
                }
                stack.push(d);
            }
        }
    }
    Ok(None)
}

/// The one writer of blocking links: replaces the incoming links of `target` with
/// `prerequisites` after every rule a link change must meet — the target and each prerequisite
/// are items of `project` of the same kind (cards link to cards, features to features), no self
/// link or duplicate, and completed work keeps its prerequisites (reopen it first). A change
/// advances the target's work revision (an open form of it is then stale) and is recorded.
/// Cycles are checked by [`check_cycles`] once every link of the unit of work is written.
/// Returns the target after the change, `None` when nothing changed.
pub(crate) fn set_links(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    project: &ProjectId,
    target: &Id,
    prerequisites: &[Id],
) -> R<Option<StoredWork>> {
    let t = match tx.work().work(target)? {
        Some(w) if &w.project_id == project => w,
        _ => return Err(not_found("work", target).into()),
    };
    if prerequisites.iter().any(|p| p == target) {
        return Err(conflict("self_link", target));
    }
    let prereqs = id_set(prerequisites, "duplicate_link")?;
    if prereqs.len() > MAX_PREREQUISITES {
        return Err(conflict("too_many_prerequisites", target));
    }
    for p in &prereqs {
        match tx.work().work(p)? {
            Some(w) if &w.project_id == project && (w.kind == FEATURE) == (t.kind == FEATURE) => {}
            Some(w) if &w.project_id == project => return Err(conflict("link_kind", &w.id)),
            // Never a link into another project (nor a disclosure of one).
            _ => return Err(not_found("work", p).into()),
        }
    }
    let current = tx.work().prerequisites(&t.id)?;
    if current == prereqs {
        return Ok(None);
    }
    // Completed work is not reopened by link editing: its prerequisites stay as they were.
    let statuses = tx.work().statuses(project)?;
    if t.status_id.as_ref().and_then(|s| semantic_of(&statuses, s)) == Some(DONE) {
        return Err(conflict("work_completed", &t.id));
    }
    tx.work().set_prerequisites(&t.id, &prereqs)?;
    let mut after = t.clone();
    after.revision = t.revision + 1;
    after.updated_at_ms = tx.now_ms();
    after.updated_by = Some(ctx.principal().clone());
    tx.work().update_work(&after, t.revision)?;
    super::items::bump(tx, project, &[after.queue()])?;
    super::allocation::invalidate_work(tx, ctx, &after.id)?;
    let added: Vec<&str> = prereqs
        .iter()
        .filter(|p| !current.contains(p))
        .map(Id::as_str)
        .collect();
    let removed: Vec<&str> = current
        .iter()
        .filter(|p| !prereqs.contains(p))
        .map(Id::as_str)
        .collect();
    let open = tx.work().open_prerequisites(&t.id)?;
    event(
        tx,
        ctx,
        &after,
        "DependenciesReplaced",
        json!({
            "blockedBy": prereqs.iter().map(Id::as_str).collect::<Vec<_>>(),
            "added": added,
            "removed": removed,
            // An open prerequisite fences new dispatch of this work at its next check.
            "openPrerequisites": open.iter().map(Id::as_str).collect::<Vec<_>>(),
        }),
    )?;
    Ok(Some(after))
}

/// Refuses the unit of work when any of `targets` now reaches one of its prerequisites (a
/// cycle), or when the graph is too large to establish that it does not.
pub(crate) fn check_cycles(tx: &mut dyn WriteTransaction, targets: &[Id]) -> R<()> {
    for t in targets {
        let prereqs = tx.work().prerequisites(t)?;
        match closes_cycle(tx.work(), t, &prereqs) {
            Ok(None) => {}
            Ok(Some(p)) => return Err(conflict("dependency_cycle", &p)),
            Err(_) => {
                return Err(Refusal::new(
                    DomainErrorCode::Conflict,
                    "dependency_graph_bound",
                    Some(t.clone()),
                )
                .retry(RetryAdvice::Never)
                .into());
            }
        }
    }
    Ok(())
}

impl WorkService {
    /// Replaces the incoming links of `target` with `edges` (each `* → target`).
    fn replace_incoming(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        target: &StoredWork,
        edges: &[DependencyEdge],
    ) -> R<(StoredWork, bool)> {
        open_project(tx, ctx, &target.project_id)?;
        let mut prereqs: Vec<Id> = Vec::new();
        for e in edges {
            if e.successor != target.id {
                return Err(super::invalid("successor", Some(&e.successor)));
            }
            // Visibility first: an item the invocation cannot see is not found.
            if e.predecessor != target.id {
                visible_work(tx.work(), ctx, &e.predecessor)?;
            }
            prereqs.push(e.predecessor.clone());
        }
        match set_links(tx, ctx, &target.project_id, &target.id, &prereqs)? {
            None => Ok((target.clone(), false)),
            Some(after) => {
                check_cycles(tx, std::slice::from_ref(&target.id))?;
                Ok((after, true))
            }
        }
    }

    pub(crate) fn task_dependencies(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &tops::DependencyLinksInput,
    ) -> R<DomainDecision> {
        let target = visible_card(tx.work(), ctx, &i.task_id)?;
        check_revision(&target, i.expected_revision.get())?;
        let (w, changed) = self.replace_incoming(tx, ctx, &target, &i.replacement.edges.0)?;
        let result = OperationResult::TasksDependencyLinks {
            value: super::items::tx_record(tx, &w)?,
        };
        Ok(if changed {
            DomainDecision::Applied { result }
        } else {
            DomainDecision::NoChange { result }
        })
    }

    pub(crate) fn feature_dependencies(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &fops::DependenciesInput,
    ) -> R<DomainDecision> {
        let target = visible_feature(tx.work(), ctx, &i.feature_id.0)?;
        check_revision(&target, i.expected_revision.get())?;
        let (w, changed) = self.replace_incoming(tx, ctx, &target, &i.replacement.edges.0)?;
        let result = OperationResult::FeaturesDependencies {
            value: super::items::tx_record(tx, &w)?,
        };
        Ok(if changed {
            DomainDecision::Applied { result }
        } else {
            DomainDecision::NoChange { result }
        })
    }
}
