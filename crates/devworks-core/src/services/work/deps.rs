//! Blocking links (WORK-02, WORK-05). A directed link `A → B` means **A blocks B**; one stored
//! edge yields both views (Blocked by, Blocks). A replacement changes exactly the target's
//! incoming links (an empty list removes them). Endpoints must be visible items of the same
//! project (cards across features and standalone issues may link; features link to features);
//! self links, duplicates and cycles across the whole project graph are refused. The check reads
//! the graph inside the command's unit of work on the sole write owner, so two individually valid
//! concurrent edits cannot together form a cycle: the second one sees the first. A graph too large
//! to search within the bound is refused visibly, never assumed acyclic. A refusal writes nothing.
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
use crate::protocol::{DomainErrorCode, Id, RetryAdvice};
use crate::services::sessions::Refusal;
use serde_json::json;
use std::collections::BTreeSet;

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

impl WorkService {
    /// Replaces the incoming links of `target` with `edges` (each `* → target`).
    fn replace_incoming(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        target: &StoredWork,
        edges: &[DependencyEdge],
        features: bool,
    ) -> R<(StoredWork, bool)> {
        open_project(tx, ctx, &target.project_id)?;
        let mut prereqs: Vec<Id> = Vec::new();
        for e in edges {
            if e.successor != target.id {
                return Err(super::invalid("successor", Some(&e.successor)));
            }
            if e.predecessor == target.id {
                return Err(conflict("self_link", &target.id));
            }
            prereqs.push(e.predecessor.clone());
        }
        let prereqs = id_set(&prereqs, "duplicate_link")?;
        for p in &prereqs {
            let w = visible_work(tx.work(), ctx, p)?;
            if w.project_id != target.project_id {
                // Never a link into another project (nor a disclosure of one).
                return Err(not_found("work", p).into());
            }
            if features != (w.kind == FEATURE) {
                return Err(conflict("link_kind", p));
            }
        }
        let current = tx.work().prerequisites(&target.id)?;
        if current == prereqs {
            return Ok((target.clone(), false));
        }
        // Completed work is not reopened by link editing: its prerequisites stay as they were.
        let statuses = tx.work().statuses(&target.project_id)?;
        if target
            .status_id
            .as_ref()
            .and_then(|s| semantic_of(&statuses, s))
            == Some(DONE)
        {
            return Err(conflict("work_completed", &target.id));
        }
        let added: Vec<Id> = prereqs
            .iter()
            .filter(|p| !current.contains(p))
            .cloned()
            .collect();
        match closes_cycle(tx.work(), &target.id, &added) {
            Ok(None) => {}
            Ok(Some(p)) => return Err(conflict("dependency_cycle", &p)),
            Err(_) => {
                return Err(Refusal::new(
                    DomainErrorCode::Conflict,
                    "dependency_graph_bound",
                    Some(target.id.clone()),
                )
                .retry(RetryAdvice::Never)
                .into());
            }
        }
        tx.work().set_prerequisites(&target.id, &prereqs)?;
        let mut after = target.clone();
        after.revision = target.revision + 1;
        after.updated_at_ms = tx.now_ms();
        after.updated_by = Some(ctx.principal().clone());
        tx.work().update_work(&after, target.revision)?;
        let removed: Vec<&str> = current
            .iter()
            .filter(|p| !prereqs.contains(p))
            .map(Id::as_str)
            .collect();
        let open = tx.work().open_prerequisites(&target.id)?;
        event(
            tx,
            ctx,
            &after,
            "DependenciesReplaced",
            json!({
                "blockedBy": prereqs.iter().map(Id::as_str).collect::<Vec<_>>(),
                "added": added.iter().map(Id::as_str).collect::<Vec<_>>(),
                "removed": removed,
                // An open prerequisite fences new dispatch of this work at its next check.
                "openPrerequisites": open.iter().map(Id::as_str).collect::<Vec<_>>(),
            }),
        )?;
        Ok((after, true))
    }

    pub(crate) fn task_dependencies(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &tops::DependencyLinksInput,
    ) -> R<DomainDecision> {
        let target = visible_card(tx.work(), ctx, &i.task_id)?;
        check_revision(&target, i.expected_revision.get())?;
        let (w, changed) =
            self.replace_incoming(tx, ctx, &target, &i.replacement.edges.0, false)?;
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
        let (w, changed) = self.replace_incoming(tx, ctx, &target, &i.replacement.edges.0, true)?;
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
