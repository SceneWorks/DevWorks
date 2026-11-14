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
use crate::protocol::{
    ActorId, AssignmentId, DomainErrorCode, Id, ProjectId, RevisionDimension, RootId,
};
use crate::services::runs::activity::RootActivity;
use crate::services::runs::scope::{PlanState, Readiness};
use crate::services::runs::{ControlService, Gate, Target, control_of, root_assignments};
use crate::services::sessions::{Refusal, counter};
use serde_json::json;

pub const OPERATIONS: [&str; 5] = [
    "next_work.claim_project_queue",
    "next_work.claim_feature_task",
    "next_work.inspect_claim",
    "next_work.inspect_queue",
    "next_work.release_claim",
];
use crate::ports::storage::MAX_PAGE;
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
/// The project's live claims (Active, Transferred, Yielding): the only ones any allocation,
/// start or reconciliation acts on. Released and expired history is never read.
pub fn claims(
    read: &dyn WorkRead,
    project: &ProjectId,
) -> Result<Vec<StoredAllocationClaim>, StorageError> {
    read.live_allocation_claims(project)
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
                    MAX_PAGE,
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
                if page.events.len() < usize::from(MAX_PAGE) {
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
/// Why a queued item was not allocated. The run gates come from [`ControlService::evaluate`]'s
/// conditions; the others are the allocator's own (workflow status, caller filters, ownership).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Reason {
    Pause,
    Prerequisite,
    Plan,
    Readiness,
    Question,
    Hold,
    Archived,
    Assignment,
    Filter,
    Claimed,
    Running,
    Handoff,
    /// Not in a Ready or In Progress status.
    Status,
}
impl Reason {
    fn of(gate: Gate) -> Self {
        match gate {
            Gate::Archived => Self::Archived,
            Gate::Pause => Self::Pause,
            Gate::Hold => Self::Hold,
            Gate::Prerequisite => Self::Prerequisite,
            Gate::Question => Self::Question,
            Gate::Plan => Self::Plan,
            Gate::Readiness { .. } => Self::Readiness,
            Gate::Assignment => Self::Assignment,
        }
    }
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Prerequisite => "prerequisite",
            Self::Plan => "plan",
            Self::Readiness => "readiness",
            Self::Question => "question",
            Self::Hold => "hold",
            Self::Archived => "archived",
            Self::Assignment => "assignment",
            Self::Filter => "filter",
            Self::Claimed => "claimed",
            Self::Running => "running",
            Self::Handoff => "handoff",
            Self::Status => "no_ready_work",
        }
    }
}
/// The `NoEligible` reason: every distinct cause, comma-separated in a fixed order. Items that
/// are not Ready or In Progress count only when nothing else held work back.
pub(crate) fn no_eligible_reason(reasons: &std::collections::BTreeSet<Reason>) -> String {
    let causes: Vec<&str> = reasons
        .iter()
        .filter(|r| **r != Reason::Status)
        .map(|r| r.as_str())
        .collect();
    if causes.is_empty() {
        Reason::Status.as_str().to_owned()
    } else {
        causes.join(",")
    }
}
/// The first condition that holds `w`'s implementation, reading [`ControlService::evaluate`].
/// With `assessable`, readiness an assessment can establish is reported apart, not as a hold.
fn holds(
    tx: &mut dyn WriteTransaction,
    service: &ControlService,
    root: &WorkRoot,
    target: &Target,
) -> Result<(Option<Reason>, bool), StorageError> {
    let control = service.evaluate(tx.runs(), root, target, ActivationPurpose::Implementation)?;
    let mut needs_assessment = false;
    for c in &control.conditions {
        if c.gate == (Gate::Readiness { assessable: true }) {
            needs_assessment = true;
        } else {
            return Ok((Some(Reason::of(c.gate)), needs_assessment));
        }
    }
    Ok((None, needs_assessment))
}
/// Eligibility shared by both queue scopes. Every run gate (pause, holds, prerequisites,
/// blocking questions, plan, readiness, findings, repository scope) is the implementation
/// eligibility [`ControlService::evaluate`] derives; this adds only the workflow status and the
/// queue structure of a feature. Ownership is filtered separately, after these gates.
fn eligible(
    tx: &mut dyn WriteTransaction,
    service: &ControlService,
    w: &StoredWork,
    feature: Option<&StoredWork>,
    assessment: bool,
) -> Result<Result<(), Reason>, StorageError> {
    let statuses = tx.work().statuses(&w.project_id)?;
    if w.status_id
        .as_ref()
        .and_then(|id| super::workflow::semantic_of(&statuses, id))
        .is_none_or(|s| s != super::workflow::READY && s != super::workflow::IN_PROGRESS)
    {
        return Ok(Err(Reason::Status));
    }
    let (held, needs_assessment) = holds(tx, service, &root(w), &Target::Root)?;
    if let Some(reason) = held {
        return Ok(Err(reason));
    }
    if let Some(f) = feature {
        // The feature's coordinator already runs this card: its assignment must be eligible too.
        let r = root(f);
        if let Some(assignment) = tx.runs().live_assignment_for_work(&RootId(w.id.clone()))?
            && assignment.root_id == r.id
            && let Some(c) = service
                .evaluate(
                    tx.runs(),
                    &r,
                    &Target::Assignment(assignment.id),
                    ActivationPurpose::Implementation,
                )?
                .conditions
                .first()
        {
            return Ok(Err(Reason::of(c.gate)));
        }
        return Ok(if needs_assessment && !assessment {
            Err(Reason::Readiness)
        } else {
            Ok(())
        });
    }
    if !w.is_feature() {
        return Ok(Ok(()));
    }
    // A feature is allocated to assess (missing or stale readiness) or to run its cards.
    let r = root(w);
    let plan = service.scope().plan(tx.runs(), &r)?;
    let PlanState::Approved { units, .. } = &plan else {
        return Ok(Err(Reason::Plan));
    };
    if super::membership::needs_root_activation(tx, &r.id)? {
        return Ok(Ok(()));
    }
    let (_, tasks) = super::queue::queue(tx.work(), &w.project_id, Some(&w.id))?;
    if tasks.is_empty() {
        if super::membership::awaiting_outgoing_handoff(tx, &r.id)? {
            return Ok(Err(Reason::Handoff));
        }
        if units
            .values()
            .any(|u| matches!(u.readiness, Readiness::Blocked { .. }))
        {
            return Ok(Err(Reason::Readiness));
        }
        return Ok(Ok(()));
    }
    let mut first = Reason::Status;
    for candidate in tasks {
        match eligible(tx, service, &candidate.work, Some(w), true)? {
            Ok(()) => return Ok(Ok(())),
            Err(reason) if first == Reason::Status => first = reason,
            Err(_) => {}
        }
    }
    Ok(Err(first))
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
        let (_, mut candidates) =
            super::queue::actionable(tx.work(), project, feature.map(|f| &f.id))?;
        // Retained Next behavior: recover interrupted in-progress work before new Ready work.
        candidates.sort_by_key(|c| c.semantic != super::workflow::IN_PROGRESS);
        // Why each skipped candidate was skipped, reported when nothing is eligible (WORK-10).
        let mut reasons = std::collections::BTreeSet::new();
        for c in candidates {
            let w = c.work;
            if c.semantic != super::workflow::READY && c.semantic != super::workflow::IN_PROGRESS {
                reasons.insert(Reason::Status);
                continue;
            }
            let brief = super::model::WorkBrief::of(&w)?;
            let filtered = if let Some(repository) = &filters.repository_id {
                !tx.work().work_repositories(&w.id)?.contains(repository)
            } else {
                false
            };
            if filtered
                || filters.task_type.as_ref().is_some_and(|kind| {
                    brief.task_type.as_ref().is_none_or(|stored| {
                        std::mem::discriminant(stored) != std::mem::discriminant(kind)
                    })
                })
                || filters.label.as_ref().is_some_and(|label| {
                    !brief
                        .labels
                        .iter()
                        .any(|l| l.0.to_lowercase() == label.to_lowercase())
                })
            {
                reasons.insert(Reason::Filter);
                continue;
            }
            if let Err(reason) = eligible(tx, &self.runs, &w, feature, false)? {
                reasons.insert(reason);
                continue;
            }
            if has_execution(tx, &w)? {
                reasons.insert(Reason::Running);
                continue;
            }
            if all
                .iter()
                .any(|c| c.work_id == w.id && held.contains(&c.id))
            {
                reasons.insert(Reason::Claimed);
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
            let previous = tx.work().latest_allocation_claim(&w.id)?;
            let fence = previous.as_ref().map_or(0, |c| c.fence) + 1;
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
                previous.as_slice(),
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
            reason: crate::protocol::support::BoundedText(no_eligible_reason(&reasons)),
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
/// Whether the claimed card's own work inside its root has quiesced: no activation of the card,
/// no live assignment for it, and none of its process trees or claimed deliveries still running.
fn work_quiesced(
    tx: &mut dyn WriteTransaction,
    claim: &StoredAllocationClaim,
) -> Result<bool, StorageError> {
    if claim.work_id == claim.root_id.0 {
        return quiesced(tx, &claim.root_id);
    }
    let work = RootId(claim.work_id.clone());
    let card = RootActivity::of(tx, &work)?;
    if card.activation.is_some() || !card.processes.is_empty() {
        return Ok(false);
    }
    let assignments = root_assignments(tx.runs(), &claim.root_id, false)?;
    if assignments
        .iter()
        .any(|a| a.work_id == work && a.state.is_live())
    {
        return Ok(false);
    }
    let ids: std::collections::HashSet<_> = assignments
        .iter()
        .filter(|a| a.work_id == work)
        .map(|a| a.id.clone())
        .collect();
    let root = RootActivity::of(tx, &claim.root_id)?;
    let ours = |assignment: Option<&AssignmentId>| assignment.is_some_and(|a| ids.contains(a));
    Ok(!root
        .processes
        .iter()
        .any(|p| ours(p.assignment_id.as_ref()))
        && !root.deliveries.iter().any(|d| {
            d.claimed
                && if d.operation {
                    ours(d.assignment.as_ref())
                } else {
                    d.work.as_ref() == Some(&work)
                }
        }))
}
/// Whether nothing of `root` still runs: no activation (whatever its owner), nothing admitted,
/// no process tree, no open inference attempt, and the same for its linked cards and for the
/// cards its assignments work on.
fn quiesced(tx: &mut dyn WriteTransaction, root: &RootId) -> Result<bool, StorageError> {
    if !tx.runs().inference_quiescent(root)? {
        return Ok(false);
    }
    let activity = RootActivity::of(tx, root)?;
    if activity.activation.is_some() || !activity.quiesced() {
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
        if a.work_id != *root && !RootActivity::of(tx, &a.work_id)?.quiesced() {
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
                .project_roots(&w.project_id, after.as_ref(), MAX_PAGE)?;
            for other in &page {
                if *other != w.id && tx.runs().live_activation(other)?.is_some() {
                    return Err(conflict("project_execution_owned", &w.project_id.0));
                }
            }
            if page.len() < usize::from(MAX_PAGE) {
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
            if eligible(tx, service, &work, None, false)?.is_err() {
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
        if eligible(tx, service, &c.work, Some(w), true)?.is_ok() {
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
            MAX_PAGE,
        )?;
        for p in &page {
            changed += reconcile_project(tx, service, &p.id)?;
        }
        if page.len() < usize::from(MAX_PAGE) {
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
        if eligible(tx, service, &task.work, Some(&feature), true)?.is_ok() {
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
