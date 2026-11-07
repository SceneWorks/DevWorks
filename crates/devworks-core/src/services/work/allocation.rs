//! Atomic work reservations, separate from activation and assignment ownership (WORK-09/10).
//! The gateway supplies request replay; every allocation and handoff runs on its write owner.
use super::{
    R, WorkService, conflict, event, invalid, open_project, stale, visible_feature, visible_work,
};
use crate::context::InvocationContext;
use crate::operations::next_work as ops;
use crate::ports::IdKind;
use crate::ports::storage::*;
use crate::protocol::records::{
    AllocationClaimRecord, AllocationOutcome, AllocationRecovery, AllocationSelection, ClaimRecord,
    CriterionState, LeaseState, WorkItemType, WorkKind,
};
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
#[derive(Clone, serde::Serialize)]
struct Filters {
    repository_id: Option<Id>,
    task_type: Option<WorkItemType>,
    label: Option<String>,
    another: bool,
    idempotency_key: Option<String>,
}

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
fn selection(
    tx: &mut dyn WriteTransaction,
    c: &StoredAllocationClaim,
    previous: &[StoredAllocationClaim],
    reused: bool,
    recovered: bool,
) -> R<AllocationSelection> {
    let w = tx
        .work()
        .work(&c.work_id)?
        .ok_or_else(|| super::not_found("work", &c.work_id))?;
    let work = super::items::tx_record(tx, &w)?;
    let claim = wire(c, tx.now_ms());
    let outcome = if recovered {
        let before = previous
            .iter()
            .filter(|old| old.work_id == c.work_id && old.id != c.id)
            .max_by_key(|old| old.fence);
        let brief = super::model::WorkBrief::of(&w)?;
        let mut after = 0;
        let mut last_activity = None;
        if brief.handoffs.is_empty() {
            loop {
                let page = tx.history().page(
                    &EventFilter::Aggregate {
                        kind: super::WORK_AGGREGATE.into(),
                        id: w.id.as_str().into(),
                    },
                    after,
                    PAGE,
                )?;
                for event in &page.events {
                    if event.kind == "WorkAllocated"
                        || before.is_some_and(|claim| claim.owner_id != event.actor)
                    {
                        continue;
                    }
                    let payload = serde_json::from_value::<crate::protocol::support::DynamicJson>(
                        event.payload.clone(),
                    )
                    .ok()
                    .map(|value| crate::protocol::records::AllocationActivityPayload { value });
                    let unavailable = payload.is_none();
                    last_activity = Some(crate::protocol::records::AllocationActivity {
                        event_id: event.id.0.clone(),
                        sequence: crate::services::sessions::rev(event.sequence),
                        actor_id: event.actor.clone(),
                        occurred_at_ms: counter(event.created_at_ms),
                        kind: crate::services::sessions::label(&event.kind, "work_change"),
                        work_id: w.id.clone(),
                        payload,
                        payload_unavailable_reason: unavailable.then(|| {
                            crate::protocol::support::BoundedText(
                                "payload_exceeds_protocol_bounds".into(),
                            )
                        }),
                    });
                }
                if page.events.len() < usize::from(PAGE) {
                    break;
                }
                after = page.events.last().expect("full page").sequence;
            }
        }
        AllocationOutcome::Recovered {
            claim,
            work: Some(work.clone()),
            reused,
            recovery: AllocationRecovery {
                previous_owner_id: before.map(|c| c.owner_id.clone()),
                previous_session_id: before.and_then(|c| c.session_id.clone()),
                links: work.links.clone(),
                latest_handoff: brief.handoffs.last().map(|h| h.item.clone()),
                last_activity,
                unmet_acceptance: crate::protocol::support::BoundedList(
                    work.acceptance
                        .0
                        .iter()
                        .filter(|criterion| !matches!(criterion.state, CriterionState::Met))
                        .cloned()
                        .collect(),
                ),
            },
        }
    } else {
        AllocationOutcome::Claimed {
            claim,
            work: Some(work),
            reused,
        }
    };
    Ok(AllocationSelection::Current(outcome))
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
    if let Some(assignment) = tx.runs().live_assignment_for_work(&RootId(w.id.clone()))? {
        if assignment.work_id != assignment.root_id || w.parent_feature_id.is_some() {
            return Ok(true);
        }
        if let Some(execution) = assignment.execution_id
            && tx
                .runs()
                .activation(&execution)?
                .is_some_and(|a| a.phase != ActivationPhase::Stopped)
        {
            return Ok(true);
        }
    }
    Ok(false)
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
        if let Some(assignment) = tx.runs().live_assignment_for_work(&RootId(w.id.clone()))?
            && assignment.root_id == r.id
            && !service
                .evaluate(
                    tx.runs(),
                    &r,
                    &Target::Assignment(assignment.id),
                    ActivationPurpose::Implementation,
                )?
                .eligible
        {
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
    if matches!(unit.readiness, Readiness::Blocked { .. }) {
        return Ok(false);
    }
    if super::membership::needs_root_activation(tx, &r.id)? {
        return Ok(true);
    }
    let (_, tasks) = super::queue::queue(tx.work(), &w.project_id, Some(&w.id))?;
    if tasks.is_empty() {
        if super::membership::awaiting_outgoing_handoff(tx, &r.id)? {
            return Ok(false);
        }
        if let PlanState::Approved { units, .. } = &plan
            && units
                .values()
                .any(|u| matches!(u.readiness, Readiness::Blocked { .. }))
        {
            return Ok(false);
        }
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
        let now = self
            .clock
            .as_ref()
            .ok_or_else(|| super::refuse(DomainErrorCode::ViewUnavailable, "host_clock", None))?
            .now_ms()
            .get();
        Ok((
            OperationResult::NextWorkInspectClaim {
                value: wire(&c, now),
            },
            c.revision,
        ))
    }
    pub(crate) fn allocate_project(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::ClaimProjectQueueInput,
        identity: &crate::context::ReceiptIdentity,
        schema: u32,
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
            schema,
            &Filters {
                repository_id: i.repository_id.clone(),
                task_type: i.task_type.clone(),
                label: i.label.as_ref().map(|s| s.0.trim().to_owned()),
                another: i.another.unwrap_or(false),
                idempotency_key: i.idempotency_key.as_ref().map(|s| s.0.clone()),
            },
        )
    }
    pub(crate) fn allocate_task(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::ClaimFeatureTaskInput,
        identity: &crate::context::ReceiptIdentity,
        schema: u32,
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
            schema,
            &Filters {
                repository_id: i.repository_id.clone(),
                task_type: i.task_type.clone(),
                label: i.label.as_ref().map(|s| s.0.trim().to_owned()),
                another: i.another.unwrap_or(false),
                idempotency_key: i.idempotency_key.as_ref().map(|s| s.0.clone()),
            },
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn allocate(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        project: &ProjectId,
        feature: Option<&StoredWork>,
        expected: i64,
        lease: i64,
        identity: &crate::context::ReceiptIdentity,
        schema: u32,
        filters: &Filters,
    ) -> R<DomainDecision> {
        open_project(tx, ctx, project)?;
        if let Some(session) = ctx.session_id() {
            let session = tx
                .sessions()
                .session(session)?
                .ok_or_else(|| super::not_found("session", session))?;
            if session.closed() {
                return Err(super::refuse(
                    DomainErrorCode::LeaseLost,
                    "owner_session",
                    Some(&session.id),
                ));
            }
            if session.project_id.as_ref().is_some_and(|id| id != project) {
                return Err(super::not_found("project", &project.0).into());
            }
        }
        if lease > MAX_LEASE_MS {
            return Err(invalid("lease_ms", None));
        }
        if matches!(ctx.principal_kind(), crate::context::PrincipalKind::Agent)
            && ctx.session_id().is_none()
        {
            return Err(invalid("owner_session", None));
        }
        let replay_identity = replay_identity(ctx, project, feature, lease, filters)?;
        if let Some(value) = replay(tx, ctx, replay_identity.as_ref())? {
            return Ok(DomainDecision::NoChange {
                result: if feature.is_some() {
                    OperationResult::NextWorkClaimFeatureTask { value }
                } else {
                    OperationResult::NextWorkClaimProjectQueue { value }
                },
            });
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

        if let Some(existing) = all.iter().find(|c| {
            !filters.another && c.queue_key == key && owned(c, ctx) && held.contains(&c.id)
        }) {
            let value = selection(tx, existing, &all, true, false)?;
            remember(tx, project, replay_identity.as_ref(), &value)?;
            return Ok(DomainDecision::NoChange {
                result: if feature.is_some() {
                    OperationResult::NextWorkClaimFeatureTask { value }
                } else {
                    OperationResult::NextWorkClaimProjectQueue { value }
                },
            });
        }
        let (_, mut candidates) = super::queue::queue(tx.work(), project, feature.map(|f| &f.id))?;
        // Retained Next behavior: recover interrupted in-progress work before new Ready work.
        candidates.sort_by_key(|c| c.semantic != super::workflow::IN_PROGRESS);
        for c in candidates {
            let w = c.work;
            let brief = super::model::WorkBrief::of(&w)?;
            if let Some(repository) = &filters.repository_id
                && !tx.work().work_repositories(&w.id)?.contains(repository)
            {
                continue;
            }
            if filters.task_type.as_ref().is_some_and(|kind| {
                brief.task_type.as_ref().is_none_or(|stored| {
                    std::mem::discriminant(stored) != std::mem::discriminant(kind)
                })
            }) {
                continue;
            }
            if filters.label.as_ref().is_some_and(|label| {
                !brief
                    .labels
                    .iter()
                    .any(|l| l.0.to_lowercase() == label.to_lowercase())
            }) {
                continue;
            }
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
            let fence = claims(tx.work(), project)?
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
            let value = selection(
                tx,
                &claim,
                &all,
                false,
                c.semantic == super::workflow::IN_PROGRESS,
            )?;
            remember(tx, project, replay_identity.as_ref(), &value)?;
            return Ok(DomainDecision::Applied {
                result: if feature.is_some() {
                    OperationResult::NextWorkClaimFeatureTask { value }
                } else {
                    OperationResult::NextWorkClaimProjectQueue { value }
                },
            });
        }
        if schema < 3 {
            return Err(Refusal::new(
                DomainErrorCode::DependencyBlocked,
                "no_eligible_work",
                Some(project.0.clone()),
            )
            .retry(crate::protocol::RetryAdvice::AfterBlockerClears)
            .into());
        }
        let value = AllocationSelection::Current(AllocationOutcome::NoEligible {
            reused: false,
            reason: crate::protocol::support::BoundedText("no_ready_work".into()),
            evidence: Some(
                Refusal::new(
                    DomainErrorCode::DependencyBlocked,
                    "no_eligible_work",
                    Some(project.0.clone()),
                )
                .fault(self.correlation()),
            ),
        });
        remember(tx, project, replay_identity.as_ref(), &value)?;
        Ok(DomainDecision::NoChange {
            result: if feature.is_some() {
                OperationResult::NextWorkClaimFeatureTask { value }
            } else {
                OperationResult::NextWorkClaimProjectQueue { value }
            },
        })
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
        if c.state != "Active" && !work_quiesced(tx, &c)? {
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
fn work_quiesced(
    tx: &mut dyn WriteTransaction,
    claim: &StoredAllocationClaim,
) -> Result<bool, StorageError> {
    if claim.work_id == claim.root_id.0 {
        return quiesced(tx, &claim.root_id);
    }
    let work = RootId(claim.work_id.clone());
    if tx.runs().live_activation(&work)?.is_some() {
        return Ok(false);
    }
    let assignments = root_assignments(tx.runs(), &claim.root_id, false)?;
    let ids: std::collections::HashSet<_> = assignments
        .iter()
        .filter(|a| a.work_id == work)
        .map(|a| a.id.clone())
        .collect();
    if assignments
        .iter()
        .any(|a| a.work_id == work && a.state.is_live())
    {
        return Ok(false);
    }
    let mut after = None;
    loop {
        let page = tx
            .resources()
            .processes(&ProcessFilter::Unsettled, after.as_ref(), PAGE)?;
        if page.iter().any(|p| {
            p.root_id.as_ref() == Some(&work)
                || p.assignment_id.as_ref().is_some_and(|a| ids.contains(a))
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
            .root_deliveries(&claim.root_id, after.as_ref(), PAGE)?;
        for id in &page {
            if let Some(entry) = tx.queue().entry(id)?
                && entry.state == QueueState::Claimed
            {
                let ours = if let Some(operation) = entry.operation_id {
                    tx.operations().operation(&operation)?.is_some_and(|op| {
                        op.assignment_id.as_ref().is_some_and(|a| ids.contains(a))
                    })
                } else {
                    entry.root_id.as_ref() == Some(&work)
                };
                if ours {
                    return Ok(false);
                }
            }
        }
        if page.len() < usize::from(PAGE) {
            break;
        }
        after = page.last().cloned();
    }
    Ok(true)
}
fn quiesced(tx: &mut dyn WriteTransaction, root: &RootId) -> Result<bool, StorageError> {
    if !tx.runs().inference_quiescent(root)? {
        return Ok(false);
    }

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
    if all
        .iter()
        .any(|c| c.work_id == w.id.0 && c.state == "Active")
    {
        let mut after = None;
        loop {
            let page = tx
                .runs()
                .project_roots(&w.project_id, after.as_ref(), PAGE)?;
            for other in &page {
                if *other != w.id && tx.runs().live_activation(other)?.is_some() {
                    return Err(conflict("project_execution_owned", &w.project_id.0));
                }
            }
            if page.len() < usize::from(PAGE) {
                break;
            }
            after = page.last().cloned();
        }
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
            if c.parent_feature_id != work.parent_feature_id
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
            if c.work_revision != work.revision {
                let mut refreshed = c.clone();
                refreshed.work_revision = work.revision;
                refreshed.revision += 1;
                tx.work().update_allocation_claim(&refreshed, c.revision)?;
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
    for c in claims(tx.work(), &w.project_id)?
        .into_iter()
        .filter(|c| c.work_id == *work)
    {
        if c.state != "Active" {
            if c.state != "Transferred" || session.is_none() {
                continue;
            }
            if let Some(old) = &c.execution_id
                && old != execution
                && tx
                    .runs()
                    .activation(old)?
                    .is_some_and(|a| a.phase != ActivationPhase::Stopped)
            {
                continue;
            }
        }
        let mut next = c.clone();
        next.work_revision = w.revision;
        next.parent_feature_id = w.parent_feature_id.clone();
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
    let mut changed = super::membership::reconcile_project(tx, service, project)?;
    for c in claims(tx.work(), project)? {
        if !live(&c) {
            continue;
        }
        let Some(w) = tx.work().work(&c.work_id)? else {
            continue;
        };
        if c.state == "Active" {
            if c.parent_feature_id != w.parent_feature_id {
                change(tx, &c, "Invalidated", &actor)?;
                changed += 1;
                continue;
            }
            if let Some(until) = owner_heartbeat(tx, &c)?
                && until > c.expires_at_ms
            {
                let mut renewed = c.clone();
                renewed.revision += 1;
                renewed.work_revision = w.revision;
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
        if c.state == "Transferred" && work_quiesced(tx, &c)? {
            change(tx, &c, "Released", &actor)?;
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

struct ReplayIdentity {
    key: String,
    intent_digest: String,
}
fn replay_identity(
    ctx: &InvocationContext,
    project: &ProjectId,
    feature: Option<&StoredWork>,
    lease: i64,
    filters: &Filters,
) -> R<Option<ReplayIdentity>> {
    let Some(key) = &filters.idempotency_key else {
        return Ok(None);
    };
    if key.chars().count() > 200 {
        return Err(invalid("idempotency_key", None));
    }
    let scope = json!({"actor":ctx.principal(),"session":ctx.session_id(),"ingress":ctx.ingress_namespace(),"project":project,"key":key});
    let intent = json!({"project":project,"feature":feature.map(|f|&f.id),"leaseMs":lease,"repository":filters.repository_id,"taskType":filters.task_type,"label":filters.label.as_ref().map(|l|l.to_lowercase()),"another":filters.another});
    let digest = |value: &serde_json::Value| {
        crate::jcs::canonicalize(value)
            .map(|s| crate::fingerprint::sha256_hex(s.as_bytes()))
            .map_err(|_| invalid("allocation_intent", None))
    };
    Ok(Some(ReplayIdentity {
        key: digest(&scope)?,
        intent_digest: digest(&intent)?,
    }))
}
fn remember(
    tx: &mut dyn WriteTransaction,
    project: &ProjectId,
    identity: Option<&ReplayIdentity>,
    selection: &AllocationSelection,
) -> R<()> {
    let Some(identity) = identity else {
        return Ok(());
    };
    let result = serde_json::to_value(selection).map_err(|_| invalid("allocation_result", None))?;
    if serde_json::to_vec(&result)
        .map_err(|_| invalid("allocation_result", None))?
        .len()
        > MAX_ALLOCATION_REPLAY_BYTES
    {
        return Err(invalid("allocation_replay_too_large", None));
    }
    let claim_id = match selection {
        AllocationSelection::Current(
            AllocationOutcome::Claimed { claim, .. } | AllocationOutcome::Recovered { claim, .. },
        ) => Some(claim.id.clone()),
        _ => None,
    };
    tx.work()
        .insert_allocation_replay(&StoredAllocationReplay {
            project_id: project.clone(),
            key: identity.key.clone(),
            intent_digest: identity.intent_digest.clone(),
            result,
            claim_id,
        })?;
    Ok(())
}
fn replay(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    identity: Option<&ReplayIdentity>,
) -> R<Option<AllocationSelection>> {
    let Some(identity) = identity else {
        return Ok(None);
    };
    let Some(saved) = tx.work().allocation_replay(&identity.key)? else {
        return Ok(None);
    };
    if saved.intent_digest != identity.intent_digest {
        return Err(conflict("allocation_intent_changed", ctx.invocation_id()));
    }
    let mut value: AllocationSelection = serde_json::from_value(saved.result)
        .map_err(|_| StorageError::InvalidRecord("stored allocation replay".into()))?;
    if let Some(claim_id) = saved.claim_id {
        let c = tx
            .work()
            .allocation_claim(&claim_id)?
            .ok_or_else(|| super::not_found("claim", &claim_id))?;
        let w = visible_work(tx.work(), ctx, &c.work_id)?;
        let statuses = tx.work().statuses(&w.project_id)?;
        let finished = w
            .status_id
            .as_ref()
            .and_then(|id| super::workflow::semantic_of(&statuses, id))
            .is_some_and(super::workflow::finished);
        if !(owned(&c, ctx) && reserved(tx, &c)?) && !finished {
            return Err(super::refuse(
                DomainErrorCode::LeaseLost,
                "claim",
                Some(&claim_id),
            ));
        }
        if let AllocationSelection::Current(
            AllocationOutcome::Claimed {
                claim,
                work,
                reused,
            }
            | AllocationOutcome::Recovered {
                claim,
                work,
                reused,
                ..
            },
        ) = &mut value
        {
            *claim = wire(&c, tx.now_ms());
            *work = Some(super::items::tx_record(tx, &w)?);
            *reused = true;
        }
    }
    if let AllocationSelection::Current(AllocationOutcome::NoEligible { reused, .. }) = &mut value {
        *reused = true;
    }
    Ok(Some(value))
}

/// A membership handoff retires only the old reservation once its fenced worker is quiescent.
/// The same durable assignment/session becomes the new execution owner; no second worker is made.
pub(crate) fn retire_handoff_claims(
    tx: &mut dyn WriteTransaction,
    work: &Id,
    actor: &ActorId,
) -> Result<(), StorageError> {
    let Some(w) = tx.work().work(work)? else {
        return Ok(());
    };
    for c in claims(tx.work(), &w.project_id)?
        .into_iter()
        .filter(|c| c.work_id == *work && live(c))
    {
        change(tx, &c, "Released", actor)?;
    }
    Ok(())
}

/// A moved worker creates a scheduling dependency on its new coordinator. Preserve the source
/// root's identity/holds, but do not let an empty or blocked source monopolize the project slot.
pub(crate) fn request_handoff_yield(
    tx: &mut dyn WriteTransaction,
    service: &ControlService,
    source: &RootId,
) -> Result<(), StorageError> {
    let Some(feature) = tx.work().work(&source.0)? else {
        return Ok(());
    };
    let (_, tasks) = super::queue::queue(tx.work(), &feature.project_id, Some(&feature.id))?;
    for task in tasks {
        if eligible(tx, service, &task.work, Some(&feature), true)? {
            return Ok(());
        }
    }
    let actor = tx.store().host_actor.clone();
    for claim in claims(tx.work(), &feature.project_id)?
        .into_iter()
        .filter(|c| {
            c.root_id == *source && c.queue_key == PROJECT_QUEUE && c.state == "Transferred"
        })
    {
        service.request_allocation_yield(tx, source)?;
        change(tx, &claim, "Yielding", &actor)?;
    }
    Ok(())
}
