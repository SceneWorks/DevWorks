//! One owned inference transport. No provider callback runs inside a storage transaction.
//! Only this owner can attest local closure; EOF does not assert remote billing completion.
use super::accounting::{self, Admission, AttemptEnd, LocalQuiescence, Observation};
use crate::ports::storage::*;
use crate::protocol::Id;
use crate::services::{router::InferenceRouter, runs::ControlService};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, RwLock};

/// Closing the fence drains any admitted accounting unit through its commit acknowledgment.
/// A transport callback after closure cannot mutate the ledger, even if local close is stuck.
#[derive(Default)]
pub struct AccountingWriteFence {
    closed: RwLock<bool>,
    admitted: Mutex<HashSet<Id>>,
}
impl AccountingWriteFence {
    pub fn close(&self) {
        *self.closed.write().unwrap_or_else(|p| p.into_inner()) = true;
    }
    /// Volatile host previews must disappear when shutdown fences the owned producer, even
    /// when an unclosed transport still holds its sink alive.
    pub fn is_closed(&self) -> bool {
        *self.closed.read().unwrap_or_else(|p| p.into_inner())
    }
    /// Registered before releasing the admission unit's fence, including the gap before a
    /// producer installs its cancellation token. Shutdown never scans settled history.
    pub fn admitted(&self) -> HashSet<Id> {
        self.admitted
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub fn forget_closed(&self, id: &Id) {
        self.admitted
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
    }
    /// Trusted host publication preparation may stage bounded local artifacts under this fence.
    /// Never call a provider, network transport or package process inside this closure.
    pub fn publication<T>(
        &self,
        work: impl FnOnce() -> Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        let closed = self.closed.read().unwrap_or_else(|p| p.into_inner());
        if *closed {
            return Err(StorageError::Cancelled);
        }
        let result = work();
        drop(closed);
        result
    }
    fn admit<F>(&self, store: &dyn ApplicationStore, work: F) -> Result<Admission, StorageError>
    where
        F: FnOnce(&mut dyn WriteTransaction) -> Result<Admission, StorageError> + Send + 'static,
    {
        let closed = self.closed.read().unwrap_or_else(|p| p.into_inner());
        if *closed {
            return Err(StorageError::Cancelled);
        }
        let result = run_write(store, WriteOptions::default(), work);
        if let Ok(Admission::Admitted { attempt, .. }) = &result {
            self.admitted
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(attempt.attempt_id.clone());
        }
        drop(closed);
        result
    }

    pub fn write<T, F>(&self, store: &dyn ApplicationStore, work: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
    {
        let closed = self.closed.read().unwrap_or_else(|p| p.into_inner());
        if *closed {
            return Err(StorageError::Cancelled);
        }
        let result = run_write(store, WriteOptions::default(), work);
        drop(closed);
        result
    }
}

/// A prepared transport, exclusively owned by one invocation of `AttemptHost::drive`.
/// `next` must observe cancellation while waiting. `close` returns only after all local
/// transport work/callbacks have ceased, including when `start` or `next` failed.
pub trait OwnedInferenceSource: Send {
    fn start(&mut self, attempt: &StoredInferenceAttempt) -> Result<(), StorageError>;
    fn start_cancellable(
        &mut self,
        attempt: &StoredInferenceAttempt,
        cancellation: &Cancellation,
    ) -> Result<(), StorageError> {
        if cancellation.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        self.start(attempt)
    }
    fn next(&mut self, cancellation: &Cancellation)
    -> Result<Option<InferenceFrame>, StorageError>;
    fn close(&mut self) -> Result<(), StorageError>;
    /// A qualified source may prove no request was sent; unknown send boundaries stay unknown.
    fn start_failure_outcome(&self) -> AttemptEnd {
        AttemptEnd::UnknownTail
    }
}
#[derive(Clone, Debug)]
pub enum InferenceFrame {
    Usage(Observation),
    Output(crate::providers::decode::DecodedFrame),
    ResponseHead(crate::providers::ProviderResponseHead),
    UsageDimensions(super::dimensions::ObservedDimensions),
    Terminal(AttemptEnd),
}
/// Trusted native result owner. The transport emits data only; this separate host sink stages
/// local artifacts and commits through the SAME fence as accounting. Provider callbacks never
/// run inside a writer or while the sink holds a publication fence.
pub trait OwnedInferenceSink: Send {
    fn response_head(
        &mut self,
        _attempt: &StoredInferenceAttempt,
        _head: crate::providers::ProviderResponseHead,
        _fence: &AccountingWriteFence,
    ) -> Result<(), StorageError> {
        Ok(())
    }

    fn accept(
        &mut self,
        attempt: &StoredInferenceAttempt,
        frame: crate::providers::decode::DecodedFrame,
        fence: &AccountingWriteFence,
    ) -> Result<(), StorageError>;
    /// Called only after physical source closure, including an interrupted/error tail.
    fn finish(
        &mut self,
        attempt: &StoredInferenceAttempt,
        outcome: AttemptEnd,
        fence: &AccountingWriteFence,
    ) -> Result<(), StorageError>;
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
    fence: Arc<AccountingWriteFence>,
    process: Option<accounting::HostProcessIdentity>,
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
            fence: Arc::new(AccountingWriteFence::default()),
            process: None,
        }
    }
    pub fn with_process_host(mut self, host: &dyn crate::ports::process::ProcessHost) -> Self {
        self.process = host
            .current_identity()
            .map(|p| accounting::HostProcessIdentity {
                pid: p.pid,
                birth: p.birth,
            });
        self
    }
    pub fn with_write_fence(mut self, fence: Arc<AccountingWriteFence>) -> Self {
        self.fence = fence;
        self
    }
    fn write<T, F>(&self, work: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
    {
        self.fence.write(self.store.as_ref(), work)
    }
    /// Creates the actual logical request and its canonical attempt in one write. Tariff
    /// resolution happens before this call; admission rechecks its publication fences.
    pub fn admit(
        &self,
        context: crate::context::InvocationContext,
        logical: crate::services::router::RequestAdmission,
        category: crate::services::router::accounting::ReportCategory,
        request: accounting::AttemptRequest,
    ) -> Result<Admission, StorageError> {
        self.admit_fenced(context, logical, category, request, Vec::new())
    }
    /// An auxiliary owner carries the actual authorization facts of its source operation.
    /// These are rechecked atomically before even creating its native request intent.
    pub fn admit_fenced(
        &self,
        context: crate::context::InvocationContext,
        logical: crate::services::router::RequestAdmission,
        category: crate::services::router::accounting::ReportCategory,
        request: accounting::AttemptRequest,
        source_fences: Vec<crate::ports::AuthorityFence>,
    ) -> Result<Admission, StorageError> {
        if logical.request_id != request.request_id {
            return Err(StorageError::ScopeDenied);
        }
        let router = self.router.clone();
        let control = self.control.clone();
        let process = self.process.clone();
        self.fence.admit(self.store.as_ref(), move |tx| {
            if !crate::authority::fences_hold(tx, &source_fences)? {
                return Err(StorageError::ScopeDenied);
            }
            if let Some(prior) = tx.router().route_request(&logical.request_id)? {
                let op = tx
                    .operations()
                    .operation(&prior.operation_id)?
                    .ok_or_else(|| StorageError::InvalidRecord("logical journal missing".into()))?;
                if op.args_digest != logical.args_digest
                    || prior.project_id != logical.project_id
                    || prior.session_id != logical.session_id
                    || prior.purpose != logical.purpose.as_str()
                    || op.payload.get("actorId").and_then(|a| a.as_str())
                        != Some(context.principal().0.as_str())
                    || op.payload.get("reportCategory").and_then(|a| a.as_str())
                        != Some(category.as_str())
                {
                    return Err(StorageError::ScopeDenied);
                }
            } else {
                router.admit_owned_request(tx, &context, &logical, category)?;
            }
            router.admit_attempt_with_process(tx, &control, &context, &request, process.as_ref())
        })
    }

    /// Blocking host worker entry point. The caller retains the cancellation handle for
    /// stop/shutdown while the owned source waits; no database lock spans source work.
    pub fn drive(
        &self,
        id: Id,
        source: Box<dyn OwnedInferenceSource>,
        cancellation: Cancellation,
    ) -> Result<DriveOutcome, StorageError> {
        self.drive_with_sink(id, source, None, cancellation)
    }
    pub fn drive_with_sink(
        &self,
        id: Id,
        mut source: Box<dyn OwnedInferenceSource>,
        mut sink: Option<Box<dyn OwnedInferenceSink>>,
        cancellation: Cancellation,
    ) -> Result<DriveOutcome, StorageError> {
        if cancellation.is_cancelled() {
            self.write(move |tx| {
                accounting::cancel_admitted(tx, &id, "host cancelled before transport dispatch")
            })?;
            return Ok(DriveOutcome::CancelledBeforeDispatch);
        }
        let router = self.router.clone();
        let control = self.control.clone();
        let dispatch_id = id.clone();
        let admission = self.write(move |tx| {
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
        let mut failure = source.start_cancellable(&attempt, &cancellation).err();
        if failure.is_some() {
            outcome = source.start_failure_outcome();
        }
        // Custody is checked once, before the first frame. While the stream runs the owner's
        // registry polls `cancellation_required` and cancels this handle, so a streamed frame
        // costs no store read.
        if failure.is_none() {
            match self.interrupted(&attempt) {
                Err(error) => {
                    failure = Some(error);
                    cancellation.cancel();
                }
                Ok(true) => cancellation.cancel(),
                Ok(false) => {}
            }
        }
        if failure.is_none() {
            loop {
                if cancellation.is_cancelled() {
                    break;
                }
                match source.next(&cancellation) {
                    Ok(Some(InferenceFrame::UsageDimensions(dimensions))) => {
                        let current = attempt.clone();
                        let control = self.control.clone();
                        if let Err(error) = self.write(move |tx| {
                            super::dimensions::observe(tx, &control, &current, dimensions)
                        }) {
                            failure = Some(error);
                            cancellation.cancel();
                            break;
                        }
                    }
                    Ok(Some(InferenceFrame::Usage(observation))) => {
                        let control = self.control.clone();
                        let id = id.clone();
                        if let Err(error) = self
                            .write(move |tx| accounting::observe(tx, &control, &id, &observation))
                        {
                            failure = Some(error);
                            cancellation.cancel();
                            break;
                        }
                    }
                    Ok(Some(InferenceFrame::ResponseHead(head))) => {
                        if let Some(sink) = sink.as_mut()
                            && let Err(error) = sink.response_head(&attempt, head, &self.fence)
                        {
                            failure = Some(error);
                            cancellation.cancel();
                            break;
                        }
                    }
                    Ok(Some(InferenceFrame::Output(frame))) => {
                        let result = match sink.as_mut() {
                            Some(sink) => sink.accept(&attempt, frame, &self.fence),
                            None => Err(StorageError::InvalidRecord(
                                "provider output has no trusted sink".into(),
                            )),
                        };
                        if let Err(error) = result {
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
        if cancellation.is_cancelled() {
            let current = attempt.clone();
            if let Err(error) =
                self.write(move |tx| accounting::note_cancellation_requested(tx, &current))
            {
                failure = Some(error);
            }
        }
        // A failed close leaves the reservation and inference quiescence guard in place.
        // A host restart can reconcile it once this process's exclusive data-root owner exits.
        if let Err(error) = source.close() {
            let current = attempt.clone();
            let _ = self.write(move |tx| {
                accounting::note_unclosed_owner(
                    tx,
                    &current,
                    "local inference transport closure failed",
                    outcome == AttemptEnd::UnknownTail,
                )
            });
            return Err(error);
        }
        if let Some(sink) = sink.as_mut()
            && let Err(error) = sink.finish(&attempt, outcome, &self.fence)
        {
            // Publication failure cannot erase usage or claim a successful result.
            outcome = AttemptEnd::UnknownTail;
            failure = Some(error);
        }
        let proof = LocalQuiescence {
            attempt_id: id,
            boot_id: bound.admitted_boot_id,
        };
        let control = self.control.clone();
        let balance = self.write(move |tx| accounting::settle(tx, &control, &proof, outcome))?;
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(DriveOutcome::Settled(balance))
    }
    /// Advisory cancellation for the runtime's independently polled owned transport registry.
    /// A failed/panicking driver cannot certify physical closure. Persist that fact through
    /// the same fence, so another call in the same managed turn cannot silently retry it.
    pub fn retain_unclosed(&self, id: Id) -> Result<(), StorageError> {
        self.write(move |tx| {
            let Some(attempt) = tx.usage().inference_attempt(&id)? else {
                return Ok(());
            };
            let Some(balance) = tx.usage().attempt_balance(&id)? else {
                return Ok(());
            };
            if !balance.local_quiescent
                && accounting::summary(&balance)?
                    .custody_recovery_reason
                    .is_none()
            {
                accounting::note_unclosed_owner(
                    tx,
                    &attempt,
                    "owned inference driver ended without verified transport closure",
                    true,
                )?;
            }
            Ok(())
        })
    }

    pub fn cancellation_required(&self, id: &Id) -> Result<bool, StorageError> {
        let attempt = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |s| {
            s.usage().inference_attempt(id)
        })?
        .ok_or_else(|| StorageError::InvalidRecord("owned inference attempt disappeared".into()))?;
        self.interrupted(&attempt)
    }
    fn interrupted(&self, attempt: &StoredInferenceAttempt) -> Result<bool, StorageError> {
        read_snapshot(self.store.as_ref(), &ReadOptions::default(), |s| {
            if let Some(feature) = &attempt.feature_id {
                let root = crate::protocol::RootId(feature.clone());
                if crate::services::runs::active_blockers(
                    s.runs(),
                    &BlockerFilter::RootLevel(root),
                )?
                .iter()
                .any(|b| b.cause.manual_resume())
                {
                    return Ok(true);
                }
            }
            let bound = accounting::snapshot(attempt)?;
            if attempt.work_id.is_none()
                && let Some(run_id) = &attempt.job_run_id
                && !s.jobs().job_run(run_id)?.is_some_and(|run| {
                    Some(run.revision) == bound.job_run_revision
                        && run.boot_id.as_ref() == Some(&bound.admitted_boot_id)
                        && run.status == "Running"
                        && !run.cancel_requested
                        && run.hold.is_none()
                })
            {
                return Ok(true);
            }
            if let Some(proof) = &bound.dispatch {
                let owner = match &proof.assignment_id {
                    Some(id) => s.runs().assignment(id)?,
                    None => crate::services::runs::coordinator(s.runs(), &proof.root_id)?,
                };
                if !owner.is_some_and(|a| {
                    a.generation == proof.assignment_generation
                        && a.root_id == proof.root_id
                        && a.execution_id.as_ref() == Some(&proof.execution_id)
                        && matches!(
                            a.state,
                            AssignmentState::Active
                                | AssignmentState::Waiting
                                | AssignmentState::Ready
                        )
                }) {
                    return Ok(true);
                }
                if let Some(session) = &attempt.session_id
                    && !s.runs().session_turn(session)?.is_some_and(|t| {
                        t.turn_id == proof.turn_id
                            && t.execution_id == proof.execution_id
                            && t.owner_epoch == proof.owner_epoch
                    })
                {
                    return Ok(true);
                }
            }
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

/// Owned read page; process probing happens after this snapshot has ended.
pub struct RecoveryCandidates {
    stamp: SnapshotStamp,
    boot: Id,
    rows: Vec<(StoredInferenceAttempt, Option<StoredAttemptBalance>)>,
}
pub struct PreparedRecovery {
    candidates: RecoveryCandidates,
    probes: Vec<crate::ports::process::Probe>,
}
#[derive(Clone, Debug)]
pub struct RecoveryProgress {
    pub visited: Vec<Id>,
    pub unresolved: usize,
    pub unclosed: Vec<Id>,
}
pub fn recovery_candidates(
    read: &dyn ReadSnapshot,
    boot: Id,
    after: Option<&Id>,
    limit: u16,
) -> Result<RecoveryCandidates, StorageError> {
    // Only attempts still holding local custody can need recovery; settled history is skipped
    // by the store's index instead of being paged and discarded here.
    let rows = read
        .usage()
        .unquiesced_attempts(after, limit)?
        .into_iter()
        .map(|a| {
            let balance = read.usage().attempt_balance(&a.attempt_id)?;
            Ok((a, balance))
        })
        .collect::<Result<_, StorageError>>()?;
    Ok(RecoveryCandidates {
        stamp: read.stamp().clone(),
        boot,
        rows,
    })
}
pub fn probe_recovery(
    candidates: RecoveryCandidates,
    host: Option<&dyn crate::ports::process::ProcessHost>,
) -> Result<PreparedRecovery, StorageError> {
    use crate::ports::process::Probe;
    let probes = candidates
        .rows
        .iter()
        .map(|(attempt, balance)| {
            let bound = accounting::snapshot(attempt)?;
            if bound.admitted_boot_id == candidates.boot
                || balance.as_ref().is_some_and(|b| b.local_quiescent)
            {
                return Ok(Probe::Unknown("already current or closed".into()));
            }
            Ok(
                match (host, bound.host_process.as_ref().and_then(|p| p.native())) {
                    (Some(host), Some(identity)) => host.probe(&identity),
                    _ => Probe::Unknown(
                        "native host identity or process probe was unavailable".into(),
                    ),
                },
            )
        })
        .collect::<Result<_, StorageError>>()?;
    Ok(PreparedRecovery { candidates, probes })
}
pub fn apply_recovery(
    tx: &mut dyn WriteTransaction,
    control: &ControlService,
    page: PreparedRecovery,
) -> Result<RecoveryProgress, StorageError> {
    use crate::ports::process::Probe;
    if tx.store().store_id != page.candidates.stamp.store_id
        || tx.store().restore_epoch != page.candidates.stamp.restore_epoch
        || tx.store().boot_id.as_ref() != Some(&page.candidates.boot)
    {
        return Err(StorageError::ScopeDenied);
    }
    let mut result = RecoveryProgress {
        visited: Vec::with_capacity(page.candidates.rows.len()),
        unresolved: 0,
        unclosed: Vec::new(),
    };
    for ((prepared, balance), probe) in page.candidates.rows.into_iter().zip(page.probes) {
        result.visited.push(prepared.attempt_id.clone());
        let bound = accounting::snapshot(&prepared)?;
        if bound.admitted_boot_id == page.candidates.boot
            || balance.as_ref().is_some_and(|b| b.local_quiescent)
        {
            continue;
        }
        let Some(current) = tx.usage().inference_attempt(&prepared.attempt_id)? else {
            continue;
        };
        let current_balance = tx.usage().attempt_balance(&current.attempt_id)?;
        if current_balance.as_ref().is_some_and(|b| b.local_quiescent) {
            continue;
        }
        if current != prepared
            || current_balance.as_ref().map(|b| b.revision) != balance.as_ref().map(|b| b.revision)
        {
            result.unresolved += 1;
            result.unclosed.push(current.attempt_id.clone());
            continue;
        }
        if probe != Probe::Gone {
            let reason = match probe {
                Probe::Alive => "the admitted host process is still alive",
                Probe::Unknown(_) => "the admitted host process cannot be verified gone",
                Probe::Gone => unreachable!(),
            };
            accounting::note_unclosed_owner(tx, &current, reason, true)?;
            result.unresolved += 1;
            result.unclosed.push(current.attempt_id.clone());
            continue;
        }
        if current.state == "Admitted" {
            accounting::cancel_admitted(
                tx,
                &current.attempt_id,
                "the admitted host process is verified gone before dispatch",
            )?;
        } else if matches!(current.state.as_str(), "Dispatched" | "UnknownTail") {
            accounting::settle(
                tx,
                control,
                &LocalQuiescence {
                    attempt_id: current.attempt_id,
                    boot_id: bound.admitted_boot_id,
                },
                AttemptEnd::UnknownTail,
            )?;
        } else {
            result.unresolved += 1;
        }
    }
    Ok(result)
}
