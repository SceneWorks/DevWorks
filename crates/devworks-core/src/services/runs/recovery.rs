//! Run-control recovery classes (AD20, CONTROL-05/08/09/10/11) and stop settlement.
//!
//! * **Boot** ([`ControlService::recover_boot`], after queue, artifact and owned-process
//!   recovery and before any delivery or command): every activation an earlier boot left live —
//!   running, stopping, claimed or accepted but never launched — is `Stopped` as `Interrupted`
//!   and its root is held with a `RestartContinue` hold until an explicit, selective Continue.
//!   The logical coordinator, assignment lineage (active assignments go dormant) and results are
//!   kept. Turns left in flight are ended as interrupted. Journaled stops of earlier boots are
//!   settled from what process recovery verified.
//! * **Shutdown** ([`ControlService::quiesce_for_shutdown`] then
//!   [`ControlService::finish_shutdown`]): admission is fenced, live activations are held for
//!   manual Continue and asked to stop, then recorded `Interrupted` once owned work was stopped.
//! * **Same-process failure and sleep/wake**: an expired activation lease of this boot is a
//!   transient failure retried with bounded backoff ([`ControlService::recover_expired`]); after
//!   a detected wake the running owner reasserts its leases first
//!   ([`ControlService::reassert_leases`]). Neither adds a manual hold or clears one.
//! * Scheduled maintenance/inference runs and keep-working are owned by the schedules and
//!   delegation items; [`RecoveryClass`] and [`LifecycleHooks`] are the hooks they plug in.
use super::substrate::ActivationFailure;
use super::{
    ACTIVATION_AGGREGATE, CONTROL_AGGREGATE, ControlService, HoldSubject, STOP_CAPABILITY,
    active_blockers, clear_blocker, ensure_control, raise_blocker, record, root_assignments,
    save_control,
};
use crate::ports::storage::{
    ActivationFilter, ActivationOutcome, ActivationPhase, AssignmentState, AttemptBinding,
    BlockerFilter, DesiredExecution, HoldCause, ProcessFilter, StopCause, StorageError,
    StoredActivation, WorkRoot, WriteTransaction,
};
use crate::protocol::{Id, OperationId, OperationState, RootId};
use serde_json::json;

const PAGE: u16 = 500;

/// How a kind of work recovers after a new boot (the AD20 recovery classes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryClass {
    /// Features and standalone issues: inspect/reconcile, then stay held until an explicit,
    /// authorized Continue (including accepted-but-unlaunched activations).
    ManualContinue,
    /// An interrupted scheduled maintenance or inference run: resume the same run identity after
    /// reconciliation when its schedule and project are enabled and holds permit.
    ResumeAfterReconciliation,
    /// Keep-working mode and restored schedules: stay stopped until explicitly restarted.
    StaysStopped,
}
/// The recovery class of a run root (every root is a feature or a standalone issue).
pub fn root_recovery_class(_root: &WorkRoot) -> RecoveryClass {
    RecoveryClass::ManualContinue
}

/// Lifecycle hooks of the schedules and keep-working items, run in the documented order: at
/// shutdown before anything stops (freeze schedule admission and keep-working allocation), and at
/// boot after root recovery (resume interrupted scheduled runs by their existing identity,
/// recompute next occurrences, never catch up). Both run in the same unit of work as root
/// control's own step, and neither can clear a feature or issue hold.
pub trait LifecycleHooks: Send + Sync {
    fn quiesce(&self, _tx: &mut dyn WriteTransaction) -> Result<(), StorageError> {
        Ok(())
    }
    fn recover(&self, _tx: &mut dyn WriteTransaction) -> Result<(), StorageError> {
        Ok(())
    }
}
/// No schedules or keep-working are installed yet.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoLifecycleHooks;
impl LifecycleHooks for NoLifecycleHooks {}

/// What boot recovery did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BootRecovery {
    /// Activations of earlier boots recorded `Interrupted`.
    pub interrupted: Vec<Id>,
    /// Roots held for manual Continue.
    pub held: Vec<RootId>,
    pub turns_ended: usize,
    /// Journaled stops of earlier boots and the state they settled in.
    pub stops: Vec<(OperationId, OperationState)>,
}

/// Drives a stop operation to its observed outcome: `ObservedComplete` when nothing of the
/// root is left running, `OutcomeUnknown` (held for reconciliation) when some owned tree could
/// not be verified stopped. It walks the journal's allowed transitions from wherever the
/// operation is (an interrupted boot may have left it `IntentRecorded`, `RetryPending` or
/// `OutcomeUnknown`).
pub fn complete_stop_operation(
    tx: &mut dyn WriteTransaction,
    op: &OperationId,
    uncertain: &[(Id, String)],
    note: &str,
) -> Result<OperationState, StorageError> {
    let observation = json!({
        "note": note,
        "uncertain": uncertain.iter().map(|(p, why)| json!({"processId": p.as_str(), "why": why})).collect::<Vec<_>>(),
    });
    for _ in 0..6 {
        let rec = tx
            .operations()
            .operation(op)?
            .ok_or_else(|| StorageError::NotFound {
                kind: "operation",
                id: op.0.as_str().to_owned(),
            })?;
        let fence = rec.latest_attempt().map(|a| a.fence());
        match rec.state {
            OperationState::IntentRecorded | OperationState::RetryPending => {
                tx.operations()
                    .begin_attempt(op, rec.revision, AttemptBinding::default())?;
            }
            OperationState::Preflighted => {
                tx.operations().transition(
                    op,
                    rec.revision,
                    fence.as_ref(),
                    OperationState::Dispatching,
                    None,
                )?;
            }
            OperationState::Dispatching => {
                let to = if uncertain.is_empty() {
                    OperationState::ObservedComplete
                } else {
                    OperationState::OutcomeUnknown
                };
                tx.operations().transition(
                    op,
                    rec.revision,
                    fence.as_ref(),
                    to,
                    Some(observation.clone()),
                )?;
            }
            OperationState::OutcomeUnknown if uncertain.is_empty() => {
                tx.operations().transition(
                    op,
                    rec.revision,
                    None,
                    OperationState::ObservedComplete,
                    Some(observation.clone()),
                )?;
            }
            state => return Ok(state),
        }
    }
    Ok(tx
        .operations()
        .operation(op)?
        .map_or(OperationState::OutcomeUnknown, |o| o.state))
}

/// Marks the stop operation `Dispatching` before its native effect (killing owned trees) runs,
/// so a crash in between leaves it `OutcomeUnknown` for boot recovery, never silently complete.
pub fn dispatch_stop_operation(
    tx: &mut dyn WriteTransaction,
    op: &OperationId,
) -> Result<OperationState, StorageError> {
    for _ in 0..3 {
        let Some(rec) = tx.operations().operation(op)? else {
            return Ok(OperationState::Failed);
        };
        let fence = rec.latest_attempt().map(|a| a.fence());
        match rec.state {
            OperationState::IntentRecorded | OperationState::RetryPending => {
                tx.operations()
                    .begin_attempt(op, rec.revision, AttemptBinding::default())?;
            }
            OperationState::Preflighted => {
                tx.operations().transition(
                    op,
                    rec.revision,
                    fence.as_ref(),
                    OperationState::Dispatching,
                    None,
                )?;
            }
            state => return Ok(state),
        }
    }
    Ok(OperationState::Dispatching)
}

impl ControlService {
    fn activations_where(
        &self,
        tx: &mut dyn WriteTransaction,
        filter: ActivationFilter,
    ) -> Result<Vec<StoredActivation>, StorageError> {
        let mut out: Vec<StoredActivation> = Vec::new();
        loop {
            let page = tx
                .runs()
                .activations(&filter, out.last().map(|a| &a.id), PAGE)?;
            let more = page.len() == usize::from(PAGE);
            out.extend(page);
            if !more {
                return Ok(out);
            }
        }
    }

    /// Holds the root for manual Continue after (or ahead of) a restart: `RestartContinue`
    /// hold, desired `Paused`, no pending reactivation, active assignments dormant. Idempotent:
    /// a root already held this way is left as it is.
    fn hold_for_restart(
        &self,
        tx: &mut dyn WriteTransaction,
        root: &WorkRoot,
        reason: &str,
    ) -> Result<bool, StorageError> {
        let held = active_blockers(tx.runs(), &BlockerFilter::RootLevel(root.id.clone()))?
            .iter()
            .any(|b| b.cause == HoldCause::RestartContinue);
        if held {
            return Ok(false);
        }
        let boot = tx
            .store()
            .boot_id
            .clone()
            .ok_or_else(|| StorageError::IncompatibleSchema("store is read-only".into()))?;
        let actor = tx.store().host_actor.clone();
        let control = ensure_control(tx, &root.id)?;
        let mut after = control.clone();
        after.revision += 1;
        after.desired = DesiredExecution::Paused;
        after.restart_boot_id = Some(boot.clone());
        after.reactivate = false;
        save_control(tx, &control, &mut after)?;
        let hold = raise_blocker(
            tx,
            &actor,
            root,
            HoldSubject::default(),
            HoldCause::RestartContinue,
            boot.as_str(),
            1,
            reason,
            json!({"clearsWith": "Continue", "bootId": boot.as_str()}),
        )?;
        for a in root_assignments(tx.runs(), &root.id, true)? {
            if a.state == AssignmentState::Active {
                let mut dormant = a.clone();
                dormant.state = AssignmentState::Waiting;
                dormant.revision += 1;
                tx.runs().update_assignment(&dormant, a.revision)?;
            }
        }
        record(
            tx,
            &actor,
            root,
            None,
            CONTROL_AGGREGATE,
            root.id.0.as_str(),
            after.revision,
            "RootHeldForRestart",
            json!({"holdId": hold.id.as_str(), "bootId": boot.as_str(), "reason": reason}),
        )?;
        Ok(true)
    }

    /// Boot recovery of run control (see the module documentation). Runs after owned-process
    /// recovery, so what it settles reflects verified process state, and before any claim.
    pub fn recover_boot(
        &self,
        tx: &mut dyn WriteTransaction,
        hooks: &dyn LifecycleHooks,
    ) -> Result<BootRecovery, StorageError> {
        let boot = tx
            .store()
            .boot_id
            .clone()
            .ok_or_else(|| StorageError::IncompatibleSchema("store is read-only".into()))?;
        let mut report = BootRecovery::default();
        let now = tx.now_ms();
        for a in self.activations_where(tx, ActivationFilter::LivePriorBoots(boot.clone()))? {
            let Some(root) = tx.sessions().work_root(&a.root_id)? else {
                continue;
            };
            let mut stopped = a.clone();
            stopped.phase = ActivationPhase::Stopped;
            stopped.outcome = Some(ActivationOutcome::Interrupted);
            stopped.stop_cause = Some(stopped.stop_cause.unwrap_or(StopCause::Restart));
            stopped.stop_requested_at_ms = Some(stopped.stop_requested_at_ms.unwrap_or(now));
            stopped.stopped_at_ms = Some(now);
            stopped.owner = None;
            stopped.lease_until_ms = None;
            stopped.detail = Some(format!(
                "the app restarted while the activation was {}",
                a.phase.as_str()
            ));
            stopped.revision = a.revision + 1;
            tx.runs().update_activation(&stopped, a.revision)?;
            let actor = tx.store().host_actor.clone();
            record(
                tx,
                &actor,
                &root,
                None,
                ACTIVATION_AGGREGATE,
                a.id.as_str(),
                stopped.revision,
                "ActivationInterrupted",
                json!({"previousPhase": a.phase.as_str(), "bootId": a.boot_id.as_str()}),
            )?;
            report.interrupted.push(a.id.clone());
            if self.hold_for_restart(
                tx,
                &root,
                "The app restarted; Continue to resume this work",
            )? {
                report.held.push(root.id.clone());
            }
        }
        loop {
            let turns = tx.runs().turns_prior_boots(&boot, PAGE)?;
            if turns.is_empty() {
                break;
            }
            for t in &turns {
                tx.runs().delete_turn(&t.session_id, &t.turn_id)?;
                report.turns_ended += 1;
            }
        }
        let mut after: Option<RootId> = None;
        loop {
            let page = tx.runs().pending_stops(after.as_ref(), PAGE)?;
            let more = page.len() == usize::from(PAGE);
            for c in &page {
                let Some(op) = &c.stop_operation_id else {
                    continue;
                };
                let uncertain = unsettled_processes(tx, &c.root_id)?;
                let state = self.settle_stop(tx, &c.root_id, op, &uncertain)?;
                report.stops.push((op.clone(), state));
            }
            if !more {
                break;
            }
            after = page.last().map(|c| c.root_id.clone());
        }
        hooks.recover(tx)?;
        Ok(report)
    }

    /// Shutdown step 1 (AD20): every live activation of this boot is held for manual Continue
    /// and asked to stop (a never-launched one stops at once). Schedules and keep-working are
    /// frozen by `hooks` in the same unit of work. Returns the roots held.
    pub fn quiesce_for_shutdown(
        &self,
        tx: &mut dyn WriteTransaction,
        hooks: &dyn LifecycleHooks,
    ) -> Result<Vec<RootId>, StorageError> {
        hooks.quiesce(tx)?;
        let boot = tx
            .store()
            .boot_id
            .clone()
            .ok_or_else(|| StorageError::IncompatibleSchema("store is read-only".into()))?;
        let actor = tx.store().host_actor.clone();
        let mut held = Vec::new();
        for a in self.activations_where(tx, ActivationFilter::LiveOfBoot(boot))? {
            let Some(root) = tx.sessions().work_root(&a.root_id)? else {
                continue;
            };
            self.hold_for_restart(
                tx,
                &root,
                "The app was closed; Continue to resume this work",
            )?;
            self.request_stop(tx, &actor, &root, StopCause::Shutdown)?;
            held.push(root.id.clone());
        }
        Ok(held)
    }

    /// Shutdown step 4 (AD20): after owned work was stopped, every activation of this boot still
    /// live is recorded `Interrupted`. Returns how many.
    pub fn finish_shutdown(&self, tx: &mut dyn WriteTransaction) -> Result<usize, StorageError> {
        let boot = tx
            .store()
            .boot_id
            .clone()
            .ok_or_else(|| StorageError::IncompatibleSchema("store is read-only".into()))?;
        let live = self.activations_where(tx, ActivationFilter::LiveOfBoot(boot))?;
        let n = live.len();
        for a in live {
            let Some(root) = tx.sessions().work_root(&a.root_id)? else {
                continue;
            };
            // The record is the current one (listed in this transaction); only its cause is set.
            let mut a = a;
            a.stop_cause = a.stop_cause.or(Some(StopCause::Shutdown));
            self.stop_activation(
                tx,
                &root,
                &a,
                ActivationOutcome::Interrupted,
                Some("the app was closed before the activation stopped"),
            )?;
        }
        Ok(n)
    }

    /// Records the outcome of a stop after its native part ran (or after boot recovery
    /// verified it): `ObservedComplete`, or `OutcomeUnknown` with one `Reconciliation` hold
    /// naming the trees that could not be verified stopped. A stopping activation nobody owns
    /// any more ends `Cancelled`.
    pub fn settle_stop(
        &self,
        tx: &mut dyn WriteTransaction,
        root_id: &RootId,
        op: &OperationId,
        uncertain: &[(Id, String)],
    ) -> Result<OperationState, StorageError> {
        let Some(root) = tx.sessions().work_root(root_id)? else {
            return complete_stop_operation(tx, op, uncertain, "the root no longer exists");
        };
        if let Some(a) = tx.runs().live_activation(root_id)?
            && a.phase == ActivationPhase::Stopping
            && a.owner.is_none()
        {
            self.stop_activation(tx, &root, &a, ActivationOutcome::Cancelled, Some("stopped"))?;
        }
        // An empty process list is not proof that the root stopped: an owned activation still
        // stopping, or root-scoped work still claimed, keeps the stop pending (it is executed and
        // settled again once they have quiesced).
        if !root_quiesced(tx, root_id)? {
            return Ok(tx
                .operations()
                .operation(op)?
                .map_or(OperationState::Dispatching, |o| o.state));
        }
        let state = complete_stop_operation(tx, op, uncertain, "owned process trees stopped")?;
        let actor = tx.store().host_actor.clone();
        if state == OperationState::ObservedComplete {
            // The trees are verified stopped now: the reconciliation hold this stop raised (and
            // only that one) is cleared with the observation.
            for b in active_blockers(tx.runs(), &BlockerFilter::RootLevel(root_id.clone()))? {
                if b.cause == HoldCause::Reconciliation && b.origin_id == op.0.as_str() {
                    clear_blocker(
                        tx,
                        &actor,
                        &root,
                        &b,
                        json!({"operationId": op.0.as_str(), "observed": "ObservedComplete"}),
                    )?;
                }
            }
        }
        if state == OperationState::OutcomeUnknown {
            let names: Vec<&str> = uncertain.iter().map(|(p, _)| p.as_str()).collect();
            raise_blocker(
                tx,
                &actor,
                &root,
                HoldSubject {
                    assignment: None,
                    operation: None,
                },
                HoldCause::Reconciliation,
                op.0.as_str(),
                1,
                &format!(
                    "Owned processes could not be verified stopped: {}",
                    names.join(", ")
                ),
                json!({"operationId": op.0.as_str(), "processes": names}),
            )?;
        }
        record(
            tx,
            &actor,
            &root,
            Some(op),
            CONTROL_AGGREGATE,
            root_id.0.as_str(),
            1,
            "RootStopObserved",
            json!({"state": format!("{state:?}"), "uncertain": uncertain.len(), "capability": STOP_CAPABILITY}),
        )?;
        Ok(state)
    }

    /// Same-process recovery of activation leases of this boot that expired: a claimed or
    /// running one is a transient failure (bounded backoff; escalated after the last attempt), a
    /// stopping one whose owner stopped renewing ends `Interrupted`. No manual hold is added.
    pub fn recover_expired(&self, tx: &mut dyn WriteTransaction) -> Result<usize, StorageError> {
        let boot = tx
            .store()
            .boot_id
            .clone()
            .ok_or_else(|| StorageError::IncompatibleSchema("store is read-only".into()))?;
        let now = tx.now_ms();
        let mut n = 0;
        for a in self.activations_where(tx, ActivationFilter::LiveOfBoot(boot))? {
            if !a.lease_until_ms.is_some_and(|t| t < now) {
                continue;
            }
            let Some(root) = tx.sessions().work_root(&a.root_id)? else {
                continue;
            };
            n += 1;
            if a.phase == ActivationPhase::Stopping {
                self.stop_activation(
                    tx,
                    &root,
                    &a,
                    ActivationOutcome::Interrupted,
                    Some("the owner stopped renewing while stopping"),
                )?;
            } else {
                self.fail_activation(
                    tx,
                    &root,
                    &a,
                    ActivationFailure::Transient("the supervisor's lease expired".into()),
                )?;
            }
        }
        Ok(n)
    }

    /// After a detected sleep/wake, the still-running owner reasserts its expired leases before
    /// expiry recovery can reassign them (CONTROL-11: running work continues; pauses stay).
    pub fn reassert_leases(
        &self,
        tx: &mut dyn WriteTransaction,
        owner: &Id,
    ) -> Result<usize, StorageError> {
        let boot = tx
            .store()
            .boot_id
            .clone()
            .ok_or_else(|| StorageError::IncompatibleSchema("store is read-only".into()))?;
        let now = tx.now_ms();
        let actor = tx.store().host_actor.clone();
        let mut n = 0;
        for a in self.activations_where(tx, ActivationFilter::LiveOfBoot(boot))? {
            if a.owner.as_ref() != Some(owner) || !a.lease_until_ms.is_some_and(|t| t < now) {
                continue;
            }
            let Some(root) = tx.sessions().work_root(&a.root_id)? else {
                continue;
            };
            let mut renewed = a.clone();
            renewed.lease_until_ms = Some(now + self.config.lease_ms);
            renewed.revision = a.revision + 1;
            tx.runs().update_activation(&renewed, a.revision)?;
            record(
                tx,
                &actor,
                &root,
                None,
                ACTIVATION_AGGREGATE,
                a.id.as_str(),
                renewed.revision,
                "ActivationLeaseReasserted",
                json!({"cause": "Wake"}),
            )?;
            n += 1;
        }
        Ok(n)
    }
}

/// Whether nothing admitted for the root is still running: no live activation with an owner and
/// no root-scoped delivery still claimed.
pub fn root_quiesced(tx: &mut dyn WriteTransaction, root: &RootId) -> Result<bool, StorageError> {
    if tx
        .runs()
        .live_activation(root)?
        .is_some_and(|a| a.owner.is_some())
    {
        return Ok(false);
    }
    let mut after: Option<Id> = None;
    loop {
        let page = tx.runs().root_deliveries(root, after.as_ref(), PAGE)?;
        for id in &page {
            if tx
                .queue()
                .entry(id)?
                .is_some_and(|e| e.state == crate::ports::storage::QueueState::Claimed)
            {
                return Ok(false);
            }
        }
        if page.len() < usize::from(PAGE) {
            return Ok(true);
        }
        after = page.last().cloned();
    }
}

/// Owned process trees of the root not yet verified quiesced, with what is known of each.
pub fn unsettled_processes(
    tx: &mut dyn WriteTransaction,
    root: &RootId,
) -> Result<Vec<(Id, String)>, StorageError> {
    let mut out = Vec::new();
    let mut after: Option<Id> = None;
    loop {
        let page = tx
            .resources()
            .processes(&ProcessFilter::Unsettled, after.as_ref(), PAGE)?;
        for p in &page {
            if p.root_id.as_ref() == Some(root) {
                out.push((p.id.clone(), format!("{:?}", p.phase)));
            }
        }
        if page.len() < usize::from(PAGE) {
            return Ok(out);
        }
        after = page.last().map(|p| p.id.clone());
    }
}
