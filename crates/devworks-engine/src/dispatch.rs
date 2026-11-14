//! The supervised durable-delivery dispatcher.
//!
//! A pass ([`Dispatcher::tick`]) repairs sleep/wake lease timing, recovers expired claims, then
//! claims at most as many ready deliveries as it has free worker permits and runs each on its own
//! task. [`Dispatcher::run`] repeats passes, sleeping until a coalesced wakeup, the next saved
//! ready/lease time, or the sweep interval — whichever comes first — so a lost wakeup is repaired
//! within one sweep interval. Before the first claim of a boot it runs [`Dispatcher::recover`].
//!
//! Per delivery (one consumer, one event):
//!
//! * **Database handler.** One write transaction: re-check the claim fence and cancellation
//!   epoch, run [`DeliveryHandler::apply`] inside a savepoint, then settle, retry with durable
//!   backoff, fail terminally (poison) or hold — the handler's writes and the outcome commit
//!   together. A handler error or panic rolls its writes back and schedules a retry.
//! * **External effect** (consumer with an [`EffectExecutor`] and a delivery with an operation).
//!   Transaction A starts an attempt fenced by the claim generation and commits `Dispatching`
//!   *before* the effect; the effect then runs outside any transaction while the lease is
//!   renewed; transaction B records the observed outcome and settles, retries (`NotApplied`),
//!   fails (`Failed`) or holds (`Unknown`). A crash between A and B leaves `Dispatching`, which
//!   restart classifies `OutcomeUnknown`: it is observed, never re-executed, and only an explicit
//!   `queue.reconcile` (or `queue.retry` once safe) redelivers it. If the operation has already
//!   completed (reconciled), the delivery only runs its handler; it never dispatches again. If B
//!   itself cannot be recorded, the operation is marked `OutcomeUnknown` and the delivery held.
//! * **Asynchronous handler** (consumer with an [`AsyncDeliveryHandler`], for example a package
//!   process). The claim stays live with a renewed lease while the handler runs outside any
//!   transaction; the handler commits later through a transactional commit path that checks the
//!   claim fence and cancellation epoch (`package::commit_event`). A cancellation request is
//!   observed at each renewal or wakeup and signalled to the handler; a commit after it is
//!   refused, and the dispatcher settles the delivery `Cancelled`. Dispatcher shutdown signals
//!   the handler the same way. A handler that ignores the signal gets its consumer's stop grace
//!   (store clock); past it the dispatcher stops supervising and renewing it, and the claim's
//!   lease expires into ordinary recovery (which honours a cancellation request). Such a
//!   consumer never carries operations: an operation-bearing delivery fails visibly instead,
//!   so nothing it handles can be possibly dispatched.
//!
//! **Cancellation** is cooperative and durable. A request against a claimed delivery raises its
//! epoch and sets a marker the store keeps through holds, retries, requeues and claim recovery.
//! The owner observes it at its next check (before applying, before dispatching, and when an
//! effect reports `NotApplied`) and settles the delivery `Cancelled` with its undispatched
//! operation. Once an effect may have dispatched, the request cannot take it back: its outcome is
//! recorded from evidence and reconciled, and an effect is never dispatched again after the
//! request.
//!
//! **Root eligibility.** Root-scoped work recovered from an earlier boot is held, not continued
//! automatically; a consumer's [`RootEligibility`] (filled by the root-control layer) confirms
//! which of it may resume. Without one it waits for an operator retry. The same port gates every
//! claimed root-scoped delivery in its own fenced transaction before it is applied or
//! dispatched ([`RootEligibility::may_dispatch`]): work of a paused, held or stopped root is held
//! awaiting eligibility and resumes only after the root does.
//!
//! **Sleep/wake detection** compares the wall clock with Tokio's monotonic clock. On Linux
//! (`CLOCK_MONOTONIC`) and macOS (`mach_absolute_time`) the monotonic clock stops during system
//! sleep, so a resume shows as a wall-clock jump and live leases are reasserted before recovery.
//! Where the monotonic clock keeps counting through suspend (Windows `QueryPerformanceCounter`
//! on current releases, Linux builds whose monotonic source includes suspend), no jump is seen.
//! The fallback is then ordinary expired-lease recovery, which is safe: a live effect's delivery
//! is held for reconciliation (its operation is possibly dispatched), the late result is
//! recorded against the operation's attempt fence, and the effect is never re-executed.
use crate::store;
use devworks_core::ports::Clock;
use devworks_core::ports::storage::{
    ApplicationStore, AttemptBinding, DeliveryFilter, DeliveryKey, EventRecord, OperationRecord,
    QueueEntry, QueueFence, QueueState, RecoveryOutcome, Settlement, StorageError,
    WriteTransaction, cancellation_applies, possibly_dispatched,
};
use devworks_core::protocol::{Id, OperationId, OperationState};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, watch};
use tokio::time::Instant;

/// Durable, cause-specific retry policy of one consumer (deterministic exponential backoff).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    pub base_delay_ms: i64,
    pub max_delay_ms: i64,
    /// Claims allowed before a transient failure becomes terminal (`Failed`, visible).
    pub max_attempts: i64,
}
impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            base_delay_ms: 1_000,
            max_delay_ms: 300_000,
            max_attempts: 8,
        }
    }
}
impl RetryPolicy {
    /// Delay after the `attempt`-th claim failed: `base * 2^(attempt-1)`, capped.
    pub fn delay_for(&self, attempt: i64) -> i64 {
        let exp = attempt.clamp(1, 32) - 1;
        self.base_delay_ms
            .saturating_mul(1_i64 << exp)
            .min(self.max_delay_ms)
            .max(0)
    }
}

/// One claimed delivery and its committed event, read inside the handler's transaction.
#[derive(Clone, Debug)]
pub struct Delivery {
    pub entry: QueueEntry,
    pub event: EventRecord,
}
impl Delivery {
    /// The current claim fence (the entry is claimed while a handler runs).
    pub fn fence(&self) -> QueueFence {
        self.entry.fence().expect("a delivered entry is claimed")
    }
}

/// What a handler decided for its delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Applied: commit the handler's writes and settle the delivery.
    Completed,
    /// The handler settled the delivery itself through a transactional commit path (for example
    /// [`crate::package::commit_event`]); commit its writes as they are.
    Committed,
    /// Transient: discard the handler's writes and retry after durable backoff.
    Retry(String),
    /// Deterministic (poison): discard the writes; visible terminal `Failed`.
    Poison(String),
    /// Cannot be interpreted now (unknown schema/upcaster failure): discard the writes and hold
    /// the delivery with diagnostics; never acknowledge it.
    Hold(String),
}

/// A durable handler for one consumer. It runs synchronously on the write owner inside the
/// delivery's transaction: no awaiting, network, inference or package/UI callbacks. Delivery is
/// at least once, so application must be idempotent per `(consumer, event)`.
pub trait DeliveryHandler: Send + Sync {
    fn apply(
        &self,
        tx: &mut dyn WriteTransaction,
        delivery: &Delivery,
    ) -> Result<Outcome, StorageError>;
}

/// Observed outcome of an external effect.
#[derive(Clone, Debug, PartialEq)]
pub enum EffectResult {
    /// Evidence read from the target that the effect happened.
    Completed(Value),
    /// The target reports a deterministic failure; nothing to retry.
    Failed(String),
    /// Evidence that the effect did not happen; safe to retry under current policy.
    NotApplied(String),
    /// The outcome cannot be established (lost response, timeout, opaque effect).
    Unknown(String),
}
pub type EffectFuture = Pin<Box<dyn Future<Output = EffectResult> + Send>>;
/// Resolves once an operation may be dispatched (see [`EffectExecutor::ready`]).
pub type ReadyFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// External effect execution and observation for one consumer's operations.
pub trait EffectExecutor: Send + Sync {
    /// Performs the effect of a `Dispatching` operation once.
    fn execute(&self, operation: OperationRecord) -> EffectFuture;
    /// Observes the target for an operation whose outcome is unknown using its saved identity;
    /// it must not perform the effect.
    fn observe(&self, operation: OperationRecord) -> EffectFuture;
    /// Resolves once `operation` may be dispatched, for example once the effect it is ordered
    /// behind has settled. Until then the dispatcher keeps the delivery's claim (renewing its
    /// lease) but releases its worker permit, so waiting work never takes capacity from other
    /// deliveries; waiting is not an attempt. `None`, the default, dispatches at once.
    fn ready(&self, _operation: &OperationRecord) -> Option<ReadyFuture> {
        None
    }
}

/// What an [`AsyncDeliveryHandler`] reports when it finishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AsyncOutcome {
    /// The handler committed through its transactional commit path (which settles the
    /// delivery); an identical replay that did not settle is settled `Completed`.
    Committed,
    /// Transient: retry after durable backoff.
    Retry(String),
    /// Deterministic (poison): visible terminal `Failed`.
    Poison(String),
    /// Hold the delivery with diagnostics.
    Hold(String),
}
pub type AsyncFuture = Pin<Box<dyn Future<Output = AsyncOutcome> + Send>>;

/// One claimed delivery handed to an [`AsyncDeliveryHandler`]. Its entry carries the claim fence
/// and cancellation epoch the handler's commit must present; `cancelled` turns `true` when the
/// dispatcher observes a cancellation request while the handler runs.
pub struct AsyncDelivery {
    pub delivery: Delivery,
    pub cancelled: watch::Receiver<bool>,
}

/// A handler that runs outside the write owner (for example a package process) and commits
/// later through a fenced transactional commit path. The dispatcher keeps the claim's lease
/// renewed while it runs.
pub trait AsyncDeliveryHandler: Send + Sync {
    fn start(&self, delivery: AsyncDelivery) -> AsyncFuture;
}

/// Decides whether root-scoped work recovered from an earlier boot may continue automatically
/// (for example: its root is not paused or held and its recovery class allows automatic
/// continuation). Filled by the root-control layer; it runs inside the dispatcher's transaction.
/// Held work is re-examined only after boot recovery or a [`Wakeup::wake`], one bounded page per
/// consumer per pass, so the root-control layer wakes the dispatcher when eligibility changes.
pub trait RootEligibility: Send + Sync {
    fn may_resume(
        &self,
        tx: &mut dyn WriteTransaction,
        entry: &QueueEntry,
    ) -> Result<bool, StorageError>;
    /// The root gate checked in the delivery's own fenced transaction before its handler runs
    /// or its effect is dispatched (`BeginDispatch` ordering): `false` holds the claimed
    /// root-scoped delivery awaiting eligibility instead, so a pause, stop or restart hold
    /// committed earlier is never bypassed by queued work. The resume scan requeues it once
    /// [`Self::may_resume`] allows. The default admits everything.
    fn may_dispatch(
        &self,
        _tx: &mut dyn WriteTransaction,
        _entry: &QueueEntry,
    ) -> Result<bool, StorageError> {
        Ok(true)
    }
}

/// A registered consumer. `name` is an opaque identifier (it is the wire `consumerId`).
#[derive(Clone)]
pub struct Consumer {
    pub name: String,
    pub handler: Arc<dyn DeliveryHandler>,
    pub effect: Option<Arc<dyn EffectExecutor>>,
    /// When set, deliveries run through this asynchronous handler instead of `handler`.
    pub async_handler: Option<Arc<dyn AsyncDeliveryHandler>>,
    /// Confirms which root-scoped work recovered from an earlier boot may resume. `None` (the
    /// safe default) leaves it held until an operator retries it.
    pub eligibility: Option<Arc<dyn RootEligibility>>,
    /// Cooperative grace (store-clock ms) an asynchronous handler gets after it is asked to stop
    /// (a cancellation request or dispatcher shutdown). Past it the dispatcher stops supervising
    /// the handler and renewing its lease. 5 000 ms matches the architecture's shutdown grace.
    pub stop_grace_ms: i64,
    pub retry: RetryPolicy,
    /// Most deliveries of this consumer in flight at once, so a burst of one consumer cannot
    /// take every worker permit from the others. `None` (the permissive default) lets it use
    /// the dispatcher's whole capacity.
    pub max_in_flight: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct DispatcherConfig {
    /// This dispatcher's owner identity recorded on its claims.
    pub owner: Id,
    pub lease_ms: i64,
    /// Worker capacity: the most deliveries in flight at once.
    pub max_in_flight: usize,
    /// Most deliveries claimed per pass (1..=500).
    pub claim_batch: u16,
    /// Upper bound on the time between passes (repairs lost wakeups).
    pub sweep_interval: Duration,
    /// A wall-clock advance exceeding the monotonic advance by more than this is a sleep/wake.
    pub wake_jump_ms: i64,
    /// Rows per recovery or eligibility transaction, and per page when observing unknown
    /// outcomes (1..=500).
    pub recovery_batch: u16,
}
impl DispatcherConfig {
    pub fn new(owner: Id) -> Self {
        Self {
            owner,
            lease_ms: 30_000,
            max_in_flight: 4,
            claim_batch: 64,
            sweep_interval: Duration::from_secs(1),
            wake_jump_ms: 10_000,
            recovery_batch: 100,
        }
    }
}

/// What boot recovery established.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    pub classified: Vec<(OperationId, OperationState)>,
    pub requeued: Vec<Id>,
    pub held: Vec<Id>,
    /// Prior-boot claims settled `Cancelled` because their cancellation was requested.
    pub cancelled: Vec<Id>,
}

/// What one pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    /// A sleep/wake (wall-clock jump) was detected and repaired.
    pub woke: bool,
    pub reasserted: usize,
    pub recovered: RecoveryOutcome,
    /// Held root-scoped deliveries whose eligibility was confirmed and that were requeued.
    pub resumed: usize,
    pub claimed: usize,
}

/// Best-effort, coalescing post-commit wakeup. It also prompts running asynchronous handlers'
/// supervisors to renew their lease and look for a cancellation request now, and has the next
/// pass re-examine held root-scoped work from the start.
#[derive(Clone)]
pub struct Wakeup {
    pass: Arc<Notify>,
    changed: Arc<Notify>,
    resume: Arc<ResumeScan>,
}
impl Wakeup {
    pub fn wake(&self) {
        self.resume.restart();
        self.pass.notify_one();
        self.changed.notify_waiters();
    }
}

/// When held root-scoped work is examined: after boot recovery or a wakeup, continuing from a
/// per-consumer cursor while pages remain. A restart invalidates the cursors.
struct ResumeScan {
    due: AtomicBool,
    generation: AtomicU64,
    cursors: Mutex<(u64, HashMap<String, Id>)>,
}
impl ResumeScan {
    fn restart(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.due.store(true, Ordering::SeqCst);
    }
}

struct Inner {
    store: Arc<dyn ApplicationStore>,
    clock: Arc<dyn Clock>,
    consumers: Vec<Consumer>,
    cfg: DispatcherConfig,
    permits: Arc<Semaphore>,
    wake: Arc<Notify>,
    changed: Arc<Notify>,
    resume: Arc<ResumeScan>,
    status: watch::Sender<u64>,
    in_flight: Mutex<HashMap<Id, QueueFence>>,
    /// Deliveries in flight per consumer (index into `consumers`).
    consumer_load: Mutex<Vec<usize>>,
    last_tick: Mutex<Option<(Instant, i64)>>,
    recovered: AtomicBool,
    stopping: AtomicBool,
    rotation: AtomicUsize,
    passes: AtomicU64,
    /// Passes in a row that failed (reset by a pass that succeeds).
    failed_passes: AtomicU64,
    /// The latest failed pass, or delivery failure that could not be recorded (diagnostics).
    last_failure: Mutex<Option<String>>,
    /// Claimed deliveries waiting for their operation to become ready, without a permit.
    parked: AtomicUsize,
}

/// What one running delivery holds; released on drop (normal return or a panic unwinding its
/// task), so a failing handler never leaks capacity.
struct Slot {
    inner: Arc<Inner>,
    id: Id,
    idx: usize,
    permit: Option<OwnedSemaphorePermit>,
}
impl Drop for Slot {
    fn drop(&mut self) {
        self.inner
            .in_flight
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.id);
        self.inner
            .consumer_load
            .lock()
            .unwrap_or_else(|p| p.into_inner())[self.idx] -= 1;
        drop(self.permit.take());
        self.inner.status.send_modify(|v| *v = v.wrapping_add(1));
        // Capacity freed: more ready work may be claimable now.
        self.inner.wake.notify_one();
    }
}

/// The dispatcher. Cheap to clone; clones share state.
#[derive(Clone)]
pub struct Dispatcher {
    inner: Arc<Inner>,
}

/// Read-only diagnostics must not keep consumers (and their services) alive.
pub(crate) struct DispatcherObserver(Weak<Inner>);

/// What the dispatcher health source reports.
pub(crate) struct DispatcherMetrics {
    pub in_flight: usize,
    pub passes: u64,
    pub failed_passes: u64,
    pub last_failure: Option<String>,
}

impl DispatcherObserver {
    pub(crate) fn metrics(&self) -> Option<DispatcherMetrics> {
        let dispatcher = Dispatcher {
            inner: self.0.upgrade()?,
        };
        Some(DispatcherMetrics {
            in_flight: dispatcher.in_flight(),
            passes: dispatcher.passes(),
            failed_passes: dispatcher.failed_passes(),
            last_failure: dispatcher.last_failure(),
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// The claim is no longer ours (recovered, reassigned or settled elsewhere).
    Lost,
    Settled(QueueState),
}

fn invalid(m: &str) -> StorageError {
    StorageError::InvalidRecord(m.into())
}

impl Dispatcher {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        clock: Arc<dyn Clock>,
        consumers: Vec<Consumer>,
        cfg: DispatcherConfig,
    ) -> Result<Self, StorageError> {
        if cfg.lease_ms <= 0
            || cfg.max_in_flight == 0
            || !(1..=500).contains(&cfg.claim_batch)
            || !(1..=500).contains(&cfg.recovery_batch)
        {
            return Err(invalid("dispatcher bounds"));
        }
        let mut names = std::collections::HashSet::new();
        for c in &consumers {
            Id::new(c.name.as_str())
                .map_err(|_| invalid("consumer names are opaque identifiers"))?;
            if !names.insert(c.name.clone()) {
                return Err(invalid("duplicate consumer"));
            }
            if c.retry.max_attempts <= 0 || c.retry.base_delay_ms < 0 {
                return Err(invalid("retry policy bounds"));
            }
            if c.stop_grace_ms < 0 {
                return Err(invalid("stop grace must be non-negative"));
            }
            if c.max_in_flight == Some(0) {
                return Err(invalid("a consumer in-flight cap must be positive"));
            }
            if c.async_handler.is_some() && c.effect.is_some() {
                return Err(invalid(
                    "an asynchronous consumer commits through its own path, not an effect executor",
                ));
            }
        }
        let (status, _) = watch::channel(0);
        let consumers_len = consumers.len();
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                clock,
                permits: Arc::new(Semaphore::new(cfg.max_in_flight)),
                consumers,
                cfg,
                wake: Arc::new(Notify::new()),
                changed: Arc::new(Notify::new()),
                resume: Arc::new(ResumeScan {
                    due: AtomicBool::new(true),
                    generation: AtomicU64::new(0),
                    cursors: Mutex::new((0, HashMap::new())),
                }),
                status,
                in_flight: Mutex::new(HashMap::new()),
                consumer_load: Mutex::new(vec![0; consumers_len]),
                last_tick: Mutex::new(None),
                recovered: AtomicBool::new(false),
                stopping: AtomicBool::new(false),
                rotation: AtomicUsize::new(0),
                passes: AtomicU64::new(0),
                failed_passes: AtomicU64::new(0),
                last_failure: Mutex::new(None),
                parked: AtomicUsize::new(0),
            }),
        })
    }

    /// Handle producers call after committing new deliveries (best effort).
    pub fn wakeup(&self) -> Wakeup {
        Wakeup {
            pass: self.inner.wake.clone(),
            changed: self.inner.changed.clone(),
            resume: self.inner.resume.clone(),
        }
    }

    /// "Queue state may have changed" hints for UI; repair a missed hint with a durable query.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.inner.status.subscribe()
    }

    pub(crate) fn observer(&self) -> DispatcherObserver {
        DispatcherObserver(Arc::downgrade(&self.inner))
    }

    /// Deliveries currently being handled by this dispatcher.
    pub fn in_flight(&self) -> usize {
        self.inner
            .in_flight
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    /// Passes completed by [`Dispatcher::run`] (diagnostics).
    pub fn passes(&self) -> u64 {
        self.inner.passes.load(Ordering::SeqCst)
    }

    /// Passes of [`Dispatcher::run`] in a row that failed; 0 once one succeeds (diagnostics).
    pub fn failed_passes(&self) -> u64 {
        self.inner.failed_passes.load(Ordering::SeqCst)
    }

    /// The latest failed pass, or delivery failure that could not be recorded (diagnostics).
    pub fn last_failure(&self) -> Option<String> {
        self.inner
            .last_failure
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn note_failure(&self, failure: String) {
        *self
            .inner
            .last_failure
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(store::bounded(&failure));
    }

    /// Stops admission; in-flight deliveries finish (see [`Dispatcher::idle`]). Running
    /// asynchronous handlers are asked to stop and get their consumer's stop grace.
    pub fn stop(&self) {
        self.inner.stopping.store(true, Ordering::SeqCst);
        self.inner.wake.notify_one();
        self.inner.changed.notify_waiters();
    }

    /// Boot recovery, run before any claim of this boot: interrupted operations are classified
    /// (`Preflighted` → `RetryPending`, `Dispatching` → `OutcomeUnknown`), then claims of earlier
    /// boots are requeued or, when their operation may have dispatched, held in `Reconciling`.
    /// Idempotent and bounded per transaction, with durable progress.
    pub async fn recover(&self) -> Result<RecoveryReport, StorageError> {
        let limit = self.inner.cfg.recovery_batch;
        let mut report = RecoveryReport::default();
        loop {
            let batch = store::write(&self.inner.store, move |tx| {
                store::classify_interrupted(tx, limit)
            })
            .await?;
            let n = batch.len();
            report.classified.extend(batch);
            if n < usize::from(limit) {
                break;
            }
        }
        loop {
            let out = store::write(&self.inner.store, move |tx| {
                store::recover_prior_boots(tx, limit)
            })
            .await?;
            let n = out.requeued.len() + out.held.len() + out.cancelled.len();
            report.requeued.extend(out.requeued);
            report.held.extend(out.held);
            report.cancelled.extend(out.cancelled);
            if n < usize::from(limit) {
                break;
            }
        }
        self.inner.recovered.store(true, Ordering::SeqCst);
        // Recovery may have held root-scoped work: examine it from the start.
        self.inner.resume.restart();
        self.notify_status();
        Ok(report)
    }

    /// One dispatch pass.
    pub async fn tick(&self) -> Result<TickReport, StorageError> {
        let inner = &self.inner;
        if !inner.recovered.load(Ordering::SeqCst) {
            self.recover().await?;
        }
        let mut report = TickReport::default();
        if inner.stopping.load(Ordering::SeqCst) {
            return Ok(report);
        }
        // Sleep/wake: the wall clock advanced much more than the monotonic clock. Re-establish
        // the leases of work this live process still runs before expired-claim recovery could
        // reassign it; claims nobody runs are then recovered normally.
        let wall = inner.clock.now_ms().get();
        let mono = Instant::now();
        report.woke = {
            let mut last = inner.last_tick.lock().unwrap_or_else(|p| p.into_inner());
            let woke = last.is_some_and(|(m0, w0)| {
                let mono_ms =
                    i64::try_from(mono.duration_since(m0).as_millis()).unwrap_or(i64::MAX);
                (wall - w0).saturating_sub(mono_ms) > inner.cfg.wake_jump_ms
            });
            *last = Some((mono, wall));
            woke
        };
        if report.woke {
            let mine: Vec<(Id, QueueFence)> = inner
                .in_flight
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let lease = inner.cfg.lease_ms;
            report.reasserted = store::write(&inner.store, move |tx| {
                let mut n = 0;
                for (id, fence) in &mine {
                    match tx.queue().reassert(id, fence, lease) {
                        Ok(_) => n += 1,
                        Err(StorageError::LeaseLost { .. } | StorageError::NotFound { .. }) => {}
                        Err(e) => return Err(e),
                    }
                }
                Ok(n)
            })
            .await?;
        }
        let limit = inner.cfg.recovery_batch;
        report.recovered =
            store::write(&inner.store, move |tx| store::recover_expired(tx, limit)).await?;
        report.resumed = self.resume_eligible().await?;

        // Claim only within free worker capacity.
        let mut permits: Vec<OwnedSemaphorePermit> = Vec::new();
        while permits.len() < usize::from(inner.cfg.claim_batch) {
            match inner.permits.clone().try_acquire_owned() {
                Ok(p) => permits.push(p),
                Err(_) => break,
            }
        }
        if permits.is_empty() || inner.consumers.is_empty() {
            return Ok(report);
        }
        let budget = permits.len();
        let start = inner.rotation.fetch_add(1, Ordering::Relaxed) % inner.consumers.len();
        // Each consumer may claim up to its own in-flight cap. The room is reserved before the
        // claim, so concurrent passes never exceed a cap together; what is not claimed is
        // released right after.
        let order: Vec<(usize, String, usize)> = {
            let mut load = inner
                .consumer_load
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            (0..inner.consumers.len())
                .map(|i| (start + i) % inner.consumers.len())
                .map(|i| {
                    let c = &inner.consumers[i];
                    let room = c
                        .max_in_flight
                        .map_or(budget, |cap| cap.saturating_sub(load[i]).min(budget));
                    load[i] += room;
                    (i, c.name.clone(), room)
                })
                .collect()
        };
        let reserved: Vec<(usize, usize)> = order.iter().map(|(i, _, r)| (*i, *r)).collect();
        let (owner, lease) = (inner.cfg.owner.clone(), inner.cfg.lease_ms);
        let claiming = store::write(&inner.store, move |tx| {
            let mut out = Vec::new();
            for (idx, name, room) in &order {
                let left = budget - out.len();
                if left == 0 {
                    break;
                }
                if *room == 0 {
                    continue;
                }
                let n = u16::try_from(left.min(*room)).unwrap_or(500).min(500);
                for e in store::claim(tx, name, &owner, lease, n)? {
                    out.push((*idx, e));
                }
            }
            Ok(out)
        })
        .await;
        {
            // Release the reservation: each claimed delivery keeps one unit, held by its slot.
            let mut load = inner
                .consumer_load
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            for (idx, room) in reserved {
                let kept = claiming
                    .as_ref()
                    .map_or(0, |c| c.iter().filter(|(i, _)| *i == idx).count());
                load[idx] -= room - kept;
            }
        }
        let claimed = claiming?;
        report.claimed = claimed.len();
        for ((idx, entry), permit) in claimed.into_iter().zip(permits) {
            if let Some(f) = entry.fence() {
                inner
                    .in_flight
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(entry.id.clone(), f);
            }
            // The slot releases the delivery's worker permit, in-flight entry and consumer load
            // when the task ends, whether the delivery returns or its handler panics.
            let slot = Slot {
                inner: inner.clone(),
                id: entry.id.clone(),
                idx,
                permit: Some(permit),
            };
            let me = self.clone();
            tokio::spawn(async move {
                let mut slot = slot;
                if me.park(idx, &entry, &mut slot).await {
                    me.deliver(idx, entry).await;
                }
            });
        }
        if report.claimed > 0 {
            self.notify_status();
        }
        Ok(report)
    }

    /// Requeues held root-scoped work recovered from an earlier boot once each consumer's
    /// [`RootEligibility`] confirms it. It runs only after boot recovery or a wakeup, examines
    /// one bounded page per consumer per pass (continuing from a cursor on later passes while
    /// pages remain), and counts only the work that became ready: a requeue that settles a
    /// cancelled delivery is not a resumption. Work whose operation may have dispatched stays
    /// held for reconciliation whatever the port says.
    async fn resume_eligible(&self) -> Result<usize, StorageError> {
        let scan = &self.inner.resume;
        if !scan.due.swap(false, Ordering::SeqCst) {
            return Ok(0);
        }
        let generation = scan.generation.load(Ordering::SeqCst);
        let mut cursors = {
            let mut c = scan.cursors.lock().unwrap_or_else(|p| p.into_inner());
            if c.0 != generation {
                *c = (generation, HashMap::new());
            }
            c.1.clone()
        };
        let batch = self.inner.cfg.recovery_batch;
        let mut resumed = 0;
        let mut unfinished = false;
        for consumer in &self.inner.consumers {
            let Some(port) = consumer.eligibility.clone() else {
                continue;
            };
            let (name, from) = (consumer.name.clone(), cursors.get(&consumer.name).cloned());
            let (n, last, more) = store::write(&self.inner.store, move |tx| {
                let page = tx
                    .queue()
                    .awaiting_eligibility(&name, from.as_ref(), batch)?;
                let more = page.len() == usize::from(batch);
                let mut n = 0;
                for e in &page {
                    if let Some(op) = &e.operation_id
                        && tx
                            .operations()
                            .operation(op)?
                            .is_some_and(|o| possibly_dispatched(o.state))
                    {
                        continue;
                    }
                    if port.may_resume(tx, e)?
                        && tx.queue().requeue(&e.id, 0)?.state == QueueState::Ready
                    {
                        n += 1;
                    }
                }
                Ok((n, page.last().map(|e| e.id.clone()), more))
            })
            .await?;
            resumed += n;
            match (more, last) {
                (true, Some(last)) => {
                    cursors.insert(consumer.name.clone(), last);
                    unfinished = true;
                }
                _ => {
                    cursors.remove(&consumer.name);
                }
            }
        }
        {
            let mut c = scan.cursors.lock().unwrap_or_else(|p| p.into_inner());
            // A restart meanwhile invalidated these cursors; the next pass starts over.
            if c.0 == generation {
                c.1 = cursors;
            }
        }
        if unfinished {
            scan.due.store(true, Ordering::SeqCst);
        }
        if resumed > 0 {
            self.notify_status();
        }
        Ok(resumed)
    }

    /// Waits until no delivery is in flight (parked deliveries included).
    pub async fn idle(&self) {
        let n = u32::try_from(self.inner.cfg.max_in_flight).unwrap_or(u32::MAX);
        loop {
            let mut changed = self.inner.status.subscribe();
            if let Ok(all) = self.inner.permits.acquire_many(n).await {
                drop(all);
            }
            if self.inner.parked.load(Ordering::SeqCst) == 0 {
                return;
            }
            let _ = tokio::time::timeout(Duration::from_millis(20), changed.changed()).await;
        }
    }

    /// Holds a claimed effect delivery until its executor reports its operation ready, without
    /// a worker permit: the permit is released while it waits (more work may be claimed) and
    /// reacquired before delivery. The claim's lease is renewed for the whole wait, the
    /// readiness wait and the reacquisition of a permit alike (every permit may be taken for
    /// longer than a lease). Returns `false` when the dispatcher stops or the claim is lost
    /// while waiting: the operation was never dispatched, so the claim is recovered as safely
    /// redeliverable.
    async fn park(&self, idx: usize, entry: &QueueEntry, slot: &mut Slot) -> bool {
        let consumer = &self.inner.consumers[idx];
        let (None, Some(executor), Some(op_id)) = (
            &consumer.async_handler,
            &consumer.effect,
            &entry.operation_id,
        ) else {
            return true;
        };
        let id = op_id.clone();
        let Ok(Some(op)) =
            store::read(&self.inner.store, move |r| r.operations().operation(&id)).await
        else {
            return true;
        };
        if !matches!(
            op.state,
            OperationState::IntentRecorded | OperationState::RetryPending
        ) {
            return true;
        }
        let Some(ready) = executor.ready(&op) else {
            return true;
        };
        let Some(fence) = entry.fence() else {
            return true;
        };
        drop(slot.permit.take());
        self.inner.parked.fetch_add(1, Ordering::SeqCst);
        // Capacity freed: other ready work may be claimed while this one waits.
        self.inner.wake.notify_one();
        let lease = self.inner.cfg.lease_ms;
        let every = Duration::from_millis(u64::try_from((lease / 3).max(1)).unwrap_or(1));
        // Ready, then a worker permit: one wait, so the lease is renewed (and a stop or a lost
        // claim honoured) until the delivery can actually run.
        let permits = self.inner.permits.clone();
        let mut admitted = std::pin::pin!(async move {
            ready.await;
            permits.acquire_owned().await.ok()
        });
        let mut proceed = true;
        let mut renew_at = tokio::time::Instant::now() + every;
        loop {
            if self.inner.stopping.load(Ordering::SeqCst) {
                proceed = false;
                break;
            }
            // Wakeups may arrive continuously. Preserve the renewal deadline across them
            // instead of restarting a full interval after each notification.
            let pause = first(
                self.inner.changed.notified(),
                tokio::time::sleep_until(renew_at),
            );
            match first(&mut admitted, pause).await {
                Either::Left(Some(p)) => {
                    slot.permit = Some(p);
                    break;
                }
                Either::Left(None) => {
                    proceed = false;
                    break;
                }
                Either::Right(_) => {
                    if tokio::time::Instant::now() < renew_at {
                        continue;
                    }
                    let (id, f) = (entry.id.clone(), fence.clone());
                    let renewed = store::write(&self.inner.store, move |tx| {
                        tx.queue().renew(&id, &f, lease)
                    })
                    .await;
                    if renewed.is_err() {
                        proceed = false;
                        break;
                    }
                    renew_at = tokio::time::Instant::now() + every;
                }
            }
        }
        self.inner.parked.fetch_sub(1, Ordering::SeqCst);
        self.notify_status();
        proceed && !self.inner.stopping.load(Ordering::SeqCst)
    }

    /// Delivers everything currently eligible: passes until one, made while no delivery was in
    /// flight, claims nothing (a pass made while deliveries ran may have claimed nothing only
    /// because a consumer was at its in-flight cap). Returns the number of claims made.
    pub async fn run_until_idle(&self) -> Result<usize, StorageError> {
        let mut total = 0;
        loop {
            let busy = self.in_flight() > 0;
            let r = self.tick().await?;
            total += r.claimed;
            self.idle().await;
            if r.claimed == 0 && !busy {
                return Ok(total);
            }
        }
    }

    /// The supervised loop: passes until [`Dispatcher::stop`], each followed by a bounded wait
    /// for a wakeup, the next saved ready/lease time, or the sweep interval.
    pub async fn run(&self) {
        self.run_with_sweep(tokio::time::sleep).await;
    }

    /// [`Dispatcher::run`] with an injected sweep timer: after each pass, `sweep(wait)` must
    /// resolve once the next pass is due without a wakeup, where `wait` is the sweep interval or
    /// the time until the next saved ready/lease time, whichever is shorter. Production uses a
    /// Tokio sleep; tests drive the sweep explicitly.
    pub async fn run_with_sweep<S, F>(&self, mut sweep: S)
    where
        S: FnMut(Duration) -> F + Send,
        F: Future<Output = ()> + Send,
    {
        let inner = &self.inner;
        while !inner.stopping.load(Ordering::SeqCst) {
            // A failed pass (store busy or unavailable) is retried after the sweep interval; it
            // is counted and kept for the health check until a pass succeeds.
            match self.tick().await {
                Ok(_) => inner.failed_passes.store(0, Ordering::SeqCst),
                Err(e) => {
                    inner.failed_passes.fetch_add(1, Ordering::SeqCst);
                    self.note_failure(format!("a dispatcher pass failed: {e}"));
                }
            }
            inner.passes.fetch_add(1, Ordering::SeqCst);
            let now = inner.clock.now_ms().get();
            let due = store::read(&inner.store, move |r| r.queue().next_due_ms(now))
                .await
                .ok()
                .flatten();
            let mut wait = inner.cfg.sweep_interval;
            if let Some(due) = due {
                let until = u64::try_from(due.saturating_sub(now).max(0)).unwrap_or(u64::MAX);
                wait = wait.min(Duration::from_millis(until));
            }
            let _ = first(inner.wake.notified(), sweep(wait)).await;
        }
    }

    /// Observes operations whose outcome is unknown behind held deliveries, using each
    /// consumer's executor, and records what the target shows (`ObservedComplete`, `Failed`,
    /// `RetryPending` when it shows the effect did not happen). It never re-executes an effect
    /// and never redelivers: an explicit `queue.reconcile` decides that. Returns the number of
    /// operations whose outcome was established.
    pub async fn observe_unknown(&self) -> Result<usize, StorageError> {
        let mut resolved = 0;
        for consumer in &self.inner.consumers {
            let Some(executor) = consumer.effect.clone() else {
                continue;
            };
            let filter = DeliveryFilter {
                consumer: Some(consumer.name.clone()),
                operation_id: None,
            };
            // Bounded pages over every unsettled delivery of the consumer. Observation does not
            // change a delivery's inspection position, so the keyset walk is stable.
            let batch = self.inner.cfg.recovery_batch;
            let mut after: Option<DeliveryKey> = None;
            loop {
                let (f, from) = (filter.clone(), after.clone());
                let (held, next): (Vec<OperationRecord>, Option<DeliveryKey>) =
                    store::read(&self.inner.store, move |r| {
                        let page = r.queue().deliveries(&f, from.as_ref(), batch)?;
                        let mut ops = Vec::new();
                        for e in page.entries {
                            if e.state != QueueState::Reconciling {
                                continue;
                            }
                            if let Some(op) = &e.operation_id
                                && let Some(rec) = r.operations().operation(op)?
                                && rec.state == OperationState::OutcomeUnknown
                            {
                                ops.push(rec);
                            }
                        }
                        Ok((ops, page.next))
                    })
                    .await?;
                for op in held {
                    let observed = executor.observe(op.clone()).await;
                    let established = store::write(&self.inner.store, move |tx| {
                        let Some(current) = tx.operations().operation(&op.id)? else {
                            return Ok(false);
                        };
                        if current.state != OperationState::OutcomeUnknown
                            || current.revision != op.revision
                        {
                            return Ok(false);
                        }
                        let (to, note) = observation(&observed);
                        match to {
                            Some(to) => {
                                tx.operations().transition(
                                    &op.id,
                                    current.revision,
                                    None,
                                    to,
                                    Some(json!({"reconciliation": note})),
                                )?;
                                Ok(true)
                            }
                            None => {
                                if let Some(a) = current.latest_attempt() {
                                    tx.operations().observe(
                                        &op.id,
                                        a.attempt_no,
                                        json!({"reconciliation": note}),
                                    )?;
                                }
                                Ok(false)
                            }
                        }
                    })
                    .await?;
                    if established {
                        resolved += 1;
                    }
                }
                match next {
                    Some(k) => after = Some(k),
                    None => break,
                }
            }
        }
        if resolved > 0 {
            self.notify_status();
        }
        Ok(resolved)
    }

    fn notify_status(&self) {
        self.inner.status.send_modify(|v| *v = v.wrapping_add(1));
    }

    async fn deliver(&self, idx: usize, claimed: QueueEntry) {
        let consumer = self.inner.consumers[idx].clone();
        let outcome = match (
            &consumer.async_handler,
            &consumer.effect,
            &claimed.operation_id,
        ) {
            (Some(handler), _, _) => {
                self.deliver_async(&consumer, handler.clone(), claimed.clone())
                    .await
            }
            (None, Some(executor), Some(_)) => {
                self.deliver_effect(&consumer, executor.clone(), claimed.clone())
                    .await
            }
            _ => self.deliver_local(&consumer, claimed.clone()).await,
        };
        if let Err(cause) = outcome {
            self.after_failure(&consumer, claimed, cause).await;
        }
    }

    /// Database-only delivery: handler and settlement in one transaction.
    async fn deliver_local(
        &self,
        consumer: &Consumer,
        claimed: QueueEntry,
    ) -> Result<Step, StorageError> {
        let handler = consumer.handler.clone();
        let policy = consumer.retry;
        let gate = consumer.eligibility.clone();
        store::write(&self.inner.store, move |tx| {
            let Some(e) = current(tx, &claimed)? else {
                return Ok(Step::Lost);
            };
            if let Some(step) = cancelled(tx, &claimed, &e)? {
                return Ok(step);
            }
            if let Some(step) = root_gate(tx, gate.as_ref(), &e)? {
                return Ok(step);
            }
            apply(tx, &*handler, &e, policy)
        })
        .await
    }

    /// Asynchronous delivery: hand the live claim to the handler, renew its lease and watch for
    /// cancellation while it runs, then settle whatever its commit path did not.
    async fn deliver_async(
        &self,
        consumer: &Consumer,
        handler: Arc<dyn AsyncDeliveryHandler>,
        claimed: QueueEntry,
    ) -> Result<Step, StorageError> {
        let c = claimed.clone();
        let gate = consumer.eligibility.clone();
        let begin = store::write(&self.inner.store, move |tx| {
            let Some(e) = current(tx, &c)? else {
                return Ok(Err(Step::Lost));
            };
            if let Some(step) = cancelled(tx, &c, &e)? {
                return Ok(Err(step));
            }
            if let Some(step) = root_gate(tx, gate.as_ref(), &e)? {
                return Ok(Err(step));
            }
            // An asynchronous handler commits through its own path: it cannot dispatch, record
            // or reconcile an operation, so an operation-bearing delivery cannot be honoured. It
            // fails visibly (its undispatched operation fails before dispatch) instead.
            if let Some(op_id) = &e.operation_id {
                let fence = e
                    .fence()
                    .ok_or_else(|| invalid("claimed entry without fence"))?;
                if let Some(op) = tx.operations().operation(op_id)?
                    && matches!(
                        op.state,
                        OperationState::IntentRecorded | OperationState::RetryPending
                    )
                {
                    tx.operations().transition(
                        &op.id,
                        op.revision,
                        None,
                        OperationState::Failed,
                        None,
                    )?;
                }
                let after = store::fail_terminal(
                    tx,
                    &e,
                    &fence,
                    "an asynchronous consumer cannot carry an operation",
                )?;
                return Ok(Err(Step::Settled(after.state)));
            }
            let event = tx
                .history()
                .event(&e.event_id)?
                .ok_or_else(|| StorageError::NotFound {
                    kind: "event",
                    id: e.event_id.0.as_str().to_owned(),
                })?;
            Ok(Ok(Delivery { entry: e, event }))
        })
        .await?;
        let delivery = match begin {
            Ok(d) => d,
            Err(step) => return Ok(step),
        };
        let fence = delivery.fence();
        let epoch = delivery.entry.cancellation_epoch;
        let (signal, cancelled_rx) = watch::channel(false);
        let mut running = handler.start(AsyncDelivery {
            delivery,
            cancelled: cancelled_rx,
        });
        let lease = self.inner.cfg.lease_ms;
        let every = Duration::from_millis(u64::try_from((lease / 3).max(1)).unwrap_or(1));
        let grace = consumer.stop_grace_ms;
        let grace_wait = Duration::from_millis(u64::try_from(grace.max(1)).unwrap_or(1));
        // Store-clock time at which the handler was asked to stop.
        let mut asked_at: Option<i64> = None;
        let outcome = loop {
            // Polled in order: a wakeup (or stop), the renewal/grace timer, then the handler.
            let wait = if asked_at.is_some() {
                every.min(grace_wait)
            } else {
                every
            };
            let check = first(self.inner.changed.notified(), tokio::time::sleep(wait));
            if let Either::Right(outcome) = first(check, &mut running).await {
                break Some(outcome);
            }
            let now = self.inner.clock.now_ms().get();
            if asked_at.is_some_and(|at| now.saturating_sub(at) >= grace) {
                // The handler ignored the request past its grace: stop supervising and renewing
                // it. Its claim's lease expires into normal recovery, which honours a requested
                // cancellation; a late commit is refused as stale.
                break None;
            }
            let (id, f) = (claimed.id.clone(), fence.clone());
            let cancel_requested = store::write(&self.inner.store, move |tx| {
                let e = tx.queue().renew(&id, &f, lease)?;
                Ok(e.cancellation_epoch != epoch || e.cancel_requested)
            })
            .await;
            // A lost lease needs no signal: the handler's commit is refused as stale.
            let stopping = self.inner.stopping.load(Ordering::SeqCst);
            if asked_at.is_none() && (stopping || matches!(cancel_requested, Ok(true))) {
                let _ = signal.send(true);
                asked_at = Some(now);
            }
            self.notify_status();
        };
        let Some(outcome) = outcome else {
            drop(running);
            return Ok(Step::Lost);
        };
        let policy = consumer.retry;
        store::write(&self.inner.store, move |tx| {
            let Some(e) = current(tx, &claimed)? else {
                // Settled by the handler's commit path (or recovered).
                return Ok(Step::Lost);
            };
            if let Some(step) = cancelled(tx, &claimed, &e)? {
                return Ok(step);
            }
            let fence = e
                .fence()
                .ok_or_else(|| invalid("claimed entry without fence"))?;
            let after = match outcome {
                AsyncOutcome::Committed => {
                    tx.queue().settle(&e.id, &fence, Settlement::Completed)?
                }
                AsyncOutcome::Retry(cause) => store::fail_retry(
                    tx,
                    &e,
                    &fence,
                    &cause,
                    policy.delay_for(e.attempt_count),
                    policy.max_attempts,
                )?,
                AsyncOutcome::Poison(cause) => store::fail_terminal(tx, &e, &fence, &cause)?,
                AsyncOutcome::Hold(cause) => store::hold(tx, &e, &fence, &cause)?,
            };
            Ok(Step::Settled(after.state))
        })
        .await
    }

    /// External-effect delivery: commit `Dispatching`, run the effect, record its outcome.
    async fn deliver_effect(
        &self,
        consumer: &Consumer,
        executor: Arc<dyn EffectExecutor>,
        claimed: QueueEntry,
    ) -> Result<Step, StorageError> {
        enum Begin {
            Dispatch(Box<OperationRecord>),
            Done(Step),
        }
        let handler = consumer.handler.clone();
        let policy = consumer.retry;
        let owner = self.inner.cfg.owner.clone();
        let c = claimed.clone();
        let gate = consumer.eligibility.clone();
        let begin = store::write(&self.inner.store, move |tx| {
            let Some(e) = current(tx, &c)? else {
                return Ok(Begin::Done(Step::Lost));
            };
            if let Some(step) = cancelled(tx, &c, &e)? {
                return Ok(Begin::Done(step));
            }
            if let Some(step) = root_gate(tx, gate.as_ref(), &e)? {
                return Ok(Begin::Done(step));
            }
            let fence = e.fence().ok_or_else(|| invalid("claimed entry without fence"))?;
            let op_id = e.operation_id.clone().ok_or_else(|| invalid("effect without operation"))?;
            let op = tx.operations().operation(&op_id)?.ok_or(StorageError::NotFound {
                kind: "operation",
                id: op_id.0.as_str().to_owned(),
            })?;
            use OperationState::*;
            match op.state {
                IntentRecorded | RetryPending => {
                    let attempt = tx.operations().begin_attempt(
                        &op.id,
                        op.revision,
                        AttemptBinding {
                            owner_generation: Some(e.generation),
                            ..AttemptBinding::default()
                        },
                    )?;
                    let rec = tx.operations().transition(
                        &op.id,
                        op.revision + 1,
                        Some(&attempt.fence()),
                        Dispatching,
                        Some(json!({"dispatcher": owner.as_str(), "deliveryGeneration": e.generation})),
                    )?;
                    Ok(Begin::Dispatch(Box::new(rec)))
                }
                // Already established (for example by reconciliation): never dispatch again.
                ObservedComplete => Ok(Begin::Done(apply(tx, &*handler, &e, policy)?)),
                Failed | CancelledBeforeDispatch => {
                    let after = store::fail_terminal(
                        tx,
                        &e,
                        &fence,
                        &format!("operation is {:?}", op.state),
                    )?;
                    Ok(Begin::Done(Step::Settled(after.state)))
                }
                Preflighted | Dispatching | OutcomeUnknown | NeedsInput => {
                    let after = store::hold(
                        tx,
                        &e,
                        &fence,
                        &format!("operation is {:?}; outcome not established", op.state),
                    )?;
                    Ok(Begin::Done(Step::Settled(after.state)))
                }
            }
        })
        .await?;
        let operation = match begin {
            Begin::Done(step) => return Ok(step),
            Begin::Dispatch(op) => *op,
        };

        // The effect runs outside any transaction; the lease is renewed while it runs.
        let mut effect = executor.execute(operation.clone());
        let lease = self.inner.cfg.lease_ms;
        let every = Duration::from_millis(u64::try_from((lease / 3).max(1)).unwrap_or(1));
        let fence = claimed
            .fence()
            .ok_or_else(|| invalid("claimed entry without fence"))?;
        let result = loop {
            match tokio::time::timeout(every, &mut effect).await {
                Ok(r) => break r,
                Err(_) => {
                    let (id, f) = (claimed.id.clone(), fence.clone());
                    // A lost lease does not stop recording the outcome: the attempt fence still
                    // binds the operation, and the delivery is then reconciled, not replayed.
                    let _ = store::write(&self.inner.store, move |tx| {
                        tx.queue().renew(&id, &f, lease)
                    })
                    .await;
                }
            }
        };

        let handler = consumer.handler.clone();
        store::write(&self.inner.store, move |tx| {
            let op = tx
                .operations()
                .operation(&operation.id)?
                .ok_or_else(|| invalid("operation vanished"))?;
            let (to, note) = observation(&result);
            let to = to.unwrap_or(OperationState::OutcomeUnknown);
            if op.state == OperationState::Dispatching {
                let fence = op.latest_attempt().map(|a| a.fence());
                tx.operations().transition(
                    &op.id,
                    op.revision,
                    fence.as_ref(),
                    to,
                    Some(json!({"effect": note})),
                )?;
            } else if let Some(a) = op.latest_attempt() {
                // Recovery already reclassified it: keep the late result as evidence only.
                tx.operations()
                    .observe(&op.id, a.attempt_no, json!({"lateEffect": note}))?;
            }
            let Some(e) = current(tx, &claimed)? else {
                return Ok(Step::Lost);
            };
            let fence = e
                .fence()
                .ok_or_else(|| invalid("claimed entry without fence"))?;
            if op.state != OperationState::Dispatching {
                let after = store::hold(tx, &e, &fence, "late effect result after recovery")?;
                return Ok(Step::Settled(after.state));
            }
            // The effect did not happen and a cancellation arrived meanwhile: the operation
            // (now `RetryPending`) is cancelled with the delivery instead of being retried.
            if matches!(result, EffectResult::NotApplied(_))
                && let Some(step) = cancelled(tx, &claimed, &e)?
            {
                return Ok(step);
            }
            let after = match &result {
                EffectResult::Completed(_) => return apply(tx, &*handler, &e, policy),
                EffectResult::Failed(cause) => store::fail_terminal(tx, &e, &fence, cause)?,
                EffectResult::NotApplied(cause) => store::fail_retry(
                    tx,
                    &e,
                    &fence,
                    cause,
                    policy.delay_for(e.attempt_count),
                    policy.max_attempts,
                )?,
                EffectResult::Unknown(cause) => store::hold(tx, &e, &fence, cause)?,
            };
            Ok(Step::Settled(after.state))
        })
        .await
    }

    /// A delivery transaction failed (handler error or panic, storage failure): its writes were
    /// rolled back. Record the cause and retry with backoff, unless the outcome is unknown.
    async fn after_failure(&self, consumer: &Consumer, claimed: QueueEntry, cause: StorageError) {
        if matches!(cause, StorageError::CommitUnknown) {
            // The commit may have happened; the durable record decides. If it did not, lease
            // expiry recovers the claim (at least once).
            return;
        }
        let policy = consumer.retry;
        let cause = format!("delivery failed: {cause}");
        // Unrecorded, the claim stays until its lease expires and recovery reclaims it; the
        // health check shows why.
        let recorded = store::write(&self.inner.store, move |tx| {
            let Some(e) = current(tx, &claimed)? else {
                return Ok(());
            };
            let fence = e
                .fence()
                .ok_or_else(|| invalid("claimed entry without fence"))?;
            // This claim dispatched an effect whose outcome could not be recorded: the outcome
            // is unknown, so it is observed and reconciled (the store then holds the delivery).
            if let Some(op_id) = &e.operation_id
                && let Some(op) = tx.operations().operation(op_id)?
                && op.state == OperationState::Dispatching
                && let Some(attempt) = op.latest_attempt()
                && attempt.binding.owner_generation == Some(e.generation)
            {
                let attempt_fence = attempt.fence();
                tx.operations().transition(
                    &op.id,
                    op.revision,
                    Some(&attempt_fence),
                    OperationState::OutcomeUnknown,
                    Some(json!({"unrecorded": store::bounded(&cause)})),
                )?;
            }
            store::fail_retry(
                tx,
                &e,
                &fence,
                &cause,
                policy.delay_for(e.attempt_count),
                policy.max_attempts,
            )?;
            Ok(())
        })
        .await;
        if let Err(e) = recorded {
            self.note_failure(format!("a delivery failure could not be recorded: {e}"));
        }
    }
}

pub(crate) enum Either<A, B> {
    Left(A),
    Right(B),
}

/// Resolves with whichever future is ready first, polling `a` before `b` each time.
pub(crate) async fn first<A: Future, B: Future>(a: A, b: B) -> Either<A::Output, B::Output> {
    let mut a = std::pin::pin!(a);
    let mut b = std::pin::pin!(b);
    std::future::poll_fn(|cx| {
        if let std::task::Poll::Ready(v) = a.as_mut().poll(cx) {
            return std::task::Poll::Ready(Either::Left(v));
        }
        if let std::task::Poll::Ready(v) = b.as_mut().poll(cx) {
            return std::task::Poll::Ready(Either::Right(v));
        }
        std::task::Poll::Pending
    })
    .await
}

/// Maps an observed effect result to the operation state it establishes (`None`: unknown).
fn observation(r: &EffectResult) -> (Option<OperationState>, Value) {
    match r {
        EffectResult::Completed(v) => (
            Some(OperationState::ObservedComplete),
            json!({"outcome": "completed", "evidence": v}),
        ),
        EffectResult::Failed(c) => (
            Some(OperationState::Failed),
            json!({"outcome": "failed", "cause": c}),
        ),
        EffectResult::NotApplied(c) => (
            Some(OperationState::RetryPending),
            json!({"outcome": "notApplied", "cause": c}),
        ),
        EffectResult::Unknown(c) => (None, json!({"outcome": "unknown", "cause": c})),
    }
}

/// The entry, if it is still claimed under the fence this dispatcher holds.
fn current(
    tx: &mut dyn WriteTransaction,
    claimed: &QueueEntry,
) -> Result<Option<QueueEntry>, StorageError> {
    let Some(e) = tx.queue().entry(&claimed.id)? else {
        return Ok(None);
    };
    let ours = e.state == QueueState::Claimed
        && e.owner == claimed.owner
        && e.generation == claimed.generation;
    Ok(ours.then_some(e))
}

/// Cooperative cancellation: a request arrived after the claim (the epoch rose) or survived a
/// hold or requeue (the durable marker). It applies while the operation, if any, has certainly
/// not dispatched (`IntentRecorded`, `RetryPending`, `Preflighted`): the delivery settles
/// `Cancelled` and the store cancels the operation with it. A possibly dispatched or already
/// established operation is reconciled instead, so this returns `None`.
fn cancelled(
    tx: &mut dyn WriteTransaction,
    claimed: &QueueEntry,
    e: &QueueEntry,
) -> Result<Option<Step>, StorageError> {
    if e.cancellation_epoch == claimed.cancellation_epoch && !e.cancel_requested {
        return Ok(None);
    }
    if !cancellation_applies(&*tx.operations(), e)? {
        return Ok(None);
    }
    let fence = e
        .fence()
        .ok_or_else(|| invalid("claimed entry without fence"))?;
    let after = tx.queue().settle(&e.id, &fence, Settlement::Cancelled)?;
    Ok(Some(Step::Settled(after.state)))
}

/// The root gate of a claimed delivery (see [`RootEligibility::may_dispatch`]). Work whose
/// operation may already have dispatched is left to reconciliation, never re-gated.
fn root_gate(
    tx: &mut dyn WriteTransaction,
    port: Option<&Arc<dyn RootEligibility>>,
    e: &QueueEntry,
) -> Result<Option<Step>, StorageError> {
    let (Some(port), Some(_)) = (port, &e.root_id) else {
        return Ok(None);
    };
    if let Some(op) = &e.operation_id
        && tx
            .operations()
            .operation(op)?
            .is_some_and(|o| possibly_dispatched(o.state))
    {
        return Ok(None);
    }
    if port.may_dispatch(tx, e)? {
        return Ok(None);
    }
    let fence = e
        .fence()
        .ok_or_else(|| invalid("claimed entry without fence"))?;
    let after = tx.queue().hold_for_eligibility(
        &e.id,
        &fence,
        "the root does not permit dispatch (paused, held or stopped)",
    )?;
    Ok(Some(Step::Settled(after.state)))
}

/// Runs the handler in a savepoint and records its outcome in the same transaction.
fn apply(
    tx: &mut dyn WriteTransaction,
    handler: &dyn DeliveryHandler,
    e: &QueueEntry,
    policy: RetryPolicy,
) -> Result<Step, StorageError> {
    let fence = e
        .fence()
        .ok_or_else(|| invalid("claimed entry without fence"))?;
    let event = tx
        .history()
        .event(&e.event_id)?
        .ok_or_else(|| StorageError::NotFound {
            kind: "event",
            id: e.event_id.0.as_str().to_owned(),
        })?;
    let delivery = Delivery {
        entry: e.clone(),
        event,
    };
    let sp = tx.savepoint()?;
    let outcome = match handler.apply(tx, &delivery) {
        Ok(o) => o,
        Err(err) => {
            tx.rollback_to(sp)?;
            let after = store::fail_retry(
                tx,
                e,
                &fence,
                &format!("handler error: {err}"),
                policy.delay_for(e.attempt_count),
                policy.max_attempts,
            )?;
            return Ok(Step::Settled(after.state));
        }
    };
    let after = match outcome {
        Outcome::Completed => {
            tx.release(sp)?;
            tx.queue().settle(&e.id, &fence, Settlement::Completed)?
        }
        Outcome::Committed => {
            tx.release(sp)?;
            match current(tx, e)? {
                // The handler's commit path replayed without settling: settle it now.
                Some(still) => tx
                    .queue()
                    .settle(&still.id, &fence, Settlement::Completed)?,
                None => return Ok(Step::Settled(QueueState::Completed)),
            }
        }
        Outcome::Retry(cause) => {
            tx.rollback_to(sp)?;
            store::fail_retry(
                tx,
                e,
                &fence,
                &cause,
                policy.delay_for(e.attempt_count),
                policy.max_attempts,
            )?
        }
        Outcome::Poison(cause) => {
            tx.rollback_to(sp)?;
            store::fail_terminal(tx, e, &fence, &cause)?
        }
        Outcome::Hold(cause) => {
            tx.rollback_to(sp)?;
            store::hold(tx, e, &fence, &cause)?
        }
    };
    Ok(Step::Settled(after.state))
}
