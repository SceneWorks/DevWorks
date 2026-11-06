//! A task's existing worker follows an association change only after its old work quiesces.
//! Membership never duplicates execution or clears a user/budget hold. The old assignment's
//! subtree is fenced immediately; its transcript/checkpoint remains durable throughout.
use super::{R, allocation};
use crate::context::InvocationContext;
use crate::ports::storage::*;
use crate::protocol::{ActorId, AssignmentId, EventId, Id, ProjectId, RootId};
use crate::services::runs::{
    ControlService, HoldSubject, active_blockers, clear_blocker, coordinator, raise_blocker,
    root_assignments,
};
use serde_json::{Value, json};
use std::collections::HashSet;
const PAGE: u16 = 500;
const MARKER: &str = "membershipHandoff";

fn marker(a: &StoredAssignment) -> bool {
    a.state == AssignmentState::Superseded
        && a.checkpoint.as_ref().is_some_and(|c| {
            c.get("kind") == Some(&json!("DevWorksMembershipHandoff"))
                && c.get("version") == Some(&json!(1))
                && c.get(MARKER).is_some()
        })
}
/// Called inside the membership write, after the new parent is stored. Source dispatch is now
/// fenced, even if a provider response or process from the previous turn has not yet settled.
pub fn request(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    before: &StoredWork,
    after: &StoredWork,
) -> R<()> {
    let Some(old_root) = &before.parent_feature_id else {
        return Ok(());
    };
    let root = RootId(old_root.clone());
    let assignments = root_assignments(tx.runs(), &root, false)?;
    for a in assignments
        .iter()
        .filter(|a| a.work_id.0 == after.id && a.work_id != a.root_id && a.state.is_live())
    {
        let tree = fence(tx, ctx.principal(), after, a, &assignments)?;
        super::event(
            tx,
            ctx,
            after,
            "WorkerHandoffRequested",
            json!({"assignmentId":a.id.0,"fromRoot":root,"toFeature":after.parent_feature_id,"sessionId":a.session_id,"fencedDescendants":tree.len()-1}),
        )?;
    }
    Ok(())
}
fn fence(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    after: &StoredWork,
    a: &StoredAssignment,
    assignments: &[StoredAssignment],
) -> Result<HashSet<AssignmentId>, StorageError> {
    let root = a.root_id.clone();
    let mut tree = HashSet::from([a.id.clone()]);
    loop {
        let count = tree.len();
        for child in assignments {
            if child.parent_id.as_ref().is_some_and(|p| tree.contains(p)) {
                tree.insert(child.id.clone());
            }
        }
        if tree.len() == count {
            break;
        }
    }
    // Retired descendants remain attributable to their original root; no old-root child
    // can become runnable merely because its parent follows the moved task.
    for child in assignments
        .iter()
        .filter(|child| tree.contains(&child.id) && child.state.is_live())
    {
        let mut fenced = child.clone();
        fenced.state = AssignmentState::Superseded;
        fenced.revision += 1;
        fenced.generation += 1;
        fenced.slot_held = false;
        if child.id == a.id {
            fenced.checkpoint = Some(
                json!({"kind":"DevWorksMembershipHandoff","version":1,"membershipHandoff":{"workId":after.id,"fromRoot":root,"toFeature":after.parent_feature_id,"hadCheckpoint":child.checkpoint.is_some(),"previousCheckpoint":child.checkpoint,"subtree":tree.iter().map(|id|id.0.as_str()).collect::<Vec<_>>()}}),
            );
        }
        tx.runs().update_assignment(&fenced, child.revision)?;
    }
    let mut guarded = HashSet::new();
    for child in assignments.iter().filter(|child| tree.contains(&child.id)) {
        if !guarded.insert(child.work_id.clone()) {
            continue;
        }
        let work =
            tx.sessions()
                .work_root(&child.work_id)?
                .ok_or_else(|| StorageError::NotFound {
                    kind: "work",
                    id: child.work_id.0.as_str().into(),
                })?;
        raise_blocker(
            tx,
            actor,
            &work,
            HoldSubject::default(),
            HoldCause::Policy,
            a.id.0.as_str(),
            after.revision,
            "existing worker subtree is quiescing before its membership handoff",
            json!({"assignmentId":a.id.0,"fromRoot":root,"workId":after.id}),
        )?;
    }
    Ok(tree)
}
fn subtree(a: &StoredAssignment) -> Result<HashSet<AssignmentId>, StorageError> {
    let value = a
        .checkpoint
        .as_ref()
        .and_then(|c| c.get(MARKER))
        .and_then(|m| m.get("subtree"))
        .cloned()
        .unwrap_or_else(|| json!([]));
    let ids: Vec<Id> = serde_json::from_value(value)
        .map_err(|_| StorageError::InvalidRecord("membership handoff subtree".into()))?;
    Ok(ids.into_iter().map(AssignmentId).collect())
}
/// Unclaimed old deliveries whose effects never started are cancelled, never copied to a new
/// root. An uncertain/dispatched effect must be observed before this handoff can finish.
fn quiesce(
    tx: &mut dyn WriteTransaction,
    a: &StoredAssignment,
    tree: &HashSet<AssignmentId>,
) -> Result<bool, StorageError> {
    for child in root_assignments(tx.runs(), &a.root_id, false)?
        .iter()
        .filter(|child| tree.contains(&child.id))
    {
        if let Some(turn) = tx.runs().session_turn(&child.session_id)? {
            let current = tx
                .runs()
                .activation(&turn.execution_id)?
                .is_some_and(|owner| {
                    owner.phase != ActivationPhase::Stopped
                        && owner.owner_epoch == turn.owner_epoch
                        && owner
                            .lease_until_ms
                            .is_some_and(|until| until > tx.now_ms())
                });
            if current {
                return Ok(false);
            }
            tx.runs().delete_turn(&child.session_id, &turn.turn_id)?;
        }
    }
    let mut after = None;
    loop {
        let page = tx
            .resources()
            .processes(&ProcessFilter::Unsettled, after.as_ref(), PAGE)?;
        if page.iter().any(|process| {
            process
                .assignment_id
                .as_ref()
                .is_some_and(|id| tree.contains(id))
        }) {
            return Ok(false);
        }
        if page.len() < usize::from(PAGE) {
            break;
        }
        after = page.last().map(|p| p.id.clone());
    }
    let mut after = None;
    loop {
        let page = tx
            .runs()
            .root_deliveries(&a.root_id, after.as_ref(), PAGE)?;
        for id in &page {
            let Some(entry) = tx.queue().entry(id)? else {
                continue;
            };
            let Some(operation) = entry
                .operation_id
                .as_ref()
                .and_then(|id| tx.operations().operation(id).transpose())
                .transpose()?
            else {
                continue;
            };
            if !operation
                .assignment_id
                .as_ref()
                .is_some_and(|id| tree.contains(id))
            {
                continue;
            }
            if entry.state == QueueState::Claimed || possibly_dispatched(operation.state) {
                return Ok(false);
            }
            if cancellable_before_dispatch(operation.state) {
                tx.queue().cancel(id)?;
            }
        }
        if page.len() < usize::from(PAGE) {
            break;
        }
        after = page.last().cloned();
    }
    Ok(true)
}
/// Shared scheduling boundary used by the native supervisor. Waiting for a destination
/// coordinator does not keep an old assignment runnable or occupy its subagent slot.
pub fn reconcile_project(
    tx: &mut dyn WriteTransaction,
    control: &ControlService,
    project: &ProjectId,
) -> Result<usize, StorageError> {
    let mut roots = Vec::new();
    let mut after = None;
    loop {
        let page = tx.runs().project_roots(project, after.as_ref(), PAGE)?;
        let more = page.len() == usize::from(PAGE);
        after = page.last().cloned();
        roots.extend(page);
        if !more {
            break;
        }
    }
    let actor = tx.store().host_actor.clone();
    let mut changed = 0;
    for source in roots {
        let existing = root_assignments(tx.runs(), &source, false)?;
        if tx.runs().live_activation(&source)?.is_none() {
            for assignment in existing
                .iter()
                .filter(|a| a.root_id == a.work_id && a.state.is_live())
            {
                if let Some(work) = tx.work().work(&assignment.work_id.0)?
                    && work.parent_feature_id.is_some()
                {
                    fence(tx, &actor, &work, assignment, &existing)?;
                }
            }
        }
        for a in root_assignments(tx.runs(), &source, false)?
            .into_iter()
            .filter(marker)
        {
            let Some(work) = tx.work().work(&a.work_id.0)? else {
                continue;
            };
            let target = RootId(
                work.parent_feature_id
                    .clone()
                    .unwrap_or_else(|| work.id.clone()),
            );
            allocation::request_handoff_yield(tx, control, &a.root_id)?;
            let tree = subtree(&a)?;
            if !quiesce(tx, &a, &tree)? {
                continue;
            }
            // Descendant scopes have ended safely and may be reassigned independently in
            // their original feature even while the moved parent awaits its new coordinator.
            for child in root_assignments(tx.runs(), &a.root_id, false)?
                .iter()
                .filter(|child| tree.contains(&child.id) && child.id != a.id)
            {
                clear_handoff_hold(tx, &actor, &child.work_id, &a.id)?;
            }
            let parent = if work.parent_feature_id.is_some() {
                match coordinator(tx.runs(), &target)? {
                    Some(parent) => Some(parent.id),
                    None => continue,
                }
            } else {
                None
            };
            let session =
                tx.sessions()
                    .session(&a.session_id)?
                    .ok_or_else(|| StorageError::NotFound {
                        kind: "session",
                        id: a.session_id.as_str().into(),
                    })?;
            if session.closed() {
                continue;
            }
            allocation::retire_handoff_claims(tx, &work.id, &actor)?;
            let mut moved = a.clone();
            moved.root_id = target.clone();
            moved.parent_id = parent;
            moved.revision += 1;
            moved.generation += 1;
            moved.state = AssignmentState::Waiting;
            moved.slot_held = false;
            moved.execution_id = tx
                .runs()
                .live_activation(&target)?
                .filter(|a| a.phase == ActivationPhase::Running)
                .map(|a| a.id);
            moved.scope_key = None;
            moved.scope_digest = None;
            moved.instruction_revision = 0;
            let newroot =
                tx.sessions()
                    .work_root(&target)?
                    .ok_or_else(|| StorageError::NotFound {
                        kind: "root",
                        id: target.0.as_str().into(),
                    })?;
            moved.plan_revision = control
                .scope()
                .plan(tx.runs(), &newroot)?
                .approved_revision();
            moved.checkpoint = a
                .checkpoint
                .as_ref()
                .and_then(|c| c.get(MARKER))
                .filter(|m| m.get("hadCheckpoint") == Some(&Value::Bool(true)))
                .and_then(|m| m.get("previousCheckpoint"))
                .cloned();
            tx.runs()
                .reparent_assignment(&moved, a.revision, session.revision)?;
            clear_handoff_hold(tx, &actor, &RootId(work.id.clone()), &a.id)?;
            record(tx, &actor, &work, &a, &moved, session.context_epoch + 1)?;
            changed += 1;
        }
    }
    Ok(changed)
}
fn clear_handoff_hold(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    work: &RootId,
    assignment: &AssignmentId,
) -> Result<(), StorageError> {
    let Some(root) = tx.sessions().work_root(work)? else {
        return Ok(());
    };
    for hold in active_blockers(tx.runs(), &BlockerFilter::RootLevel(work.clone()))?
        .into_iter()
        .filter(|hold| hold.cause == HoldCause::Policy && hold.origin_id == assignment.0.as_str())
    {
        clear_blocker(
            tx,
            actor,
            &root,
            &hold,
            json!({"assignmentId":assignment.0,"reason":"worker subtree quiesced and responsibility reconciled"}),
        )?;
    }
    Ok(())
}
fn record(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    work: &StoredWork,
    old: &StoredAssignment,
    new: &StoredAssignment,
    epoch: i64,
) -> Result<(), StorageError> {
    let id = EventId(tx.new_id(crate::ports::IdKind::Event));
    tx.history().append(NewEvent{id,actor:actor.clone(),project_id:Some(work.project_id.clone()),root_id:Some(new.root_id.clone()),operation_id:None,aggregate_kind:super::WORK_AGGREGATE.into(),aggregate_id:work.id.as_str().into(),aggregate_revision:work.revision,kind:"WorkerMembershipHandedOff".into(),payload:json!({"assignmentId":new.id.0,"sessionId":new.session_id,"fromRoot":old.root_id,"fromParent":old.parent_id,"toRoot":new.root_id,"toParent":new.parent_id,"contextEpoch":epoch,"checkpointPreserved":new.checkpoint.is_some()})})?;
    Ok(())
}
