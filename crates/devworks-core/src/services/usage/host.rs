//! One owned inference transport. No provider callback runs inside a storage transaction.
//! Only this owner can attest local closure; EOF does not assert remote billing completion.
use super::accounting::{self, Admission, AttemptEnd, LocalQuiescence, Observation};
use crate::ports::storage::*;
use crate::protocol::Id;
use crate::services::{router::InferenceRouter, runs::ControlService};
use std::sync::Arc;

/// A prepared transport, exclusively owned by one invocation of `AttemptHost::drive`.
/// `next` must observe cancellation while waiting. `close` returns only after all local
/// transport work/callbacks have ceased, including when `start` or `next` failed.
pub trait OwnedInferenceSource: Send {
    fn start(&mut self, attempt: &StoredInferenceAttempt) -> Result<(), StorageError>;
    fn next(&mut self, cancellation: &Cancellation)
    -> Result<Option<InferenceFrame>, StorageError>;
    fn close(&mut self) -> Result<(), StorageError>;
}
#[derive(Clone, Debug)]
pub enum InferenceFrame {
    Usage(Observation),
    Terminal(AttemptEnd),
}
#[derive(Clone, Debug)]
pub enum DriveOutcome {
    Settled(StoredAttemptBalance),
    Refused(Admission),
    /// The durable dispatch may already have an owner. Never open a second source.
    AlreadyDispatched,
    CancelledBeforeDispatch,
}

pub struct AttemptHost {
    store: Arc<dyn ApplicationStore>,
    control: Arc<ControlService>,
    router: Arc<InferenceRouter>,
}
impl AttemptHost {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        control: Arc<ControlService>,
        router: Arc<InferenceRouter>,
    ) -> Self {
        Self {
            store,
            control,
            router,
        }
    }
    /// Blocking host worker entry point. The caller retains the cancellation handle for
    /// stop/shutdown while the owned source waits; no database lock spans source work.
    pub fn drive(
        &self,
        id: Id,
        mut source: Box<dyn OwnedInferenceSource>,
        cancellation: Cancellation,
    ) -> Result<DriveOutcome, StorageError> {
        if cancellation.is_cancelled() {
            run_write(self.store.as_ref(), WriteOptions::default(), move |tx| {
                accounting::cancel_admitted(tx, &id, "host cancelled before transport dispatch")
            })?;
            return Ok(DriveOutcome::CancelledBeforeDispatch);
        }
        let router = self.router.clone();
        let control = self.control.clone();
        let dispatch_id = id.clone();
        let admission = run_write(self.store.as_ref(), WriteOptions::default(), move |tx| {
            let admission = router.mark_attempt_dispatched(tx, &control, &dispatch_id)?;
            if !matches!(admission, Admission::Admitted { .. }) {
                accounting::cancel_admitted(
                    tx,
                    &dispatch_id,
                    "final transport admission was fenced",
                )?;
            }
            Ok(admission)
        })?;
        let attempt = match admission {
            Admission::Admitted { reused: true, .. } => return Ok(DriveOutcome::AlreadyDispatched),
            Admission::Admitted {
                attempt,
                reused: false,
            } => attempt,
            refusal => return Ok(DriveOutcome::Refused(refusal)),
        };
        let bound = accounting::snapshot(&attempt)?;
        let mut outcome = AttemptEnd::UnknownTail;
        let mut failure = source.start(&attempt).err();
        if failure.is_none() {
            loop {
                match self.interrupted(&attempt) {
                    Err(error) => {
                        failure = Some(error);
                        cancellation.cancel();
                        break;
                    }
                    Ok(true) => cancellation.cancel(),
                    Ok(false) => {}
                }
                if cancellation.is_cancelled() {
                    break;
                }
                match source.next(&cancellation) {
                    Ok(Some(InferenceFrame::Usage(observation))) => {
                        let control = self.control.clone();
                        let id = id.clone();
                        if let Err(error) =
                            run_write(self.store.as_ref(), WriteOptions::default(), move |tx| {
                                accounting::observe(tx, &control, &id, &observation)
                            })
                        {
                            failure = Some(error);
                            cancellation.cancel();
                            break;
                        }
                    }
                    Ok(Some(InferenceFrame::Terminal(terminal))) => {
                        outcome = terminal;
                        break;
                    }
                    Ok(None) => break,
                    Err(error) => {
                        failure = Some(error);
                        cancellation.cancel();
                        break;
                    }
                }
            }
        }
        // A failed close leaves the reservation and inference quiescence guard in place.
        // A host restart can reconcile it once this process's exclusive data-root owner exits.
        source.close()?;
        let proof = LocalQuiescence {
            attempt_id: id,
            boot_id: bound.admitted_boot_id,
        };
        let control = self.control.clone();
        let balance = run_write(self.store.as_ref(), WriteOptions::default(), move |tx| {
            accounting::settle(tx, &control, &proof, outcome)
        })?;
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(DriveOutcome::Settled(balance))
    }
    fn interrupted(&self, attempt: &StoredInferenceAttempt) -> Result<bool, StorageError> {
        read_snapshot(self.store.as_ref(), &ReadOptions::default(), |s| {
            if let Some(root) = &attempt.execution_root_id {
                if s.runs()
                    .root_control(root)?
                    .is_some_and(|c| c.desired == DesiredExecution::Paused)
                {
                    return Ok(true);
                }
                if let Some(execution) = &attempt.execution_id {
                    let bound = accounting::snapshot(attempt)?;
                    if !s.runs().activation(execution)?.is_some_and(|a| {
                        matches!(a.phase, ActivationPhase::Claimed | ActivationPhase::Running)
                            && Some(a.owner_epoch) == bound.dispatch.as_ref().map(|p| p.owner_epoch)
                    }) {
                        return Ok(true);
                    }
                }
            }
            Ok(false)
        })
    }
}

/// Bounded old-boot recovery. Exclusive data-root ownership proves the previous process's
/// local transports are gone; it does not prove remote completion or remove unknown billing.
pub fn recover_page(
    tx: &mut dyn WriteTransaction,
    control: &ControlService,
    after: Option<&Id>,
    limit: u16,
) -> Result<Vec<Id>, StorageError> {
    let current_boot = tx
        .store()
        .boot_id
        .clone()
        .ok_or_else(|| StorageError::InvalidRecord("missing host boot".into()))?;
    let rows = tx
        .usage()
        .inference_attempts(&UsageAttemptFilter::default(), after, limit)?;
    let mut visited = Vec::with_capacity(rows.len());
    for attempt in rows {
        visited.push(attempt.attempt_id.clone());
        let bound = accounting::snapshot(&attempt)?;
        if bound.admitted_boot_id == current_boot {
            continue;
        }
        if attempt.state == "Admitted" {
            accounting::cancel_admitted(
                tx,
                &attempt.attempt_id,
                "previous host exited before dispatch",
            )?;
        } else if matches!(attempt.state.as_str(), "Dispatched" | "UnknownTail")
            && !tx
                .usage()
                .attempt_balance(&attempt.attempt_id)?
                .is_some_and(|b| b.local_quiescent)
        {
            accounting::settle(
                tx,
                control,
                &LocalQuiescence {
                    attempt_id: attempt.attempt_id,
                    boot_id: bound.admitted_boot_id,
                },
                AttemptEnd::UnknownTail,
            )?;
        }
    }
    Ok(visited)
}
