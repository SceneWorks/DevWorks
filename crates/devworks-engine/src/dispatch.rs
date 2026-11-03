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
//!   completed (reconciled), the delivery only runs its handler; it never dispatches again.
use crate::store;
use devworks_core::ports::Clock;
use devworks_core::ports::storage::{
    ApplicationStore, AttemptBinding, DeliveryFilter, EventRecord, OperationRecord, QueueEntry,
    QueueFence, QueueState, RecoveryOutcome, Settlement, StorageError, WriteTransaction,
};
use devworks_core::protocol::{Id, OperationId, OperationState};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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

/// External effect execution and observation for one consumer's operations.
pub trait EffectExecutor: Send + Sync {
    /// Performs the effect of a `Dispatching` operation once.
    fn execute(&self, operation: OperationRecord) -> EffectFuture;
    /// Observes the target for an operation whose outcome is unknown using its saved identity;
    /// it must not perform the effect.
    fn observe(&self, operation: OperationRecord) -> EffectFuture;
}

/// A registered consumer. `name` is an opaque identifier (it is the wire `consumerId`).
#[derive(Clone)]
pub struct Consumer {
    pub name: String,
    pub handler: Arc<dyn DeliveryHandler>,
    pub effect: Option<Arc<dyn EffectExecutor>>,
    pub retry: RetryPolicy,
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
    /// Rows per recovery transaction (1..=500).
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
}

/// What one pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    /// A sleep/wake (wall-clock jump) was detected and repaired.
    pub woke: bool,
    pub reasserted: usize,
    pub recovered: RecoveryOutcome,
    pub claimed: usize,
}

/// Best-effort, coalescing post-commit wakeup.
#[derive(Clone)]
pub struct Wakeup(Arc<Notify>);
impl Wakeup {
    pub fn wake(&self) {
        self.0.notify_one();
    }
}

struct Inner {
    store: Arc<dyn ApplicationStore>,
    clock: Arc<dyn Clock>,
    consumers: Vec<Consumer>,
    cfg: DispatcherConfig,
    permits: Arc<Semaphore>,
    wake: Arc<Notify>,
    status: watch::Sender<u64>,
    in_flight: Mutex<HashMap<Id, QueueFence>>,
    last_tick: Mutex<Option<(Instant, i64)>>,
    recovered: AtomicBool,
    stopping: AtomicBool,
    rotation: AtomicUsize,
    passes: AtomicU64,
}

/// The dispatcher. Cheap to clone; clones share state.
#[derive(Clone)]
pub struct Dispatcher {
    inner: Arc<Inner>,
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
        }
        let (status, _) = watch::channel(0);
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                clock,
                permits: Arc::new(Semaphore::new(cfg.max_in_flight)),
                consumers,
                cfg,
                wake: Arc::new(Notify::new()),
                status,
                in_flight: Mutex::new(HashMap::new()),
                last_tick: Mutex::new(None),
                recovered: AtomicBool::new(false),
                stopping: AtomicBool::new(false),
                rotation: AtomicUsize::new(0),
                passes: AtomicU64::new(0),
            }),
        })
    }

    /// Handle producers call after committing new deliveries (best effort).
    pub fn wakeup(&self) -> Wakeup {
        Wakeup(self.inner.wake.clone())
    }

    /// "Queue state may have changed" hints for UI; repair a missed hint with a durable query.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.inner.status.subscribe()
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

    /// Stops admission; in-flight deliveries finish (see [`Dispatcher::idle`]).
    pub fn stop(&self) {
        self.inner.stopping.store(true, Ordering::SeqCst);
        self.inner.wake.notify_one();
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
            let n = out.requeued.len() + out.held.len();
            report.requeued.extend(out.requeued);
            report.held.extend(out.held);
            if n < usize::from(limit) {
                break;
            }
        }
        self.inner.recovered.store(true, Ordering::SeqCst);
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
        let order: Vec<(usize, String)> = (0..inner.consumers.len())
            .map(|i| (start + i) % inner.consumers.len())
            .map(|i| (i, inner.consumers[i].name.clone()))
            .collect();
        let (owner, lease) = (inner.cfg.owner.clone(), inner.cfg.lease_ms);
        let claimed = store::write(&inner.store, move |tx| {
            let mut out = Vec::new();
            for (idx, name) in &order {
                let left = budget - out.len();
                if left == 0 {
                    break;
                }
                let n = u16::try_from(left).unwrap_or(500).min(500);
                for e in store::claim(tx, name, &owner, lease, n)? {
                    out.push((*idx, e));
                }
            }
            Ok(out)
        })
        .await?;
        report.claimed = claimed.len();
        for ((idx, entry), permit) in claimed.into_iter().zip(permits) {
            if let Some(f) = entry.fence() {
                inner
                    .in_flight
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(entry.id.clone(), f);
            }
            let me = self.clone();
            tokio::spawn(async move {
                let id = entry.id.clone();
                me.deliver(idx, entry).await;
                me.inner
                    .in_flight
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                drop(permit);
                me.notify_status();
                // Capacity freed: more ready work may be claimable now.
                me.inner.wake.notify_one();
            });
        }
        if report.claimed > 0 {
            self.notify_status();
        }
        Ok(report)
    }

    /// Waits until no delivery is in flight.
    pub async fn idle(&self) {
        let n = u32::try_from(self.inner.cfg.max_in_flight).unwrap_or(u32::MAX);
        if let Ok(all) = self.inner.permits.acquire_many(n).await {
            drop(all);
        }
    }

    /// Delivers everything currently eligible: passes until one claims nothing and no delivery
    /// is in flight. Returns the number of claims made.
    pub async fn run_until_idle(&self) -> Result<usize, StorageError> {
        let mut total = 0;
        loop {
            let r = self.tick().await?;
            total += r.claimed;
            self.idle().await;
            if r.claimed == 0 {
                return Ok(total);
            }
        }
    }

    /// The supervised loop: passes until [`Dispatcher::stop`], each followed by a bounded wait
    /// for a wakeup, the next saved ready/lease time, or the sweep interval.
    pub async fn run(&self) {
        let inner = &self.inner;
        while !inner.stopping.load(Ordering::SeqCst) {
            // A failed pass (store busy or unavailable) is retried after the sweep interval.
            let _ = self.tick().await;
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
            let _ = tokio::time::timeout(wait, inner.wake.notified()).await;
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
            let held: Vec<OperationRecord> = store::read(&self.inner.store, move |r| {
                let page = r.queue().deliveries(&filter, None, 500)?;
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
                Ok(ops)
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
        let outcome = match (&consumer.effect, &claimed.operation_id) {
            (Some(executor), Some(_)) => {
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
        store::write(&self.inner.store, move |tx| {
            let Some(e) = current(tx, &claimed)? else {
                return Ok(Step::Lost);
            };
            if let Some(step) = cancelled(tx, &claimed, &e)? {
                return Ok(step);
            }
            apply(tx, &*handler, &e, policy)
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
        let begin = store::write(&self.inner.store, move |tx| {
            let Some(e) = current(tx, &c)? else {
                return Ok(Begin::Done(Step::Lost));
            };
            if let Some(step) = cancelled(tx, &c, &e)? {
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
        let _ = store::write(&self.inner.store, move |tx| {
            let Some(e) = current(tx, &claimed)? else {
                return Ok(());
            };
            let fence = e
                .fence()
                .ok_or_else(|| invalid("claimed entry without fence"))?;
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
    }
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

/// Cooperative cancellation: the epoch rose after the claim, so stop before applying. An
/// undispatched operation is cancelled with it.
fn cancelled(
    tx: &mut dyn WriteTransaction,
    claimed: &QueueEntry,
    e: &QueueEntry,
) -> Result<Option<Step>, StorageError> {
    if e.cancellation_epoch == claimed.cancellation_epoch {
        return Ok(None);
    }
    let fence = e
        .fence()
        .ok_or_else(|| invalid("claimed entry without fence"))?;
    if let Some(op_id) = &e.operation_id
        && let Some(op) = tx.operations().operation(op_id)?
        && matches!(
            op.state,
            OperationState::IntentRecorded | OperationState::RetryPending
        )
    {
        tx.operations().transition(
            &op.id,
            op.revision,
            None,
            OperationState::CancelledBeforeDispatch,
            None,
        )?;
    }
    let after = tx.queue().settle(&e.id, &fence, Settlement::Cancelled)?;
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
