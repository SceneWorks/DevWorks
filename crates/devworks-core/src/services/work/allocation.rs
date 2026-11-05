//! Atomic work reservations, separate from activation and assignment ownership (WORK-09/10).
//! The gateway supplies request replay; every allocation and handoff runs on its write owner.
use super::{
    R, WorkService, conflict, event, invalid, open_project, stale, visible_feature, visible_work,
};
use crate::context::InvocationContext;
use crate::operations::next_work as ops;
use crate::ports::IdKind;
use crate::ports::storage::*;
use crate::protocol::records::{AllocationClaimRecord, ClaimRecord, LeaseState, WorkKind};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::{ActorId, DomainErrorCode, Id, ProjectId, RevisionDimension, RootId};
use crate::services::runs::scope::{PlanState, Readiness};
use crate::services::runs::{ControlService, Target, control_of, root_assignments};
use crate::services::sessions::{Refusal, counter};
use serde_json::json;

pub const OPERATIONS: [&str; 5] = [
    "next_work.claim_project_queue",
    "next_work.claim_feature_task",
    "next_work.inspect_claim",
    "next_work.inspect_queue",
    "next_work.release_claim",
];
const PAGE: u16 = 500;
const MAX_LEASE_MS: i64 = 86_400_000;

fn live(c: &StoredAllocationClaim) -> bool {
    matches!(c.state.as_str(), "Active" | "Transferred" | "Yielding")
}
fn owner_heartbeat(
    tx: &mut dyn WriteTransaction,
    c: &StoredAllocationClaim,
) -> Result<Option<i64>, StorageError> {
    let Some(session) = &c.session_id else {
        return Ok(None);
    };
    if !tx.sessions().session(session)?.is_some_and(|s| !s.closed()) {
        return Ok(None);
    }
    let Some(turn) = tx.runs().session_turn(session)? else {
        return Ok(None);
    };
    if tx.store().boot_id.as_ref() != Some(&turn.boot_id) {
        return Ok(None);
    }
    let Some(a) = tx.runs().activation(&turn.execution_id)? else {
        return Ok(None);
    };
    if a.phase != ActivationPhase::Running
        || a.owner.is_none()
        || a.owner_epoch != turn.owner_epoch
        || a.boot_id != turn.boot_id
        || a.lease_until_ms.is_none_or(|until| until <= tx.now_ms())
    {
        return Ok(None);
    }
    if !tx
        .runs()
        .assignment(&turn.assignment_id)?
        .is_some_and(|assignment| {
            assignment.state.is_live()
                && assignment.session_id == *session
                && assignment.execution_id.as_ref() == Some(&a.id)
        })
    {
        return Ok(None);
    }
    Ok(a.lease_until_ms)
}
fn reserved(
    tx: &mut dyn WriteTransaction,
    c: &StoredAllocationClaim,
) -> Result<bool, StorageError> {
    if !live(c) {
        return Ok(false);
    }
    if c.state != "Active" {
        return Ok(true);
    }
    Ok(c.expires_at_ms > tx.now_ms() || owner_heartbeat(tx, c)?.is_some())
}
fn owned(c: &StoredAllocationClaim, ctx: &InvocationContext) -> bool {
    c.owner_id == *ctx.principal()
        && c.session_id.as_ref() == ctx.session_id()
        && (c.session_id.is_some() || c.ingress_namespace == *ctx.ingress_namespace())
}
pub fn claims(
    read: &dyn WorkRead,
    project: &ProjectId,
) -> Result<Vec<StoredAllocationClaim>, StorageError> {
    let mut out: Vec<StoredAllocationClaim> = Vec::new();
    loop {
        let page = read.allocation_claims(project, out.last().map(|c| &c.id), PAGE)?;
        let more = page.len() == usize::from(PAGE);
        out.extend(page);
        if !more {
            return Ok(out);
        }
    }
}
fn change(
    tx: &mut dyn WriteTransaction,
    c: &StoredAllocationClaim,
    state: &str,
    actor: &ActorId,
) -> Result<StoredAllocationClaim, StorageError> {
    let mut next = c.clone();
    if let Some(w) = tx.work().work(&c.work_id)? {
        next.work_revision = w.revision;
        next.parent_feature_id = w.parent_feature_id;
    }
    next.state = state.into();
    next.revision += 1;
    next.fence += 1;
    tx.work().update_allocation_claim(&next, c.revision)?;
    let id = crate::protocol::EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent { id, actor: actor.clone(), project_id: Some(c.project_id.clone()), root_id: Some(c.root_id.clone()), operation_id: None,
        aggregate_kind: "allocation_claim".into(), aggregate_id: c.id.as_str().into(), aggregate_revision: next.revision,
        kind: format!("Allocation{state}"), payload: json!({"claimId": c.id, "workId":c.work_id,"rootId":c.root_id,"fence":next.fence,"ownerId":c.owner_id,"sessionId":c.session_id}) })?;
    Ok(next)
}
fn wire(c: &StoredAllocationClaim, now: i64) -> AllocationClaimRecord {
    AllocationClaimRecord {
        id: c.id.clone(),
        work_id: c.work_id.clone(),
        owner_id: c.owner_id.clone(),
        fence: counter(c.fence),
        expires_at_ms: counter(c.expires_at_ms),
        work_kind: Some(if c.kind == FEATURE {
            WorkKind::Feature
        } else if c.parent_feature_id.is_some() {
            WorkKind::Task
        } else {
            WorkKind::Issue
        }),
        work_revision: Some(crate::services::sessions::rev(c.work_revision)),
        project_id: Some(c.project_id.clone()),
        root_id: Some(c.root_id.clone()),
        owner_session_id: c.session_id.clone(),
        queue_revision: Some(crate::services::sessions::rev(c.queue_revision)),
        state: match c.state.as_str() {
            "Active" if c.expires_at_ms > now => LeaseState::Held,
            "Transferred" => LeaseState::Held,
            "Yielding" => LeaseState::Releasing,
            "Yielded" => LeaseState::Waiting,
            _ => LeaseState::Released,
        },
    }
}
fn root(w: &StoredWork) -> WorkRoot {
    WorkRoot {
        id: RootId(w.id.clone()),
        project_id: w.project_id.clone(),
        kind: w.kind.clone(),
        title: w.title.clone(),
        revision: w.revision,
        scope_revision: w.scope_revision,
        parent_feature_id: w.parent_feature_id.clone().map(RootId),
        created_at_ms: w.created_at_ms,
    }
}
fn has_execution(tx: &mut dyn WriteTransaction, w: &StoredWork) -> Result<bool, StorageError> {
    if tx.runs().live_activation(&RootId(w.id.clone()))?.is_some() {
        return Ok(true);
    }
    let r = RootId(w.parent_feature_id.clone().unwrap_or_else(|| w.id.clone()));
    Ok(root_assignments(tx.runs(), &r, true)?.iter().any(|a| {
        a.work_id.0 == w.id
            && a.execution_id
                .as_ref()
                .is_some_and(|_| a.state == AssignmentState::Active)
    }))
}
/// Eligibility shared by both queue scopes. Ownership is filtered separately, after these gates.
fn eligible(
    tx: &mut dyn WriteTransaction,
    service: &ControlService,
    w: &StoredWork,
    feature: Option<&StoredWork>,
    assessment: bool,
) -> Result<bool, StorageError> {
    let statuses = tx.work().statuses(&w.project_id)?;
    if w.status_id
        .as_ref()
        .and_then(|id| super::workflow::semantic_of(&statuses, id))
        .is_none_or(|s| s != super::workflow::READY && s != super::workflow::IN_PROGRESS)
    {
        return Ok(false);
    }
    if !tx.work().open_prerequisites(&w.id)?.is_empty() {
        return Ok(false);
    }
    let r = root(feature.unwrap_or(w));
    let control = service.evaluate(tx.runs(), &r, &Target::Root, ActivationPurpose::Planning)?;
    if !control.eligible {
        return Ok(false);
    }
    if let Some(f) = feature {
        if !tx.work().open_prerequisites(&f.id)?.is_empty() {
            return Ok(false);
        }
        if !crate::services::runs::active_blockers(
            tx.runs(),
            &BlockerFilter::RootLevel(RootId(w.id.clone())),
        )?
        .is_empty()
        {
            return Ok(false);
        }
        if control_of(tx.runs(), &RootId(w.id.clone()))?.desired == DesiredExecution::Paused {
            return Ok(false);
        }
        let plan = service.scope().plan(tx.runs(), &r)?;
        let PlanState::Approved { feature: unit, .. } = &plan else {
            return Ok(false);
        };
        let mut needs_assessment =
            matches!(unit.readiness, Readiness::Missing | Readiness::Stale { .. });
        if !unit.readiness.is_current() && !needs_assessment {
            return Ok(false);
        }
        let brief = super::model::WorkBrief::of(w)?;
        for key in &brief.inherits {
            let Some(unit) = plan.unit(Some(key.as_str())) else {
                return Ok(false);
            };
            if !unit.readiness.is_current() {
                if matches!(unit.readiness, Readiness::Missing | Readiness::Stale { .. }) {
                    needs_assessment = true;
                } else {
                    return Ok(false);
                }
            }
        }
        if let Some((_, readiness)) = tx.runs().feature_plan(&r.id)?.and_then(|p| p.readiness)
            && super::readiness::gating_finding(&readiness, &w.id).is_some()
        {
            return Ok(false);
        }
        let scope = tx
            .work()
            .feature_plan(&f.id)?
            .and_then(|s| s.approved)
            .and_then(|p| super::model::active_of(Some(&p)));
        if let Some(p) = scope
            && tx
                .work()
                .work_repositories(&w.id)?
                .iter()
                .any(|id| !p.draft.repository_ids.0.contains(id))
        {
            return Ok(false);
        }
        return Ok(!needs_assessment || assessment);
    }
    if !w.is_feature() {
        return Ok(service
            .evaluate(
                tx.runs(),
                &r,
                &Target::Root,
                ActivationPurpose::Implementation,
            )?
            .eligible);
    }
    let plan = service.scope().plan(tx.runs(), &r)?;
    let PlanState::Approved { feature: unit, .. } = &plan else {
        return Ok(false);
    };
    let (_, tasks) = super::queue::queue(tx.work(), &w.project_id, Some(&w.id))?;
    if tasks.is_empty() {
        return Ok(unit.readiness.is_current()
            || matches!(unit.readiness, Readiness::Missing | Readiness::Stale { .. }));
    }
    for candidate in tasks {
        if eligible(tx, service, &candidate.work, Some(w), true)? {
            return Ok(true);
        }
    }
    Ok(false)
}
/// Membership/status changes fence reservations, while a transferred execution keeps its owner.
pub fn invalidate_work(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    work: &Id,
) -> Result<(), StorageError> {
    let Some(w) = tx.work().work(work)? else {
        return Ok(());
    };
    for c in claims(tx.work(), &w.project_id)? {
        if c.work_id == *work && c.state == "Active" {
            change(tx, &c, "Invalidated", ctx.principal())?;
        }
    }
    Ok(())
}
impl WorkService {
    pub(crate) fn inspect_queue(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &ops::InspectQueueInput,
    ) -> R<(OperationResult, i64)> {
        super::visible_project(read.projects(), ctx, &i.project_id)?;
        if let Some(f) = &i.feature_id {
            let work = visible_feature(read.work(), ctx, f)?;
            if work.project_id != i.project_id {
                return Err(super::not_found("feature", f).into());
            }
        }
        let queue = i
            .feature_id
            .as_ref()
            .map_or_else(|| PROJECT_QUEUE.to_owned(), feature_queue);
        let revision = read.work().queue_revision(&i.project_id, &queue)?;
        Ok((
            OperationResult::NextWorkInspectQueue {
                value: crate::protocol::records::AllocationQueueSnapshot {
                    project_id: i.project_id.clone(),
                    feature_id: i.feature_id.clone(),
                    queue_revision: crate::services::sessions::rev(revision),
                    workflow_revision: crate::services::sessions::rev(
                        read.work().queue_revision(&i.project_id, WORKFLOW_QUEUE)?,
                    ),
                },
            },
            revision,
        ))
    }
    pub(crate) fn inspect_claim(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &ops::InspectClaimInput,
    ) -> R<(OperationResult, i64)> {
        let c = read
            .work()
            .allocation_claim(&i.claim_id)?
            .ok_or_else(|| super::not_found("claim", &i.claim_id))?;
        visible_work(read.work(), ctx, &c.work_id)?;
        Ok((
            OperationResult::NextWorkInspectClaim { value: wire(&c, 0) },
            c.revision,
        ))
    }
    pub(crate) fn allocate_project(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::ClaimProjectQueueInput,
        identity: &crate::context::ReceiptIdentity,
    ) -> R<DomainDecision> {
        self.allocate(
            tx,
            ctx,
            &i.project_id,
            None,
            i.expected_queue_revision.get(),
            i.lease_ms
                .0
                .parse()
                .map_err(|_| invalid("lease_ms", None))?,
            identity,
        )
    }
    pub(crate) fn allocate_task(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::ClaimFeatureTaskInput,
        identity: &crate::context::ReceiptIdentity,
    ) -> R<DomainDecision> {
        let f = visible_feature(tx.work(), ctx, &i.root_id.0)?;
        self.allocate(
            tx,
            ctx,
            &f.project_id,
            Some(&f),
            i.expected_task_set_revision.get(),
            i.lease_ms
                .0
                .parse()
                .map_err(|_| invalid("lease_ms", None))?,
            identity,
        )
    }
    fn allocate(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        project: &ProjectId,
        feature: Option<&StoredWork>,
        expected: i64,
        lease: i64,
        identity: &crate::context::ReceiptIdentity,
    ) -> R<DomainDecision> {
        open_project(tx, ctx, project)?;
        if lease > MAX_LEASE_MS {
            return Err(invalid("lease_ms", None));
        }
        if matches!(ctx.principal_kind(), crate::context::PrincipalKind::Agent)
            && ctx.session_id().is_none()
        {
            return Err(invalid("owner_session", None));
        }
        let key = feature.map_or_else(|| PROJECT_QUEUE.into(), |f| feature_queue(&f.id));
        let current = tx.work().queue_revision(project, &key)?;
        if current != expected {
            return Err(stale(
                RevisionDimension::Queue,
                "work_queue",
                &project.0,
                expected,
                current,
            ));
        }
        let all = claims(tx.work(), project)?;
        let mut held = std::collections::HashSet::new();
        for c in &all {
            if reserved(tx, c)? {
                held.insert(c.id.clone());
            }
        }

        if let Some(existing) = all
            .iter()
            .find(|c| c.queue_key == key && owned(c, ctx) && held.contains(&c.id))
        {
            let value = wire(existing, tx.now_ms());
            return Ok(DomainDecision::NoChange {
                result: if feature.is_some() {
                    OperationResult::NextWorkClaimFeatureTask { value }
                } else {
                    OperationResult::NextWorkClaimProjectQueue { value }
                },
            });
        }
        if feature.is_none()
            && all
                .iter()
                .any(|c| c.queue_key == PROJECT_QUEUE && held.contains(&c.id))
        {
            return Err(conflict("project_queue_owned", &project.0));
        }
        let (_, mut candidates) = super::queue::queue(tx.work(), project, feature.map(|f| &f.id))?;
        // Retained Next behavior: recover interrupted in-progress work before new Ready work.
        candidates.sort_by_key(|c| c.semantic != super::workflow::IN_PROGRESS);
        for c in candidates {
            let w = c.work;
            if !eligible(tx, &self.runs, &w, feature, false)? || has_execution(tx, &w)? {
                continue;
            }
            if all
                .iter()
                .any(|c| c.work_id == w.id && held.contains(&c.id))
            {
                continue;
            }
            // Expired reservations are retired before another owner gets a fresh monotonic fence.
            for old in all
                .iter()
                .filter(|c| c.work_id == w.id && c.state == "Active" && !held.contains(&c.id))
            {
                let state = if old.expires_at_ms <= tx.now_ms() {
                    "Expired"
                } else {
                    "Invalidated"
                };
                change(tx, old, state, ctx.principal())?;
            }
            if feature.is_none() {
                for old in all.iter().filter(|c| {
                    c.queue_key == PROJECT_QUEUE
                        && c.work_id != w.id
                        && c.state == "Active"
                        && !held.contains(&c.id)
                }) {
                    let state = if old.expires_at_ms <= tx.now_ms() {
                        "Expired"
                    } else {
                        "Invalidated"
                    };
                    change(tx, old, state, ctx.principal())?;
                }
            }
            let fence = all
                .iter()
                .filter(|c| c.work_id == w.id)
                .map(|c| c.fence)
                .max()
                .unwrap_or(0)
                + 1;
            let root_id = RootId(feature.map_or_else(|| w.id.clone(), |f| f.id.clone()));
            let claim = StoredAllocationClaim {
                id: tx.new_id(IdKind::Link),
                project_id: project.clone(),
                root_id: root_id.clone(),
                work_id: w.id.clone(),
                kind: w.kind.clone(),
                queue_key: key.clone(),
                owner_id: ctx.principal().clone(),
                session_id: ctx.session_id().cloned(),
                ingress_namespace: ctx.ingress_namespace().clone(),
                invocation_id: Id::new(format!(
                    "allocation-{}",
                    crate::fingerprint::sha256_hex(
                        format!(
                            "{}:{}",
                            identity.scope_key().as_str(),
                            identity.request_key().0.as_str()
                        )
                        .as_bytes()
                    )
                ))
                .expect("digest identifier"),
                fence,
                expires_at_ms: tx.now_ms().saturating_add(lease),
                state: "Active".into(),
                revision: 1,
                work_revision: w.revision,
                parent_feature_id: w.parent_feature_id.clone(),
                created_at_ms: tx.now_ms(),
                execution_id: None,
                root_control_revision: control_of(tx.runs(), &root_id)?.revision,
                queue_revision: current,
                feature_task_set_revision: if w.is_feature() {
                    Some(tx.work().queue_revision(project, &feature_queue(&w.id))?)
                } else {
                    None
                },
            };
            tx.work().insert_allocation_claim(&claim)?;
            super::items::bump(tx, project, std::slice::from_ref(&key))?;
            event(
                tx,
                ctx,
                &w,
                "WorkAllocated",
                json!({"claimId":claim.id,"fence":claim.fence,"ownerId":claim.owner_id,"sessionId":claim.session_id,"queueRevision":current,"invocationId":claim.invocation_id}),
            )?;
            let value = wire(&claim, tx.now_ms());
            return Ok(DomainDecision::Applied {
                result: if feature.is_some() {
                    OperationResult::NextWorkClaimFeatureTask { value }
                } else {
                    OperationResult::NextWorkClaimProjectQueue { value }
                },
            });
        }
        Err(Refusal::new(
            DomainErrorCode::DependencyBlocked,
            "no_eligible_work",
            Some(project.0.clone()),
        )
        .retry(crate::protocol::RetryAdvice::AfterBlockerClears)
        .into())
    }
    pub(crate) fn release_allocation(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::ReleaseClaimInput,
    ) -> R<DomainDecision> {
        let c = tx
            .work()
            .allocation_claim(&i.claim_id)?
            .ok_or_else(|| super::not_found("claim", &i.claim_id))?;
        visible_work(tx.work(), ctx, &c.work_id)?;
        if !owned(&c, ctx) || c.fence != i.expected_fence.get() {
            return Err(super::refuse(
                DomainErrorCode::LeaseLost,
                "claim",
                Some(&c.id),
            ));
        }
        if !live(&c) {
            return Ok(DomainDecision::NoChange {
                result: OperationResult::NextWorkReleaseClaim {
                    value: wire(&c, tx.now_ms()),
                },
            });
        }
        if c.state != "Active" && !quiesced(tx, &c.root_id)? {
            return Err(conflict("execution_not_quiescent", &c.id));
        }
        let released = change(tx, &c, "Released", ctx.principal())?;
        super::items::bump(tx, &c.project_id, std::slice::from_ref(&c.queue_key))?;
        Ok(DomainDecision::Applied {
            result: OperationResult::NextWorkReleaseClaim {
                value: wire(&released, tx.now_ms()),
            },
        })
    }
}
fn quiesced(tx: &mut dyn WriteTransaction, root: &RootId) -> Result<bool, StorageError> {
    if tx.runs().live_activation(root)?.is_some()
        || !crate::services::runs::recovery::root_quiesced(tx, root)?
        || !crate::services::runs::recovery::unsettled_processes(tx, root)?.is_empty()
    {
        return Ok(false);
    }
    if let Some(work) = tx.sessions().work_root(root)? {
        for linked in crate::services::runs::linked_roots(tx, &work)? {
            if !quiesced(tx, &linked.id)? {
                return Ok(false);
            }
        }
    }
    for a in root_assignments(tx.runs(), root, true)? {
        if a.state == AssignmentState::Active {
            return Ok(false);
        }
        if a.work_id != *root
            && (!crate::services::runs::recovery::root_quiesced(tx, &a.work_id)?
                || !crate::services::runs::recovery::unsettled_processes(tx, &a.work_id)?
                    .is_empty())
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The old wire claim remains embedded in WorkRecord for restart discovery.
pub fn projected_claim(
    read: &dyn WorkRead,
    work: &StoredWork,
) -> Result<Option<ClaimRecord>, StorageError> {
    Ok(claims(read, &work.project_id)?
        .into_iter()
        .find(|c| c.work_id == work.id && live(c))
        .map(|c| ClaimRecord {
            id: c.id,
            work_id: c.work_id,
            owner_id: c.owner_id,
            fence: counter(c.fence),
            expires_at_ms: counter(c.expires_at_ms),
            state: if c.state == "Yielding" {
                LeaseState::Releasing
            } else {
                LeaseState::Held
            },
        }))
}
/// Run Start checks the reservation in the same transaction as its activation. An unclaimed
/// explicit Start retains its normal planning policy; an automatic reservation never weakens it.
pub fn check_start(
    tx: &mut dyn WriteTransaction,
    service: &ControlService,
    ctx: &InvocationContext,
    w: &WorkRoot,
) -> R<()> {
    let Some(work) = tx.work().work(&w.id.0)? else {
        return Ok(());
    };
    let all = claims(tx.work(), &w.project_id)?;
    if all.iter().any(|c| {
        c.queue_key == PROJECT_QUEUE
            && c.root_id != w.id
            && matches!(c.state.as_str(), "Transferred" | "Yielding")
    }) {
        return Err(conflict("project_execution_owned", &w.project_id.0));
    }
    for c in all.iter().filter(|c| c.work_id == w.id.0 && live(c)) {
        if c.state == "Active" {
            if !owned(c, ctx) || !reserved(tx, c)? {
                return Err(super::refuse(
                    DomainErrorCode::LeaseLost,
                    "claim",
                    Some(&c.id),
                ));
            }
            if c.work_revision != work.revision
                || c.parent_feature_id != work.parent_feature_id
                || c.root_control_revision != control_of(tx.runs(), &w.id)?.revision
            {
                return Err(super::refuse(
                    DomainErrorCode::SourceChanged,
                    "claim",
                    Some(&c.id),
                ));
            }
            if !eligible(tx, service, &work, None, false)? {
                return Err(super::refuse(
                    DomainErrorCode::DependencyBlocked,
                    "claim",
                    Some(&c.id),
                ));
            }
        } else if c.state == "Yielding" {
            return Err(conflict("root_yielding", &c.id));
        }
    }
    Ok(())
}
/// Visible ownership handoff to the already existing root activation or new worker session.
pub fn transfer(
    tx: &mut dyn WriteTransaction,
    work: &Id,
    execution: &Id,
    session: Option<&Id>,
    actor: &ActorId,
) -> Result<(), StorageError> {
    let Some(w) = tx.work().work(work)? else {
        return Ok(());
    };
    for c in claims(tx.work(), &w.project_id)?.into_iter().filter(|c| {
        c.work_id == *work
            && (c.state == "Active"
                || (c.state == "Transferred"
                    && session.is_some()
                    && c.execution_id.as_ref() == Some(execution)))
    }) {
        let mut next = c.clone();
        next.execution_id = Some(execution.clone());
        next.session_id = session.cloned().or(c.session_id.clone());
        if let Some(session) = session
            .and_then(|id| tx.sessions().session(id).transpose())
            .transpose()?
        {
            next.owner_id = session.agent_id;
        }
        next.state = "Transferred".into();
        next.revision += 1;
        next.fence += 1;
        tx.work().update_allocation_claim(&next, c.revision)?;
        let id = crate::protocol::EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent{id,actor:actor.clone(),project_id:Some(w.project_id.clone()),root_id:Some(c.root_id),operation_id:None,aggregate_kind:"allocation_claim".into(),aggregate_id:c.id.as_str().into(),aggregate_revision:next.revision,
            kind:"AllocationTransferred".into(),payload:json!({"workId":work,"executionId":execution,"sessionId":next.session_id,"fence":next.fence})})?;
    }
    Ok(())
}
fn fully_dependency_blocked(
    tx: &mut dyn WriteTransaction,
    service: &ControlService,
    w: &StoredWork,
) -> Result<bool, StorageError> {
    if !tx.work().open_prerequisites(&w.id)?.is_empty() {
        return Ok(true);
    }
    if !w.is_feature() {
        return Ok(false);
    }
    let (_, tasks) = super::queue::queue(tx.work(), &w.project_id, Some(&w.id))?;
    let unfinished: Vec<_> = tasks
        .into_iter()
        .filter(|c| !super::workflow::finished(&c.semantic))
        .collect();
    if unfinished.is_empty() {
        return Ok(false);
    }
    let mut dependency = false;
    for c in unfinished {
        dependency |= !c.open_prerequisites.is_empty();
        if eligible(tx, service, &c.work, Some(w), true)? {
            return Ok(false);
        }
    }
    Ok(dependency)
}
/// One keep-working scheduling boundary. A fully dependency-blocked root is asked to stop
/// cooperatively, retaining coordinator/worker lineage and every explicit hold. Its project
/// slot is released only after the activation, descendants, deliveries and processes quiesce.
pub fn reconcile_project(
    tx: &mut dyn WriteTransaction,
    service: &ControlService,
    project: &ProjectId,
) -> Result<usize, StorageError> {
    let actor = tx.store().host_actor.clone();
    let mut changed = 0;
    for c in claims(tx.work(), project)? {
        if !live(&c) {
            continue;
        }
        let Some(w) = tx.work().work(&c.work_id)? else {
            continue;
        };
        if c.state == "Active" {
            if c.work_revision != w.revision || c.parent_feature_id != w.parent_feature_id {
                change(tx, &c, "Invalidated", &actor)?;
                changed += 1;
                continue;
            }
            if let Some(until) = owner_heartbeat(tx, &c)?
                && until > c.expires_at_ms
            {
                let mut renewed = c.clone();
                renewed.revision += 1;
                renewed.expires_at_ms = until;
                tx.work().update_allocation_claim(&renewed, c.revision)?;
                changed += 1;
                continue;
            }
        }
        if c.state == "Active" && !reserved(tx, &c)? {
            let state = if c.expires_at_ms <= tx.now_ms() {
                "Expired"
            } else {
                "Invalidated"
            };
            change(tx, &c, state, &actor)?;
            changed += 1;
            continue;
        }
        if c.queue_key != PROJECT_QUEUE {
            continue;
        }
        if c.state == "Transferred" && fully_dependency_blocked(tx, service, &w)? {
            // Holds remain independently owned. A yield is not a user pause or a Continue.
            if service
                .evaluate(
                    tx.runs(),
                    &root(&w),
                    &Target::Root,
                    ActivationPurpose::Planning,
                )?
                .eligible
            {
                service.request_allocation_yield(tx, &c.root_id)?;
                change(tx, &c, "Yielding", &actor)?;
                changed += 1;
            }
        } else if c.state == "Yielding" && quiesced(tx, &c.root_id)? {
            change(tx, &c, "Yielded", &actor)?;
            super::items::bump(tx, project, std::slice::from_ref(&c.queue_key)).map_err(
                |f| match f {
                    crate::services::sessions::Failure::Storage(e) => e,
                    _ => StorageError::InvalidRecord("allocation queue".into()),
                },
            )?;
            changed += 1;
        } else if c.state == "Transferred" {
            let statuses = tx.work().statuses(project)?;
            let ended = w
                .status_id
                .as_ref()
                .and_then(|id| super::workflow::semantic_of(&statuses, id))
                .is_some_and(super::workflow::finished);
            if ended && quiesced(tx, &c.root_id)? {
                change(tx, &c, "Released", &actor)?;
                changed += 1;
            }
        }
    }
    Ok(changed)
}
/// Supervisor sweep covers every project independently of any UI or allocation invocation.
pub fn reconcile(
    tx: &mut dyn WriteTransaction,
    service: &ControlService,
) -> Result<usize, StorageError> {
    let mut after = None;
    let mut changed = 0;
    loop {
        let page = tx.projects().projects(
            &ProjectFilter {
                only: None,
                lifecycles: vec![ACTIVE.into()],
            },
            after.as_ref(),
            PAGE,
        )?;
        for p in &page {
            changed += reconcile_project(tx, service, &p.id)?;
        }
        if page.len() < usize::from(PAGE) {
            return Ok(changed);
        }
        after = page.last().map(|p| ProjectPosition {
            created_at_ms: p.created_at_ms,
            id: p.id.clone(),
        });
    }
}
