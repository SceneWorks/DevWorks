//! The run-control substrate later items build on: the activation supervisor's claim, renewal,
//! stop acknowledgment and completion; `BeginDispatch` (the one atomic eligibility recheck every
//! provider request and tool call passes); root and assignment identity; plan adoption at safe
//! boundaries; redirect acknowledgment and incorporation; the one continuation turn per session;
//! late-result acceptance; and the immediate stop of an archived project.
//!
//! Every function runs inside the caller's write transaction on the sole write owner, so it is
//! ordered against Pause, Stop, Redirect and Continue: whichever commits first wins, and the
//! loser observes it. Nothing here holds a transaction across a network request or launch.
use super::{
    ACTIVATION_AGGREGATE, ASSIGNMENT_AGGREGATE, ControlService, Eligibility, HoldSubject,
    INSTRUCTION_AGGREGATE, Target, active_blockers, clear_blocker, control_of, coordinator,
    ensure_control, is_run_root, raise_blocker, record, root_assignments, save_control,
};
use crate::ports::IdKind;
use crate::ports::storage::{
    ActivationFilter, ActivationOutcome, ActivationPhase, ActivationPurpose, AssignmentState,
    BlockerFilter, DesiredExecution, HoldCause, InstructionStage, StopCause, StorageError,
    StoredActivation, StoredAssignment, StoredInstruction, StoredTurn, WorkRoot, WriteTransaction,
};
use crate::protocol::{ActorId, AssignmentId, Id, OperationId, ProjectId, RootId};
use serde_json::{Value, json};

/// Why a supervisor, broker or delegation call was refused. Nothing was written.
#[derive(Clone, Debug)]
pub enum Fenced {
    NotFound(&'static str),
    /// The caller is not the current owner (stale claim epoch, earlier boot, superseded
    /// generation, or an assignment bound to another activation).
    LeaseLost(String),
    /// A stop was requested: no new dispatch; acknowledge and finish instead.
    StopRequested,
    /// Current eligibility refuses it, with every reason.
    Blocked(Box<Eligibility>),
    Invalid(String),
}
/// The outcome of a fenced substrate call.
pub type Gate<T> = Result<Result<T, Fenced>, StorageError>;

/// One dispatch admission request (`BeginDispatch`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispatchRequest {
    pub root: RootId,
    pub execution_id: Id,
    pub owner_epoch: i64,
    pub target: Target,
    pub purpose: ActivationPurpose,
}

/// The revisions a dispatch was admitted under; the caller records them on its operation
/// attempt, so a later result is checked against them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admission {
    pub control_revision: i64,
    pub generation: i64,
    pub owner_epoch: i64,
    pub plan_revision: Option<i64>,
    pub readiness_revision: Option<i64>,
    pub instruction_revision: i64,
    pub stop_epoch: i64,
}

/// Why an activation's work failed, by category (CONTROL-06): transient causes retry with
/// bounded deterministic backoff, capacity and rate limits wait visibly until their ready time,
/// and a deterministic failure stops and escalates once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivationFailure {
    /// Lost supervisor, connectivity loss, provider exhaustion: retry with backoff.
    Transient(String),
    RateLimited {
        until_ms: i64,
        detail: String,
    },
    Capacity {
        until_ms: i64,
        detail: String,
    },
    /// Repeating it cannot help: stop and raise one actionable blocker.
    Deterministic(String),
}

/// A new assignment of a root (root and assignment identity, AGENT-05/14).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewRunAssignment {
    pub root: RootId,
    /// The work it is for: the root itself, or work in the root feature's hierarchy.
    pub work: RootId,
    pub parent: Option<AssignmentId>,
    /// Its fresh session (a session of this root).
    pub session: Id,
    /// The approved scope unit it implements; none for planning or the coordinator.
    pub scope_key: Option<String>,
}

/// What adopting the active approved plan did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanAdoption {
    pub revision: Option<i64>,
    /// Assignments whose bound scope changed or left the plan (fenced).
    pub superseded: Vec<AssignmentId>,
    /// Assignments whose bound scope is unchanged (they keep running).
    pub kept: Vec<AssignmentId>,
}

/// The coordinator's recorded disposition of a redirect.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Disposition {
    pub summary: String,
    /// Incompatible assignments the redirect supersedes; the rest keep running.
    pub superseded: Vec<AssignmentId>,
    /// A pending plan revision the redirect proposed (it authorizes nothing until approved).
    pub pending_plan_revision: Option<i64>,
}

/// A result an owner submits as current work evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultClaim {
    pub assignment: AssignmentId,
    pub execution_id: Id,
    pub owner_epoch: i64,
    /// The plan revision the work was dispatched under.
    pub plan_revision: Option<i64>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResultAcceptance {
    Current,
    /// Retained as attributable history; it cannot change current acceptance.
    Stale(String),
}

fn host(tx: &dyn WriteTransaction) -> ActorId {
    tx.store().host_actor.clone()
}
fn boot(tx: &dyn WriteTransaction) -> Result<Id, StorageError> {
    tx.store()
        .boot_id
        .clone()
        .ok_or_else(|| StorageError::IncompatibleSchema("store is read-only".into()))
}
/// The root `root` as the substrate drives it: a run root, or a card linked to a feature that
/// keeps run controls of its own (an issue linked while its agent was running continues and
/// finishes under its own activation, held by its feature's gates; an archival stops it).
fn work_root(
    tx: &mut dyn WriteTransaction,
    root: &RootId,
) -> Result<Option<WorkRoot>, StorageError> {
    let Some(w) = tx.sessions().work_root(root)? else {
        return Ok(None);
    };
    if is_run_root(&w) || tx.runs().root_control(&w.id)?.is_some() {
        return Ok(Some(w));
    }
    Ok(None)
}

impl ControlService {
    /// The live activation `execution` if the caller still owns it under `epoch` in this boot and
    /// it is its root's current generation, with its root.
    fn owned(
        &self,
        tx: &mut dyn WriteTransaction,
        execution: &Id,
        epoch: i64,
    ) -> Gate<(WorkRoot, StoredActivation)> {
        let Some(a) = tx.runs().activation(execution)? else {
            return Ok(Err(Fenced::NotFound("activation")));
        };
        let current_boot = boot(tx)?;
        let control = control_of(tx.runs(), &a.root_id)?;
        if !a.phase.is_live()
            || a.owner_epoch != epoch
            || a.owner.is_none()
            || a.boot_id != current_boot
            || control.generation != a.generation
        {
            return Ok(Err(Fenced::LeaseLost(format!(
                "activation {} is not owned under epoch {epoch}",
                a.id.as_str()
            ))));
        }
        let Some(root) = work_root(tx, &a.root_id)? else {
            return Ok(Err(Fenced::NotFound("root")));
        };
        Ok(Ok((root, a)))
    }

    fn update(
        &self,
        tx: &mut dyn WriteTransaction,
        root: &WorkRoot,
        before: &StoredActivation,
        mut after: StoredActivation,
        kind: &str,
        payload: Value,
    ) -> Result<StoredActivation, StorageError> {
        after.revision = before.revision + 1;
        tx.runs().update_activation(&after, before.revision)?;
        let actor = host(tx);
        record(
            tx,
            &actor,
            root,
            None,
            ACTIVATION_AGGREGATE,
            after.id.as_str(),
            after.revision,
            kind,
            payload,
        )?;
        Ok(after)
    }

    /// Clears the root's capacity and rate-limit waits whose ready time has passed (capacity
    /// release triggers reevaluation automatically; it never clears a manual pause).
    fn sweep_waits(
        &self,
        tx: &mut dyn WriteTransaction,
        root: &WorkRoot,
    ) -> Result<(), StorageError> {
        let now = tx.now_ms();
        let actor = host(tx);
        for b in active_blockers(tx.runs(), &BlockerFilter::RootLevel(root.id.clone()))? {
            let ready = b.clearance.get("readyAtMs").and_then(Value::as_i64);
            if b.cause == HoldCause::Capacity && ready.is_some_and(|t| t <= now) {
                clear_blocker(
                    tx,
                    &actor,
                    root,
                    &b,
                    json!({"readyAtMs": ready, "observedAtMs": now}),
                )?;
            }
        }
        Ok(())
    }

    /// Clears every capacity or rate-limit wait (on any root) whose ready time has passed, and
    /// returns the roots it cleared one on: their queued work may be eligible again, so the
    /// caller wakes the dispatcher.
    pub fn sweep_expired_waits(
        &self,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Vec<RootId>, StorageError> {
        let now = tx.now_ms();
        let actor = host(tx);
        let mut cleared: Vec<RootId> = Vec::new();
        for b in active_blockers(
            tx.runs(),
            &BlockerFilter::RootLevelCause(HoldCause::Capacity),
        )? {
            let ready = b.clearance.get("readyAtMs").and_then(Value::as_i64);
            let Some(root_id) = b.root_id.clone() else {
                continue;
            };
            if !ready.is_some_and(|t| t <= now) {
                continue;
            }
            let Some(root) = work_root(tx, &root_id)? else {
                continue;
            };
            clear_blocker(
                tx,
                &actor,
                &root,
                &b,
                json!({"readyAtMs": ready, "observedAtMs": now}),
            )?;
            if !cleared.contains(&root_id) {
                cleared.push(root_id);
            }
        }
        Ok(cleared)
    }

    /// Claims the oldest pending activation of this boot that is ready and whose root permits
    /// it to resume, under a fresh claim epoch and lease. A pending activation of a superseded
    /// generation is stopped (`Prevented`) instead. Paused, held or stopped roots are never
    /// claimed: a lease alone is not authority to run.
    pub fn claim_activation(
        &self,
        tx: &mut dyn WriteTransaction,
        owner: &Id,
    ) -> Result<Option<StoredActivation>, StorageError> {
        let current_boot = boot(tx)?;
        let now = tx.now_ms();
        let mut after: Option<Id> = None;
        loop {
            let page = tx.runs().activations(
                &ActivationFilter::Claimable {
                    boot: current_boot.clone(),
                    now_ms: now,
                },
                after.as_ref(),
                50,
            )?;
            let more = page.len() == 50;
            for a in &page {
                // Each candidate in its own savepoint: one root's storage or record failure is
                // rolled back and skipped, never blocking every other root's claims.
                let sp = tx.savepoint()?;
                match self.try_claim(tx, a, owner, now) {
                    Ok(Some(claimed)) => {
                        tx.release(sp)?;
                        return Ok(Some(claimed));
                    }
                    Ok(None) => tx.release(sp)?,
                    Err(_) => tx.rollback_to(sp)?,
                }
            }
            if !more {
                return Ok(None);
            }
            after = page.last().map(|a| a.id.clone());
        }
    }

    /// One claim candidate: `Some` when claimed, `None` when it was skipped (superseded,
    /// prevented or not eligible).
    fn try_claim(
        &self,
        tx: &mut dyn WriteTransaction,
        a: &StoredActivation,
        owner: &Id,
        now: i64,
    ) -> Result<Option<StoredActivation>, StorageError> {
        let Some(root) = work_root(tx, &a.root_id)? else {
            return Ok(None);
        };
        let control = control_of(tx.runs(), &root.id)?;
        if control.generation != a.generation {
            let mut stopped = a.clone();
            stopped.phase = ActivationPhase::Stopped;
            stopped.stopped_at_ms = Some(now);
            stopped.stop_cause = Some(StopCause::Superseded);
            stopped.outcome = Some(ActivationOutcome::Prevented);
            self.update(
                tx,
                &root,
                a,
                stopped,
                "ActivationPrevented",
                json!({"cause": "Superseded"}),
            )?;
            return Ok(None);
        }
        self.sweep_waits(tx, &root)?;
        if !self
            .evaluate(tx.runs(), &root, &Target::Root, ActivationPurpose::Resume)?
            .eligible
        {
            return Ok(None);
        }
        let mut claimed = a.clone();
        claimed.phase = ActivationPhase::Claimed;
        claimed.owner = Some(owner.clone());
        claimed.lease_until_ms = Some(now + self.config.lease_ms);
        claimed.owner_epoch += 1;
        let claimed = self.update(
            tx,
            &root,
            a,
            claimed,
            "ActivationClaimed",
            json!({"owner": owner.as_str(), "ownerEpoch": a.owner_epoch + 1, "attempt": a.attempts}),
        )?;
        Ok(Some(claimed))
    }

    /// Marks a claimed activation `Running` (it launched), adopts the active approved plan and
    /// rebinds the root's live assignments to it (a new owner generation each), so results of
    /// the previous activation are stale.
    pub fn start_activation(
        &self,
        tx: &mut dyn WriteTransaction,
        execution: &Id,
        epoch: i64,
    ) -> Gate<StoredActivation> {
        let (root, a) = match self.owned(tx, execution, epoch)? {
            Ok(x) => x,
            Err(f) => return Ok(Err(f)),
        };
        match a.phase {
            ActivationPhase::Claimed => {}
            ActivationPhase::Stopping => return Ok(Err(Fenced::StopRequested)),
            _ => {
                return Ok(Err(Fenced::Invalid(
                    "only a claimed activation starts".into(),
                )));
            }
        }
        let actor = host(tx);
        self.adopt_plan(tx, &actor, &root)?;
        for asg in root_assignments(tx.runs(), &root.id, true)? {
            if asg.execution_id.as_ref() == Some(&a.id) {
                continue;
            }
            let mut bound = asg.clone();
            bound.execution_id = Some(a.id.clone());
            bound.generation += 1;
            bound.revision += 1;
            tx.runs().update_assignment(&bound, asg.revision)?;
        }
        let mut running = a.clone();
        running.phase = ActivationPhase::Running;
        Ok(Ok(self.update(
            tx,
            &root,
            &a,
            running,
            "ActivationRunning",
            json!({}),
        )?))
    }

    /// Renews the owner's lease. The returned record shows a requested stop (`Stopping`), which
    /// the owner must honour at its next safe boundary.
    pub fn renew_activation(
        &self,
        tx: &mut dyn WriteTransaction,
        execution: &Id,
        epoch: i64,
    ) -> Gate<StoredActivation> {
        let (root, a) = match self.owned(tx, execution, epoch)? {
            Ok(x) => x,
            Err(f) => return Ok(Err(f)),
        };
        // A linked card's retained activation runs under its feature's controls: once they hold
        // it, the lease is not extended; the activation is asked to stop instead (it finishes at
        // its next safe boundary, or its lease runs out).
        if a.phase != ActivationPhase::Stopping
            && let Some(why) = super::inherited_hold(tx.runs(), &root)?
        {
            let actor = tx.store().host_actor.clone();
            let stopping = self
                .request_stop(tx, &actor, &root, StopCause::Pause)?
                .unwrap_or(a);
            let _ = why;
            return Ok(Ok(stopping));
        }
        let mut renewed = a.clone();
        renewed.lease_until_ms = Some(tx.now_ms() + self.config.lease_ms);
        renewed.revision = a.revision + 1;
        tx.runs().update_activation(&renewed, a.revision)?;
        Ok(Ok(renewed))
    }

    /// Records that the owner reached a safe boundary after a requested stop and admits nothing
    /// more (the acknowledged stage); the activation stays `Stopping` until it finishes.
    pub fn acknowledge_stop(
        &self,
        tx: &mut dyn WriteTransaction,
        execution: &Id,
        epoch: i64,
    ) -> Gate<StoredActivation> {
        let (root, a) = match self.owned(tx, execution, epoch)? {
            Ok(x) => x,
            Err(f) => return Ok(Err(f)),
        };
        if a.phase != ActivationPhase::Stopping {
            return Ok(Err(Fenced::Invalid("no stop was requested".into())));
        }
        if a.stop_acknowledged_at_ms.is_some() {
            return Ok(Ok(a));
        }
        let mut acked = a.clone();
        acked.stop_acknowledged_at_ms = Some(tx.now_ms());
        Ok(Ok(self.update(
            tx,
            &root,
            &a,
            acked,
            "ActivationStopAcknowledged",
            json!({"cause": a.stop_cause.map(StopCause::as_str)}),
        )?))
    }

    /// Ends the activation (`Stopped`) with its attributed outcome. Its active assignments go
    /// dormant (`Waiting`, checkpoint kept). When Continue was accepted while it was stopping
    /// and the root still permits it, the next activation is recorded now.
    pub fn finish_activation(
        &self,
        tx: &mut dyn WriteTransaction,
        execution: &Id,
        epoch: i64,
        outcome: ActivationOutcome,
        detail: Option<&str>,
    ) -> Gate<StoredActivation> {
        let (root, a) = match self.owned(tx, execution, epoch)? {
            Ok(x) => x,
            Err(f) => return Ok(Err(f)),
        };
        Ok(Ok(self.stop_activation(tx, &root, &a, outcome, detail)?))
    }

    pub(crate) fn stop_activation(
        &self,
        tx: &mut dyn WriteTransaction,
        root: &WorkRoot,
        a: &StoredActivation,
        outcome: ActivationOutcome,
        detail: Option<&str>,
    ) -> Result<StoredActivation, StorageError> {
        let now = tx.now_ms();
        let mut stopped = a.clone();
        stopped.phase = ActivationPhase::Stopped;
        stopped.outcome = Some(outcome);
        stopped.stopped_at_ms = Some(now);
        stopped.owner = None;
        stopped.lease_until_ms = None;
        stopped.detail = detail.map(|d| d.chars().take(4096).collect());
        let stopped = self.update(
            tx,
            root,
            a,
            stopped,
            "ActivationStopped",
            json!({"outcome": outcome.as_str(), "detail": detail, "cause": a.stop_cause.map(StopCause::as_str)}),
        )?;
        for asg in root_assignments(tx.runs(), &root.id, true)? {
            if asg.state == AssignmentState::Active && asg.execution_id.as_ref() == Some(&a.id) {
                let mut dormant = asg.clone();
                dormant.state = AssignmentState::Waiting;
                dormant.revision += 1;
                tx.runs().update_assignment(&dormant, asg.revision)?;
            }
        }
        let control = control_of(tx.runs(), &root.id)?;
        if control.reactivate && control.desired == DesiredExecution::Enabled {
            let manual = active_blockers(tx.runs(), &BlockerFilter::RootLevel(root.id.clone()))?
                .iter()
                .any(|b| b.cause.manual_resume());
            if !manual {
                let actor = host(tx);
                let plan = self.scope.plan(tx.runs(), root)?.approved_revision();
                self.new_activation(
                    tx,
                    &actor,
                    root,
                    &control,
                    ActivationPurpose::Resume,
                    plan,
                    "Continue",
                )?;
            }
        }
        Ok(stopped)
    }

    /// Records a categorized failure of the activation's work (CONTROL-06).
    pub fn report_failure(
        &self,
        tx: &mut dyn WriteTransaction,
        execution: &Id,
        epoch: i64,
        failure: ActivationFailure,
    ) -> Gate<StoredActivation> {
        let (root, a) = match self.owned(tx, execution, epoch)? {
            Ok(x) => x,
            Err(f) => return Ok(Err(f)),
        };
        Ok(Ok(self.fail_activation(tx, &root, &a, failure)?))
    }

    pub(crate) fn fail_activation(
        &self,
        tx: &mut dyn WriteTransaction,
        root: &WorkRoot,
        a: &StoredActivation,
        failure: ActivationFailure,
    ) -> Result<StoredActivation, StorageError> {
        let now = tx.now_ms();
        if a.phase == ActivationPhase::Stopping {
            // A stop was requested: the failure ends it rather than scheduling a retry.
            let detail = format!("{failure:?}");
            return self.stop_activation(
                tx,
                root,
                a,
                ActivationOutcome::Interrupted,
                Some(&detail),
            );
        }
        let requeue = |a: &StoredActivation, ready_at: i64, detail: String, attempts: i64| {
            let mut p = a.clone();
            p.phase = ActivationPhase::Pending;
            p.owner = None;
            p.lease_until_ms = None;
            p.ready_at_ms = ready_at;
            p.detail = Some(detail.chars().take(4096).collect());
            p.attempts = attempts;
            p
        };
        let actor = host(tx);
        let kind = if matches!(failure, ActivationFailure::RateLimited { .. }) {
            "RateLimited"
        } else {
            "Capacity"
        };
        match failure {
            ActivationFailure::Transient(detail) if a.attempts + 1 < self.config.max_attempts => {
                let attempt = a.attempts + 1;
                let delay = self.config.backoff_ms(attempt);
                let p = requeue(a, now + delay, detail.clone(), attempt);
                self.update(
                    tx,
                    root,
                    a,
                    p,
                    "ActivationRetryScheduled",
                    json!({"category": "Transient", "attempt": attempt, "delayMs": delay, "detail": detail}),
                )
            }
            ActivationFailure::Transient(detail) => self.escalate(
                tx,
                root,
                a,
                &format!(
                    "retries exhausted after {} attempts: {detail}",
                    self.config.max_attempts
                ),
            ),
            ActivationFailure::RateLimited { until_ms, detail }
            | ActivationFailure::Capacity { until_ms, detail } => {
                raise_blocker(
                    tx,
                    &actor,
                    root,
                    HoldSubject::default(),
                    HoldCause::Capacity,
                    a.id.as_str(),
                    until_ms,
                    &format!("{kind}: {detail}"),
                    json!({"kind": kind, "readyAtMs": until_ms}),
                )?;
                let p = requeue(a, until_ms, detail.clone(), a.attempts);
                self.update(
                    tx,
                    root,
                    a,
                    p,
                    "ActivationWaiting",
                    json!({"category": kind, "readyAtMs": until_ms, "detail": detail}),
                )
            }
            ActivationFailure::Deterministic(detail) => self.escalate(tx, root, a, &detail),
        }
    }

    /// Stops the activation `Failed` and raises one actionable blocker for a person or the
    /// owning service to resolve; the root's pauses are untouched.
    fn escalate(
        &self,
        tx: &mut dyn WriteTransaction,
        root: &WorkRoot,
        a: &StoredActivation,
        detail: &str,
    ) -> Result<StoredActivation, StorageError> {
        let actor = host(tx);
        raise_blocker(
            tx,
            &actor,
            root,
            HoldSubject::default(),
            HoldCause::Policy,
            a.id.as_str(),
            1,
            &format!("Escalated: {detail}"),
            json!({"kind": "Escalation", "executionId": a.id.as_str()}),
        )?;
        self.stop_activation(tx, root, a, ActivationOutcome::Failed, Some(detail))
    }

    /// `BeginDispatch`: atomically rechecks the owner, the root controls, the active approved
    /// scope, readiness, the assignment's and operation's own blockers and the redirect fence
    /// before a provider request or tool call is admitted. It first adopts a newly approved
    /// plan (this is a safe boundary). A pause, stop or redirect committed earlier refuses it;
    /// one committed later finds it admitted and in flight.
    pub fn begin_dispatch(
        &self,
        tx: &mut dyn WriteTransaction,
        request: &DispatchRequest,
    ) -> Gate<Admission> {
        let (root, a) = match self.owned(tx, &request.execution_id, request.owner_epoch)? {
            Ok(x) => x,
            Err(f) => return Ok(Err(f)),
        };
        if a.root_id != request.root {
            return Ok(Err(Fenced::Invalid(
                "the activation belongs to another root".into(),
            )));
        }
        match a.phase {
            ActivationPhase::Running => {}
            ActivationPhase::Stopping => return Ok(Err(Fenced::StopRequested)),
            _ => return Ok(Err(Fenced::Invalid("the activation is not running".into()))),
        }
        let actor = host(tx);
        self.adopt_plan(tx, &actor, &root)?;
        let e = self.evaluate(tx.runs(), &root, &request.target, request.purpose)?;
        if !e.eligible {
            return Ok(Err(Fenced::Blocked(Box::new(e))));
        }
        let assignment = match &request.target {
            Target::Assignment(x)
            | Target::Operation {
                assignment: Some(x),
                ..
            } => Some(x.clone()),
            _ => None,
        };
        if let Some(id) = assignment {
            let bound = tx
                .runs()
                .assignment(&id)?
                .is_some_and(|x| x.execution_id.as_ref() == Some(&a.id));
            if !bound {
                return Ok(Err(Fenced::LeaseLost(format!(
                    "assignment {} is not bound to this activation",
                    id.0.as_str()
                ))));
            }
        }
        Ok(Ok(Admission {
            control_revision: e.control_revision,
            generation: e.generation,
            owner_epoch: a.owner_epoch,
            plan_revision: e.plan_revision,
            readiness_revision: e.readiness_revision,
            instruction_revision: e.instruction_revision,
            stop_epoch: e.stop_epoch,
        }))
    }

    /// Registers an assignment under its root's identity: the work must be the root or in the
    /// root feature's hierarchy, a parent must be a live assignment of the same root, the session
    /// must be a session of the root, a scope unit must be in the active approved plan (its digest
    /// and plan revision are bound), and a root has one live coordinator.
    pub fn register_assignment(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        new: NewRunAssignment,
    ) -> Gate<StoredAssignment> {
        let Some(root) = work_root(tx, &new.root)? else {
            return Ok(Err(Fenced::NotFound("root")));
        };
        let Some(work) = tx.sessions().work_root(&new.work)? else {
            return Ok(Err(Fenced::NotFound("work")));
        };
        if work.id != root.id && work.parent_feature_id.as_ref() != Some(&root.id) {
            return Ok(Err(Fenced::Invalid(
                "the work is outside the root's hierarchy".into(),
            )));
        }
        if let Some(p) = &new.parent {
            match tx.runs().assignment(p)? {
                Some(x) if x.root_id == root.id && x.state.is_live() => {}
                _ => {
                    return Ok(Err(Fenced::Invalid(
                        "the parent is not a live assignment of this root".into(),
                    )));
                }
            }
        }
        match tx.sessions().session(&new.session)? {
            Some(s) if s.root_id.as_ref() == Some(&root.id) => {}
            _ => {
                return Ok(Err(Fenced::Invalid(
                    "the session is not a session of this root".into(),
                )));
            }
        }
        let plan = self.scope.plan(tx.runs(), &root)?;
        let (digest, plan_revision) = match &new.scope_key {
            None => (None, plan.approved_revision()),
            Some(key) if !super::scope::valid_scope_key(key) => {
                return Ok(Err(Fenced::Invalid(
                    "a scope unit key is 1 to 256 characters; the feature-wide scope has none"
                        .into(),
                )));
            }
            Some(key) => match plan.unit(Some(key)) {
                Some(u) => (Some(u.digest.clone()), plan.approved_revision()),
                None => {
                    return Ok(Err(Fenced::Invalid(
                        "the scope unit is not part of the active approved plan".into(),
                    )));
                }
            },
        };
        let execution = tx
            .runs()
            .live_activation(&root.id)?
            .filter(|a| a.phase == ActivationPhase::Running)
            .map(|a| a.id);
        let a = StoredAssignment {
            id: AssignmentId(tx.new_id(IdKind::Link)),
            root_id: root.id.clone(),
            work_id: work.id.clone(),
            parent_id: new.parent.clone(),
            session_id: new.session.clone(),
            execution_id: execution,
            state: AssignmentState::Ready,
            generation: 0,
            slot_held: false,
            revision: 1,
            checkpoint: None,
            scope_key: new.scope_key.clone(),
            scope_digest: digest,
            plan_revision,
            // Bound to no redirect yet: a new or replacement coordinator binds instructions only
            // through `acknowledge_instructions`, so one registered after a redirect is fenced
            // until it has bound it into its context.
            instruction_revision: 0,
            created_at_ms: tx.now_ms(),
        };
        match tx.runs().insert_assignment(&a) {
            Ok(()) => {}
            Err(StorageError::Duplicate { .. }) => {
                return Ok(Err(Fenced::Invalid(
                    "the root already has a live coordinator or the work a live assignment".into(),
                )));
            }
            Err(e) => return Err(e),
        }
        if let Some(execution) = &a.execution_id {
            crate::services::work::allocation::transfer(
                tx,
                &a.work_id.0,
                execution,
                Some(&a.session_id),
                actor,
            )?;
        }
        record(
            tx,
            actor,
            &root,
            None,
            ASSIGNMENT_AGGREGATE,
            a.id.0.as_str(),
            1,
            "AssignmentRegistered",
            json!({
                "workId": a.work_id.0.as_str(),
                "parentId": a.parent_id.as_ref().map(|p| p.0.as_str()),
                "sessionId": a.session_id.as_str(),
                "scopeKey": a.scope_key,
                "planRevision": a.plan_revision,
            }),
        )?;
        Ok(Ok(a))
    }

    /// Changes an assignment's state at its expected revision (checkpoint kept unless given).
    pub fn set_assignment_state(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        id: &AssignmentId,
        expected: i64,
        state: AssignmentState,
        checkpoint: Option<Value>,
    ) -> Gate<StoredAssignment> {
        let Some(a) = tx.runs().assignment(id)? else {
            return Ok(Err(Fenced::NotFound("assignment")));
        };
        if a.revision != expected {
            return Ok(Err(Fenced::LeaseLost(format!(
                "assignment {} is at revision {}",
                id.0.as_str(),
                a.revision
            ))));
        }
        if !a.state.is_live() {
            return Ok(Err(Fenced::Invalid("the assignment has ended".into())));
        }
        let Some(root) = work_root(tx, &a.root_id)? else {
            return Ok(Err(Fenced::NotFound("root")));
        };
        let mut after = a.clone();
        after.state = state;
        after.revision += 1;
        if checkpoint.is_some() {
            after.checkpoint = checkpoint;
        }
        tx.runs().update_assignment(&after, a.revision)?;
        record(
            tx,
            actor,
            &root,
            None,
            ASSIGNMENT_AGGREGATE,
            id.0.as_str(),
            after.revision,
            "AssignmentStateChanged",
            json!({"from": a.state.as_str(), "to": state.as_str()}),
        )?;
        Ok(Ok(after))
    }

    /// Adopts the active approved plan if its revision changed since the root last adopted
    /// one: assignments whose bound unit left the plan or changed digest are superseded
    /// (fenced); the others rebind to the new revision and keep running. A pending revision is
    /// never adopted.
    pub fn adopt_plan(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        root: &WorkRoot,
    ) -> Result<PlanAdoption, StorageError> {
        let plan = self.scope.plan(tx.runs(), root)?;
        let Some(revision) = plan.approved_revision() else {
            return Ok(PlanAdoption::default());
        };
        let control = ensure_control(tx, &root.id)?;
        if control.adopted_plan_revision == Some(revision) {
            return Ok(PlanAdoption {
                revision: Some(revision),
                ..PlanAdoption::default()
            });
        }
        let mut adoption = PlanAdoption {
            revision: Some(revision),
            ..PlanAdoption::default()
        };
        for a in root_assignments(tx.runs(), &root.id, true)? {
            let Some(key) = &a.scope_key else {
                continue;
            };
            let mut after = a.clone();
            after.revision += 1;
            match plan.unit(Some(key)) {
                Some(u) if a.scope_digest.as_deref() == Some(u.digest.as_str()) => {
                    after.plan_revision = Some(revision);
                    adoption.kept.push(a.id.clone());
                }
                _ => {
                    after.state = AssignmentState::Superseded;
                    adoption.superseded.push(a.id.clone());
                }
            }
            tx.runs().update_assignment(&after, a.revision)?;
        }
        let mut after = control.clone();
        after.adopted_plan_revision = Some(revision);
        save_control(tx, &control, &mut after)?;
        record(
            tx,
            actor,
            root,
            None,
            super::CONTROL_AGGREGATE,
            root.id.0.as_str(),
            after.revision,
            "PlanAdopted",
            json!({
                "planRevision": revision,
                "superseded": adoption.superseded.iter().map(|a| a.0.as_str()).collect::<Vec<_>>(),
                "kept": adoption.kept.iter().map(|a| a.0.as_str()).collect::<Vec<_>>(),
            }),
        )?;
        Ok(adoption)
    }

    /// The acknowledged stage: at a safe boundary the coordinator binds its next context to every
    /// received instruction up to `up_to_seq` (wakeups coalesce to the latest; every instruction
    /// and its order is kept). Its dispatch fence lifts once it has bound the latest one.
    pub fn acknowledge_instructions(
        &self,
        tx: &mut dyn WriteTransaction,
        execution: &Id,
        epoch: i64,
        up_to_seq: i64,
    ) -> Gate<Vec<StoredInstruction>> {
        let (root, a) = match self.owned(tx, execution, epoch)? {
            Ok(x) => x,
            Err(f) => return Ok(Err(f)),
        };
        let actor = host(tx);
        let now = tx.now_ms();
        let mut acked = Vec::new();
        let mut after_seq = 0;
        loop {
            let page = tx.runs().instructions(&root.id, after_seq, 500)?;
            let more = page.len() == 500;
            for i in &page {
                after_seq = i.seq;
                if i.seq > up_to_seq || i.stage != InstructionStage::Received {
                    continue;
                }
                let mut x = i.clone();
                x.stage = InstructionStage::Acknowledged;
                x.acknowledged_at_ms = Some(now);
                x.acknowledged_by = Some(a.id.clone());
                x.revision += 1;
                tx.runs().update_instruction(&x, i.revision)?;
                record(
                    tx,
                    &actor,
                    &root,
                    None,
                    INSTRUCTION_AGGREGATE,
                    x.id.as_str(),
                    x.revision,
                    "InstructionAcknowledged",
                    json!({"seq": x.seq, "executionId": a.id.as_str()}),
                )?;
                acked.push(x);
            }
            if !more {
                break;
            }
        }
        if let Some(c) = coordinator(tx.runs(), &root.id)? {
            let control = control_of(tx.runs(), &root.id)?;
            let bound = up_to_seq.min(control.instruction_revision);
            if bound > c.instruction_revision {
                let mut after = c.clone();
                after.instruction_revision = bound;
                after.revision += 1;
                tx.runs().update_assignment(&after, c.revision)?;
            }
        }
        Ok(Ok(acked))
    }

    /// The incorporated stage: records the coordinator's disposition of an acknowledged
    /// instruction and supersedes exactly the incompatible assignments it names.
    pub fn incorporate_instruction(
        &self,
        tx: &mut dyn WriteTransaction,
        execution: &Id,
        epoch: i64,
        instruction: &Id,
        disposition: Disposition,
    ) -> Gate<StoredInstruction> {
        let (root, _) = match self.owned(tx, execution, epoch)? {
            Ok(x) => x,
            Err(f) => return Ok(Err(f)),
        };
        let Some(i) = tx.runs().instruction(instruction)? else {
            return Ok(Err(Fenced::NotFound("instruction")));
        };
        if i.root_id != root.id {
            return Ok(Err(Fenced::NotFound("instruction")));
        }
        match i.stage {
            InstructionStage::Received => {
                return Ok(Err(Fenced::Invalid(
                    "an instruction is acknowledged before it is incorporated".into(),
                )));
            }
            InstructionStage::Incorporated => return Ok(Ok(i)),
            InstructionStage::Acknowledged => {}
        }
        let actor = host(tx);
        // Every referenced assignment is validated before anything changes: a refusal writes
        // nothing (the caller's transaction may still commit its other work).
        let mut named = Vec::with_capacity(disposition.superseded.len());
        for id in &disposition.superseded {
            match tx.runs().assignment(id)? {
                Some(a) if a.root_id == root.id => named.push(a),
                _ => return Ok(Err(Fenced::NotFound("assignment"))),
            }
        }
        for a in named {
            if a.state.is_live() {
                let mut after = a.clone();
                after.state = AssignmentState::Superseded;
                after.revision += 1;
                tx.runs().update_assignment(&after, a.revision)?;
            }
        }
        let mut x = i.clone();
        x.stage = InstructionStage::Incorporated;
        x.incorporated_at_ms = Some(tx.now_ms());
        x.disposition = Some(json!({
            "summary": disposition.summary.chars().take(4096).collect::<String>(),
            "superseded": disposition.superseded.iter().map(|a| a.0.as_str()).collect::<Vec<_>>(),
            "pendingPlanRevision": disposition.pending_plan_revision,
        }));
        x.revision += 1;
        tx.runs().update_instruction(&x, i.revision)?;
        record(
            tx,
            &actor,
            &root,
            None,
            INSTRUCTION_AGGREGATE,
            x.id.as_str(),
            x.revision,
            "InstructionIncorporated",
            x.disposition.clone().unwrap_or(Value::Null),
        )?;
        Ok(Ok(x))
    }

    /// Starts the session's one inference/continuation turn. A turn still in flight under the
    /// current owner refuses a second; a turn left by an earlier owner is ended as interrupted.
    pub fn begin_turn(
        &self,
        tx: &mut dyn WriteTransaction,
        session: &Id,
        assignment: &AssignmentId,
        execution: &Id,
        epoch: i64,
    ) -> Gate<StoredTurn> {
        let (root, a) = match self.owned(tx, execution, epoch)? {
            Ok(x) => x,
            Err(f) => return Ok(Err(f)),
        };
        if a.phase != ActivationPhase::Running {
            return Ok(Err(Fenced::StopRequested));
        }
        match tx.runs().assignment(assignment)? {
            Some(x)
                if x.root_id == root.id
                    && x.state.is_live()
                    && &x.session_id == session
                    && x.execution_id.as_ref() == Some(execution) => {}
            _ => {
                return Ok(Err(Fenced::LeaseLost(
                    "the assignment is not bound to this session and activation".into(),
                )));
            }
        }
        let actor = host(tx);
        if let Some(t) = tx.runs().session_turn(session)? {
            let current = &t.execution_id == execution && t.owner_epoch == epoch;
            if current {
                return Ok(Err(Fenced::LeaseLost(format!(
                    "turn {} of this session is still in flight",
                    t.turn_id.as_str()
                ))));
            }
            tx.runs().delete_turn(session, &t.turn_id)?;
            record(
                tx,
                &actor,
                &root,
                None,
                "session_turn",
                session.as_str(),
                1,
                "TurnInterrupted",
                json!({"turnId": t.turn_id.as_str(), "executionId": t.execution_id.as_str()}),
            )?;
        }
        let turn = StoredTurn {
            session_id: session.clone(),
            turn_id: tx.new_id(IdKind::Link),
            assignment_id: assignment.clone(),
            execution_id: execution.clone(),
            owner_epoch: epoch,
            boot_id: boot(tx)?,
            started_at_ms: tx.now_ms(),
        };
        match tx.runs().insert_turn(&turn) {
            Ok(()) => Ok(Ok(turn)),
            Err(StorageError::Duplicate { .. }) => Ok(Err(Fenced::LeaseLost(
                "another turn of this session is in flight".into(),
            ))),
            Err(e) => Err(e),
        }
    }

    /// Ends the session's turn `turn` (a stale turn identity is refused).
    pub fn end_turn(&self, tx: &mut dyn WriteTransaction, session: &Id, turn: &Id) -> Gate<()> {
        match tx.runs().delete_turn(session, turn) {
            Ok(()) => Ok(Ok(())),
            Err(StorageError::LeaseLost { .. }) => {
                Ok(Err(Fenced::LeaseLost("the turn already ended".into())))
            }
            Err(e) => Err(e),
        }
    }

    /// Decides whether a submitted result is current evidence (CONTROL-04): a result from an
    /// earlier activation or claim epoch, a superseded assignment or an older plan revision is
    /// retained as attributable history but never accepted as current.
    pub fn accept_result(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        claim: &ResultClaim,
    ) -> Result<ResultAcceptance, StorageError> {
        let Some(a) = tx.runs().assignment(&claim.assignment)? else {
            return Ok(ResultAcceptance::Stale("unknown assignment".into()));
        };
        let activation = tx.runs().activation(&claim.execution_id)?;
        let current_boot = boot(tx)?;
        // The submitting owner must still own the activation now: an activation interrupted by
        // a restart, or requeued after its lease expired, has no current owner even though its
        // record keeps the old claim epoch.
        let owner_current = activation.as_ref().is_some_and(|x| {
            x.owner_epoch == claim.owner_epoch
                && x.owner.is_some()
                && x.boot_id == current_boot
                && matches!(
                    x.phase,
                    ActivationPhase::Claimed | ActivationPhase::Running | ActivationPhase::Stopping
                )
        });
        // Checked against the active approved plan now, not the assignment's cached binding: a
        // plan approved since the last safe boundary already governs what counts as current.
        let root = work_root(tx, &a.root_id)?;
        let plan = match &root {
            Some(r) => Some(self.scope.plan(tx.runs(), r)?),
            None => None,
        };
        let active_revision = plan.as_ref().and_then(|p| p.approved_revision());
        let compatible = match &plan {
            Some(p) if p.approved_revision().is_some() => {
                p.unit(a.scope_key.as_deref()).is_some_and(|u| {
                    a.scope_digest
                        .as_deref()
                        .is_none_or(|d| d == u.digest.as_str())
                })
            }
            _ => true,
        };
        let current_revision = active_revision.max(a.plan_revision);
        let verdict = if !a.state.is_live() {
            ResultAcceptance::Stale(format!("the assignment is {}", a.state.as_str()))
        } else if a.execution_id.as_ref() != Some(&claim.execution_id) {
            ResultAcceptance::Stale("the result comes from an earlier activation".into())
        } else if !owner_current {
            ResultAcceptance::Stale("the result comes from a superseded owner".into())
        } else if !compatible {
            ResultAcceptance::Stale(
                "the approved scope of this work changed or left the active plan".into(),
            )
        } else if claim.plan_revision.is_some() && claim.plan_revision < current_revision {
            ResultAcceptance::Stale("the result was produced under an older plan".into())
        } else {
            ResultAcceptance::Current
        };
        if let Some(root) = root {
            let (kind, reason) = match &verdict {
                ResultAcceptance::Current => ("ResultAccepted", None),
                ResultAcceptance::Stale(r) => ("LateResultRetained", Some(r.clone())),
            };
            record(
                tx,
                actor,
                &root,
                None,
                ASSIGNMENT_AGGREGATE,
                a.id.0.as_str(),
                a.revision,
                kind,
                json!({
                    "executionId": claim.execution_id.as_str(),
                    "ownerEpoch": claim.owner_epoch,
                    "planRevision": claim.plan_revision,
                    "reason": reason,
                }),
            )?;
        }
        Ok(verdict)
    }

    /// Records an explicit adoption of compatible older evidence by the current owner after it
    /// validated it against current requirements (it is never adopted automatically).
    pub fn adopt_evidence(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        assignment: &AssignmentId,
        evidence: &str,
        rationale: &str,
    ) -> Gate<()> {
        let Some(a) = tx.runs().assignment(assignment)? else {
            return Ok(Err(Fenced::NotFound("assignment")));
        };
        let Some(root) = work_root(tx, &a.root_id)? else {
            return Ok(Err(Fenced::NotFound("root")));
        };
        record(
            tx,
            actor,
            &root,
            None,
            ASSIGNMENT_AGGREGATE,
            a.id.0.as_str(),
            a.revision,
            "EvidenceAdopted",
            json!({"evidence": evidence, "rationale": rationale}),
        )?;
        Ok(Ok(()))
    }

    /// Whether the root currently permits any root-scoped dispatch: enabled, not archived, and
    /// no active root-level blocker. The dispatcher's root gate and its recovery of held work
    /// both ask this.
    pub fn root_permits_dispatch(
        &self,
        tx: &mut dyn WriteTransaction,
        root: &RootId,
    ) -> Result<bool, StorageError> {
        let Some(root) = work_root(tx, root)? else {
            return Ok(false);
        };
        Ok(self
            .evaluate(tx.runs(), &root, &Target::Root, ActivationPurpose::Resume)?
            .eligible)
    }

    /// Confirmed project archival (WORK-16, owned by the work item): every root of the project
    /// is stopped immediately — paused under a new stop epoch, its activation asked to stop and
    /// its root-scoped deliveries cancelled — with one journaled stop each. The caller then stops
    /// the project's owned processes at once (no cooperative grace); other projects continue.
    pub fn stop_project(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        project: &ProjectId,
    ) -> Result<Vec<(RootId, OperationId)>, StorageError> {
        let mut out = Vec::new();
        let mut after: Option<RootId> = None;
        loop {
            let page = tx.runs().project_roots(project, after.as_ref(), 500)?;
            let more = page.len() == 500;
            for id in &page {
                let Some(root) = work_root(tx, id)? else {
                    continue;
                };
                let control = ensure_control(tx, &root.id)?;
                let (op, _) = self.stop_root(
                    tx,
                    actor,
                    &root,
                    &control,
                    StopCause::Archive,
                    "The project was archived",
                )?;
                out.push((root.id.clone(), op));
            }
            if !more {
                return Ok(out);
            }
            after = page.last().cloned();
        }
    }
}

impl ControlService {
    /// Cooperative scheduling yield; preserves desired execution, blockers and assignments.
    /// The project allocator retains its slot until this activation has stopped and every
    /// descendant/effect/process has quiesced. A pending activation has never launched.
    pub fn request_allocation_yield(
        &self,
        tx: &mut dyn WriteTransaction,
        root_id: &RootId,
    ) -> Result<(), StorageError> {
        let Some(root) = tx.sessions().work_root(root_id)? else {
            return Ok(());
        };
        for linked in super::linked_roots(tx, &root)? {
            self.request_allocation_yield(tx, &linked.id)?;
        }
        let Some(before) = tx.runs().live_activation(root_id)? else {
            return Ok(());
        };
        if before.phase == ActivationPhase::Stopping {
            return Ok(());
        }
        if before.phase == ActivationPhase::Pending {
            self.stop_activation(
                tx,
                &root,
                &before,
                ActivationOutcome::Prevented,
                Some("yielded for prerequisite work"),
            )?;
            return Ok(());
        }
        let mut after = before.clone();
        after.phase = ActivationPhase::Stopping;
        after.stop_cause = Some(StopCause::Superseded);
        after.stop_requested_at_ms = Some(tx.now_ms());
        self.update(
            tx,
            &root,
            &before,
            after,
            "AllocationYieldRequested",
            json!({"reason":"fully dependency-blocked; waiting for quiescence"}),
        )?;
        Ok(())
    }
}
