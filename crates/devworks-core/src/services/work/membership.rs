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
    let mut fenced_ids = HashSet::new();
    for a in assignments
        .iter()
        .filter(|a| a.work_id != a.root_id && a.state.is_live())
    {
        if fenced_ids.contains(&a.id) {
            continue;
        }
        let primary = a.work_id.0 == after.id;
        let delegated = tx
            .runs()
            .agent_assignment_context(&a.id)?
            .map(|context| {
                serde_json::from_value::<Vec<Id>>(context.task_ids)
                    .map(|tasks| tasks.contains(&after.id))
                    .map_err(|_| StorageError::InvalidRecord("assignment task scope".into()))
            })
            .transpose()?
            .unwrap_or(false);
        if !primary && !delegated {
            continue;
        }
        let tree = fence(tx, ctx.principal(), after, a, &assignments)?;
        fenced_ids.extend(tree.iter().cloned());
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
                json!({"kind":"DevWorksMembershipHandoff","version":1,"membershipHandoff":{"workId":after.id,"secondaryTaskId":(child.work_id.0 != after.id).then_some(&after.id),"fromRoot":root,"toFeature":after.parent_feature_id,"hadCheckpoint":child.checkpoint.is_some(),"previousCheckpoint":child.checkpoint,"subtree":tree.iter().map(|id|id.0.as_str()).collect::<Vec<_>>()}}),
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
        crate::services::runs::ensure_control(tx, &work.id)?;
        raise_blocker(
            tx,
            actor,
            &work,
            HoldSubject::default(),
            HoldCause::Policy,
            a.id.0.as_str(),
            after.revision,
            "existing worker subtree is quiescing before its membership handoff",
            json!({"membershipHandoff":true,"assignmentId":a.id.0,"fromRoot":root,"workId":after.id}),
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
            let pending = active_blockers(
                tx.runs(),
                &BlockerFilter::RootLevel(RootId(work.id.clone())),
            )?
            .iter()
            .any(|hold| {
                hold.origin_id == a.id.0.as_str()
                    && hold.clearance.get("membershipHandoff") == Some(&Value::Bool(true))
            });
            if !pending {
                continue;
            }
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
            if !tx.runs().assignment_handoff_quiescent(&a.id)? {
                continue;
            }
            if a.checkpoint
                .as_ref()
                .and_then(|c| c.get(MARKER))
                .and_then(|m| m.get("secondaryTaskId"))
                .is_some_and(|id| !id.is_null())
            {
                allocation::retire_handoff_claims(tx, &work.id, &actor)?;
                clear_handoff_hold(tx, &actor, &RootId(work.id.clone()), &a.id)?;
                let id = EventId(tx.new_id(crate::ports::IdKind::Event));
                tx.history().append(NewEvent{id,actor:actor.clone(),project_id:Some(work.project_id.clone()),root_id:Some(a.root_id.clone()),operation_id:None,aggregate_kind:super::WORK_AGGREGATE.into(),aggregate_id:work.id.as_str().into(),aggregate_revision:work.revision,kind:"WorkerMembershipScopeRetired".into(),payload:json!({"assignmentId":a.id.0,"sessionId":a.session_id,"movedSecondaryTask":a.checkpoint.as_ref().and_then(|c| c.get(MARKER)).and_then(|m| m.get("secondaryTaskId")),"checkpointRetained":true})})?;
                changed += 1;
                continue;
            }
            let current_session = tx.sessions().session(&a.session_id)?;
            if current_session.as_ref().is_none_or(|s| s.closed()) {
                allocation::retire_handoff_claims(tx, &work.id, &actor)?;
                clear_handoff_hold(tx, &actor, &RootId(work.id.clone()), &a.id)?;
                let id = EventId(tx.new_id(crate::ports::IdKind::Event));
                tx.history().append(NewEvent{id,actor:actor.clone(),project_id:Some(work.project_id.clone()),root_id:Some(target.clone()),operation_id:None,aggregate_kind:super::WORK_AGGREGATE.into(),aggregate_id:work.id.as_str().into(),aggregate_revision:work.revision,kind:"WorkerHandoffRetired".into(),payload:json!({"assignmentId":a.id.0,"sessionId":a.session_id,"fromRoot":a.root_id,"toRoot":target,"reason":"worker session closed","checkpointRetained":true})})?;
                changed += 1;
                continue;
            }
            let parent = if work.parent_feature_id.is_some() {
                match coordinator(tx.runs(), &target)? {
                    Some(parent) => Some(parent.id),
                    None => continue,
                }
            } else {
                None
            };
            let session = current_session.expect("checked open worker session");
            let savepoint = tx.savepoint()?;
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
            match tx
                .runs()
                .reparent_assignment(&moved, a.revision, session.revision)
            {
                Ok(()) => tx.release(savepoint)?,
                Err(StorageError::Busy) => {
                    tx.rollback_to(savepoint)?;
                    continue;
                }
                Err(error) => {
                    tx.rollback_to(savepoint)?;
                    return Err(error);
                }
            }
            clear_handoff_hold(tx, &actor, &RootId(work.id.clone()), &a.id)?;
            if moved.parent_id.is_some() {
                raise_blocker(
                    tx,
                    &actor,
                    &newroot,
                    HoldSubject {
                        assignment: Some(moved.id.clone()),
                        operation: None,
                    },
                    HoldCause::Capacity,
                    "membership_destination_slot",
                    moved.generation,
                    "transferred worker must reacquire destination eligibility and subagent capacity",
                    json!({"membershipSlot":true}),
                )?;
            }
            record(tx, &actor, &work, &a, &moved, session.context_epoch + 1)?;
            changed += 1;
        }
    }
    changed += release_destination_capacity_holds(tx, control, project)?;
    Ok(changed)
}
fn release_destination_capacity_holds(
    tx: &mut dyn WriteTransaction,
    control: &ControlService,
    project: &ProjectId,
) -> Result<usize, StorageError> {
    let limit = crate::settings::service::max_subagents(tx.configuration(), project)?;
    let actor = tx.store().host_actor.clone();
    let mut changed = 0;
    let mut after = None;
    loop {
        let page = tx.runs().project_roots(project, after.as_ref(), PAGE)?;
        for root in &page {
            let assignments = root_assignments(tx.runs(), root, true)?;
            let mut used = assignments
                .iter()
                .filter(|a| a.parent_id.is_some() && a.state == AssignmentState::Active)
                .count();
            for assignment in assignments {
                let holds =
                    active_blockers(tx.runs(), &BlockerFilter::Assignment(assignment.id.clone()))?
                        .into_iter()
                        .filter(|b| {
                            b.cause == HoldCause::Capacity
                                && b.origin_id == "membership_destination_slot"
                        })
                        .collect::<Vec<_>>();
                if holds.is_empty() {
                    continue;
                }
                let Some(workroot) = tx.sessions().work_root(root)? else {
                    continue;
                };
                let session = tx.sessions().session(&assignment.session_id)?;
                if session.as_ref().is_none_or(|s| s.closed())
                    && tx.runs().session_turn(&assignment.session_id)?.is_none()
                {
                    let mut retired = assignment.clone();
                    retired.state = AssignmentState::Superseded;
                    retired.slot_held = false;
                    retired.revision += 1;
                    tx.runs().update_assignment(&retired, assignment.revision)?;
                    for hold in holds {
                        clear_blocker(
                            tx,
                            &actor,
                            &workroot,
                            &hold,
                            json!({"reason":"transferred worker session closed before admission"}),
                        )?;
                    }
                    changed += 1;
                    continue;
                }
                if used >= limit
                    || !tx
                        .runs()
                        .live_activation(root)?
                        .is_some_and(|a| a.phase == ActivationPhase::Running)
                {
                    continue;
                }
                let eligibility = control.evaluate(
                    tx.runs(),
                    &workroot,
                    &crate::services::runs::Target::Assignment(assignment.id.clone()),
                    ActivationPurpose::Implementation,
                )?;
                if eligibility
                    .conditions
                    .iter()
                    .any(|condition| !holds.iter().any(|hold| hold.id == condition.hold_id))
                {
                    continue;
                }
                if assignment.purpose != crate::ports::storage::AssignmentPurpose::Child {
                    // Legacy primary worker assignments are resumed by their existing
                    // coordinator path rather than the child supervisor.
                    let mut active = assignment.clone();
                    active.state = AssignmentState::Active;
                    active.slot_held = true;
                    active.revision += 1;
                    tx.runs().update_assignment(&active, assignment.revision)?;
                }
                // A Child must remain Waiting until the destination supervisor launches it.
                for hold in holds {
                    clear_blocker(
                        tx,
                        &actor,
                        &workroot,
                        &hold,
                        json!({"destinationAllowance":limit,"occupiedBefore":used,"assignmentId":assignment.id.0}),
                    )?;
                }
                changed += 1;
                used += 1;
            }
        }
        if page.len() < usize::from(PAGE) {
            return Ok(changed);
        }
        after = page.last().cloned();
    }
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
        .filter(|hold| {
            hold.cause == HoldCause::Policy
                && hold.origin_id == assignment.0.as_str()
                && hold.clearance.get(MARKER) == Some(&Value::Bool(true))
        })
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

/// An empty source cannot reacquire the project slot while its only worker awaits routing.
pub(crate) fn awaiting_outgoing_handoff(
    tx: &mut dyn WriteTransaction,
    root: &RootId,
) -> Result<bool, StorageError> {
    for assignment in root_assignments(tx.runs(), root, false)? {
        if marker(&assignment)
            && tx.work().work(&assignment.work_id.0)?.is_some_and(|work| {
                work.parent_feature_id
                    .as_ref()
                    .is_some_and(|parent| parent != &root.0)
            })
            && active_blockers(tx.runs(), &BlockerFilter::RootLevel(assignment.work_id))?
                .iter()
                .any(|hold| hold.clearance.get(MARKER) == Some(&Value::Bool(true)))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A destination coordinator must be activatable to accept an incoming worker. These
/// temporary routing/capacity waits are not permission to resume a user/budget-paused task.
pub(crate) fn needs_root_activation(
    tx: &mut dyn WriteTransaction,
    feature: &RootId,
) -> Result<bool, StorageError> {
    let Some(root) = tx.sessions().work_root(feature)? else {
        return Ok(false);
    };
    let (_, tasks) = super::queue::queue(tx.work(), &root.project_id, Some(&feature.0))?;
    for candidate in tasks {
        if !candidate.open_prerequisites.is_empty()
            || !matches!(
                candidate.semantic.as_str(),
                super::workflow::READY | super::workflow::IN_PROGRESS
            )
        {
            continue;
        }
        if crate::services::runs::control_of(tx.runs(), &RootId(candidate.work.id.clone()))?.desired
            == DesiredExecution::Paused
        {
            continue;
        }
        let own = active_blockers(
            tx.runs(),
            &BlockerFilter::RootLevel(RootId(candidate.work.id.clone())),
        )?;
        if own
            .iter()
            .any(|h| h.clearance.get("membershipHandoff") != Some(&Value::Bool(true)))
        {
            continue;
        }
        if let Some(handoff) = own
            .iter()
            .find(|h| h.clearance.get("membershipHandoff") == Some(&Value::Bool(true)))
        {
            let assignment = AssignmentId(
                Id::new(handoff.origin_id.clone())
                    .map_err(|_| StorageError::InvalidRecord("membership hold origin".into()))?,
            );
            if active_blockers(tx.runs(), &BlockerFilter::Assignment(assignment))?.is_empty() {
                return Ok(true);
            }
        }
        if let Some(assignment) = tx
            .runs()
            .live_assignment_for_work(&RootId(candidate.work.id))?
            && assignment.root_id == *feature
        {
            let holds = active_blockers(tx.runs(), &BlockerFilter::Assignment(assignment.id))?;
            if !holds.is_empty()
                && holds.iter().all(|h| {
                    h.cause == HoldCause::Capacity && h.origin_id == "membership_destination_slot"
                })
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
