//! Run root supervision for the application lifecycle (CONTROL-03/05/06/08..11): what the
//! composition root runs at boot, while open and at shutdown on top of core's
//! [`ControlService`].
//!
//! * [`RunSupervisor::boot`] runs run-control boot recovery after owned-process and lease
//!   recovery and before delivery: activations an earlier boot left live (including accepted but
//!   never launched ones) are recorded `Interrupted` and their roots held for an explicit,
//!   selective Continue; journaled stops are settled from verified process state.
//! * [`RunSupervisor::tick`] reasserts its activation leases after a detected sleep/wake (the
//!   same wall-versus-monotonic test as the dispatcher and housekeeper; no manual hold is added),
//!   recovers expired leases of this boot with bounded backoff, executes journaled stops (owned
//!   trees of the root are stopped through the process owner, then the stop is observed), and —
//!   when an [`ActivationLauncher`] is installed — claims ready activations, marks them running
//!   and launches them, renewing each lease and signalling a requested stop.
//! * [`RunSupervisor::quiesce`] and [`RunSupervisor::finish`] are the run-control steps of the
//!   bounded shutdown ([`crate::runtime::Runtime::shutdown`]).
//! * [`RunGate`] is the root-control [`RootEligibility`] every consumer gets at boot.
use crate::dispatch::{Either, RootEligibility, Wakeup, first};
use crate::store;
use devworks_core::ports::Clock;
use devworks_core::ports::process::NativeIdentity;
use devworks_core::ports::storage::{
    ActivationOutcome, ActivationPhase, ApplicationStore, QueueEntry, StorageError,
    StoredActivation, WriteTransaction, is_terminal,
};
use devworks_core::protocol::{Id, OperationId, OperationState, ProjectId, RootId};
use devworks_core::services::process::{OwnedProcess, ProcessSupervisor, StopOutcome, StopScope};
use devworks_core::services::runs::ControlService;
use devworks_core::services::runs::recovery::{
    BootRecovery, LifecycleHooks, dispatch_stop_operation, stop_reconciliation_targets,
    stop_superseded,
};
use devworks_core::services::runs::substrate::ActivationFailure;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// The root-control gate: root-scoped deliveries run, and held ones resume, only while their
/// root is enabled, not archived and free of root-level holds.
pub struct RunGate {
    service: Arc<ControlService>,
    /// A consumer's own port, which must also allow.
    inner: Option<Arc<dyn RootEligibility>>,
}
impl RunGate {
    pub fn new(service: Arc<ControlService>, inner: Option<Arc<dyn RootEligibility>>) -> Self {
        Self { service, inner }
    }
    fn permits(
        &self,
        tx: &mut dyn WriteTransaction,
        entry: &QueueEntry,
    ) -> Result<bool, StorageError> {
        match &entry.root_id {
            Some(root) => self.service.root_permits_dispatch(tx, root),
            None => Ok(true),
        }
    }
}
impl RootEligibility for RunGate {
    fn may_resume(
        &self,
        tx: &mut dyn WriteTransaction,
        entry: &QueueEntry,
    ) -> Result<bool, StorageError> {
        if !self.permits(tx, entry)? {
            return Ok(false);
        }
        match &self.inner {
            Some(p) => p.may_resume(tx, entry),
            None => Ok(true),
        }
    }
    fn may_dispatch(
        &self,
        tx: &mut dyn WriteTransaction,
        entry: &QueueEntry,
    ) -> Result<bool, StorageError> {
        if !self.permits(tx, entry)? {
            return Ok(false);
        }
        match &self.inner {
            Some(p) => p.may_dispatch(tx, entry),
            None => Ok(true),
        }
    }
}

/// How a launched activation ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchEnd {
    /// It ended (completed its work, honoured a stop at a safe boundary, ...).
    Finished {
        outcome: ActivationOutcome,
        detail: Option<String>,
    },
    /// Its work failed; the category decides retry, wait or escalation.
    Failed(ActivationFailure),
}
pub type LaunchFuture = Pin<Box<dyn Future<Output = LaunchEnd> + Send>>;

/// One running activation handed to the launcher. `stop` turns `true` when a stop was
/// requested (pause, stop, archival, shutdown); the launcher admits nothing more and returns at
/// its next safe boundary. Every dispatch it makes goes through
/// [`ControlService::begin_dispatch`] with this activation's ID and claim epoch.
pub struct Launch {
    pub activation: StoredActivation,
    pub stop: watch::Receiver<bool>,
}

/// Launches a claimed activation's coordinator (the agent supervisor, a later item). The
/// supervisor owns claim, lease renewal, stop signalling and the recorded outcome.
pub trait ActivationLauncher: Send + Sync {
    fn launch(&self, launch: Launch) -> LaunchFuture;
}

/// What one tick did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunTick {
    pub woke: bool,
    pub reasserted: usize,
    pub expired: usize,
    pub stops: Vec<(RootId, OperationState)>,
    /// Roots whose pending stop could not be executed this pass (each in its own unit of
    /// work: one root's failure never blocks another's).
    pub failed: Vec<(RootId, String)>,
    /// Roots whose capacity or rate-limit wait expired and was cleared.
    pub swept: Vec<RootId>,
    pub launched: Vec<Id>,
}

/// One launch, keyed by its activation and claim epoch: a relaunch of the same activation after
/// its lease expired is a different entry, so the old launch's cleanup never removes it.
struct Running {
    activation: Id,
    /// The activation's generation and the claim epoch this launch runs under.
    generation: i64,
    epoch: i64,
    root: RootId,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}
fn launch_key(activation: &Id, epoch: i64) -> String {
    format!("{}#{epoch}", activation.as_str())
}

pub struct RunSupervisor {
    store: Arc<dyn ApplicationStore>,
    clock: Arc<dyn Clock>,
    service: Arc<ControlService>,
    processes: Arc<ProcessSupervisor>,
    launcher: Option<Arc<dyn ActivationLauncher>>,
    hooks: Arc<dyn LifecycleHooks>,
    owner: Id,
    /// A wall-clock advance exceeding the monotonic advance by more than this is a sleep/wake.
    pub wake_jump_ms: i64,
    /// Cooperative stop period of a root's owned trees when it is stopped.
    pub stop_grace: Duration,
    /// The bound of one root's owned-tree stop.
    pub stop_bound: Duration,
    /// Most pending stops one pass executes (1..=500). Passes rotate through every pending
    /// stop from a cursor, so no root's stop waits behind others that stay pending.
    pub stop_batch: u16,
    stop_cursor: Mutex<Option<RootId>>,
    /// The dispatcher's wakeup: a supervision commit that can make a root eligible again
    /// (a cleared hold, an expired wait, a settled stop, a recovered lease) restarts its scan
    /// of root work held awaiting eligibility.
    dispatcher: Mutex<Option<Wakeup>>,
    last: Mutex<Option<(Instant, i64)>>,
    stopping: AtomicBool,
    wake: Notify,
    running: Mutex<HashMap<String, Running>>,
}

impl RunSupervisor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        clock: Arc<dyn Clock>,
        service: Arc<ControlService>,
        processes: Arc<ProcessSupervisor>,
        launcher: Option<Arc<dyn ActivationLauncher>>,
        hooks: Arc<dyn LifecycleHooks>,
        owner: Id,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            clock,
            service,
            processes,
            launcher,
            hooks,
            owner,
            wake_jump_ms: 10_000,
            stop_grace: Duration::from_secs(5),
            stop_bound: Duration::from_secs(30),
            stop_batch: 500,
            stop_cursor: Mutex::new(None),
            dispatcher: Mutex::new(None),
            last: Mutex::new(None),
            stopping: AtomicBool::new(false),
            wake: Notify::new(),
            running: Mutex::new(HashMap::new()),
        })
    }

    pub fn service(&self) -> &Arc<ControlService> {
        &self.service
    }
    /// The owner identity recorded on this supervisor's activation claims.
    pub fn owner(&self) -> &Id {
        &self.owner
    }
    /// IDs of the activations this supervisor has launched and not yet seen end.
    pub fn launched(&self) -> Vec<Id> {
        let map = self.running.lock().unwrap_or_else(|p| p.into_inner());
        let mut ids: Vec<Id> = map.values().map(|r| r.activation.clone()).collect();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        ids.dedup();
        ids
    }

    /// How many launch tasks this supervisor still tracks (one per launched claim).
    pub fn launches(&self) -> usize {
        self.running.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// Signals every launch to stop and aborts its task at once, without recording anything
    /// (the drop path: the next boot interrupts and holds whatever was live). Nothing renews a
    /// lease or writes after this.
    pub fn abort_launches(&self) {
        self.stop();
        let tasks: Vec<Running> = {
            let mut map = self.running.lock().unwrap_or_else(|p| p.into_inner());
            map.drain().map(|(_, r)| r).collect()
        };
        for r in tasks {
            let _ = r.stop.send(true);
            r.task.abort();
        }
    }

    /// Boot recovery of run roots (before any delivery, claim or command).
    pub async fn boot(&self) -> Result<BootRecovery, StorageError> {
        let (service, hooks) = (self.service.clone(), self.hooks.clone());
        store::write(&self.store, move |tx| service.recover_boot(tx, &*hooks)).await
    }

    /// Connects the dispatcher whose held root work supervision commits may make eligible.
    pub fn set_dispatcher_wakeup(&self, wakeup: Wakeup) {
        *self.dispatcher.lock().unwrap_or_else(|p| p.into_inner()) = Some(wakeup);
    }
    fn wake_dispatcher(&self) {
        if let Some(w) = self
            .dispatcher
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            w.wake();
        }
    }

    /// Requests a tick now (a committed stop, pause or continue).
    pub fn notify(&self) {
        self.wake.notify_one();
    }

    /// One supervision pass (see the module documentation).
    pub async fn tick(self: &Arc<Self>) -> Result<RunTick, StorageError> {
        let mut report = RunTick::default();
        let wall = self.clock.now_ms().get();
        let mono = Instant::now();
        report.woke = {
            let mut last = self.last.lock().unwrap_or_else(|p| p.into_inner());
            let woke = last.is_some_and(|(m0, w0)| {
                let mono_ms =
                    i64::try_from(mono.duration_since(m0).as_millis()).unwrap_or(i64::MAX);
                (wall - w0).saturating_sub(mono_ms) > self.wake_jump_ms
            });
            *last = Some((mono, wall));
            woke
        };
        if report.woke {
            let (service, owner) = (self.service.clone(), self.owner.clone());
            report.reasserted =
                store::write(&self.store, move |tx| service.reassert_leases(tx, &owner)).await?;
        }
        let service = self.service.clone();
        report.expired = store::write(&self.store, move |tx| service.recover_expired(tx)).await?;
        let service = self.service.clone();
        report.swept = store::write(&self.store, move |tx| service.sweep_expired_waits(tx)).await?;
        self.signal_stopping().await?;
        (report.stops, report.failed) = self.execute_stops().await?;
        // Anything here may have made a root eligible again: restart the dispatcher's scan of
        // root work held awaiting eligibility (it rechecks each root; a needless wake is cheap).
        if report.reasserted > 0
            || report.expired > 0
            || !report.swept.is_empty()
            || !report.stops.is_empty()
        {
            self.wake_dispatcher();
        }
        if self.launcher.is_some() && !self.stopping.load(Ordering::SeqCst) {
            loop {
                // Shutdown (or drop) may begin while this tick runs: no further claim, and a
                // claim committed meanwhile is not launched (shutdown records it interrupted).
                if self.stopping.load(Ordering::SeqCst) {
                    break;
                }
                let (service, owner) = (self.service.clone(), self.owner.clone());
                let claimed = store::write(&self.store, move |tx| {
                    let Some(a) = service.claim_activation(tx, &owner)? else {
                        return Ok(None);
                    };
                    service
                        .start_activation(tx, &a.id, a.owner_epoch)
                        .map(|r| r.ok())
                })
                .await?;
                let Some(running) = claimed else {
                    break;
                };
                if self.stopping.load(Ordering::SeqCst) {
                    break;
                }
                report.launched.push(running.id.clone());
                self.spawn_launch(running);
            }
        }
        Ok(report)
    }

    /// Executes every journaled stop still to be observed: the stop is marked dispatching, the
    /// launches and owned trees it covers are stopped (bounded), and the outcome is recorded.
    #[allow(clippy::type_complexity)]
    async fn execute_stops(
        &self,
    ) -> Result<(Vec<(RootId, OperationState)>, Vec<(RootId, String)>), StorageError> {
        let batch = self.stop_batch.clamp(1, 500);
        let cursor = self
            .stop_cursor
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let from = cursor.clone();
        let mut pending = store::read(&self.store, move |s| {
            s.runs().pending_stops(from.as_ref(), batch)
        })
        .await?;
        if pending.is_empty() && cursor.is_some() {
            // Past the last pending stop: wrap around to the first.
            pending =
                store::read(&self.store, move |s| s.runs().pending_stops(None, batch)).await?;
        }
        // A full page continues after its last root next pass; a short one starts over.
        *self.stop_cursor.lock().unwrap_or_else(|p| p.into_inner()) =
            if pending.len() == usize::from(batch) {
                pending.last().map(|c| c.root_id.clone())
            } else {
                None
            };
        let mut out = Vec::new();
        let mut failed = Vec::new();
        for c in pending {
            // Shutdown stops admitting root stops: the remaining trees are stopped by the shared
            // bounded shutdown of owned trees, and the stops settle at the next boot.
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
            let Some(op) = c.stop_operation_id.clone() else {
                continue;
            };
            match self
                .stop_root_now(
                    &c.root_id,
                    &op,
                    Some(StopScope::Root(c.root_id.clone())),
                    self.stop_grace,
                )
                .await
            {
                Ok(state) => out.push((c.root_id, state)),
                // Each root's pass is its own unit of work: a failure is retried next pass and
                // never blocks the other roots' stops.
                Err(e) => failed.push((c.root_id, e.to_string())),
            }
        }
        Ok((out, failed))
    }

    /// One execution pass of a root's stop, fenced to the generation it covers: only launches
    /// of covered generations are signalled, and owned trees are stopped (`kill`) only while the
    /// root still carries the stop. After a Continue (or once a newer generation exists) the pass
    /// only observes what the stop covered; it never stops the newer generation's work.
    async fn stop_root_now(
        &self,
        root: &RootId,
        op: &OperationId,
        kill: Option<StopScope>,
        grace: Duration,
    ) -> Result<OperationState, StorageError> {
        let (o, r) = (op.clone(), root.clone());
        let (control, targets) = store::write(&self.store, move |tx| {
            dispatch_stop_operation(tx, &o)?;
            let control = devworks_core::services::runs::control_of(tx.runs(), &r)?;
            let targets = stop_reconciliation_targets(tx.runs(), &r, &o)?;
            Ok((control, targets))
        })
        .await?;
        let superseded = stop_superseded(&control);
        self.signal_covered(root, control.stop_generation);
        let (processes, bound) = (self.processes.clone(), self.stop_bound);
        let results = match kill {
            // While the root still carries the stop: every owned tree of its scope.
            Some(scope) if !superseded => {
                tokio::task::spawn_blocking(move || processes.stop(&scope, grace, bound))
                    .await
                    .unwrap_or_default()
            }
            // After a Continue: exactly the trees this stop holds for reconciliation, by
            // identity; the newer generation's trees are never touched.
            Some(_) => tokio::task::spawn_blocking(move || {
                targets
                    .into_iter()
                    .map(|id| {
                        let tree = OwnedProcess {
                            id: id.clone(),
                            identity: NativeIdentity {
                                pid: 0,
                                birth: String::new(),
                                group: None,
                            },
                            lease: None,
                        };
                        (id, processes.cancel(&tree, grace, bound))
                    })
                    .collect()
            })
            .await
            .unwrap_or_default(),
            None => Vec::new(),
        };
        let uncertain = unverified(results);
        let (service, root, op) = (self.service.clone(), root.clone(), op.clone());
        // Settlement adds the trees it covers that are still unsettled.
        store::write(&self.store, move |tx| {
            service.settle_stop(tx, &root, &op, &uncertain)
        })
        .await
    }

    /// Signals the launches of `root` whose activation generation is at most `generation`.
    fn signal_covered(&self, root: &RootId, generation: i64) {
        let map = self.running.lock().unwrap_or_else(|p| p.into_inner());
        for r in map
            .values()
            .filter(|r| &r.root == root && r.generation <= generation)
        {
            let _ = r.stop.send(true);
        }
    }

    /// Signals every launch whose own claim was asked to stop or lost: its activation is no
    /// longer running or claimed, or it was claimed again under a newer epoch. A relaunch of the
    /// same activation, or a newer generation of the root, is not signalled.
    async fn signal_stopping(&self) -> Result<(), StorageError> {
        let claims: Vec<(String, Id, i64)> = {
            let map = self.running.lock().unwrap_or_else(|p| p.into_inner());
            map.iter()
                .map(|(k, r)| (k.clone(), r.activation.clone(), r.epoch))
                .collect()
        };
        if claims.is_empty() {
            return Ok(());
        }
        let stopping: Vec<String> = store::read(&self.store, move |s| {
            let mut out = Vec::new();
            for (key, id, epoch) in &claims {
                let current = s.runs().activation(id)?.is_some_and(|a| {
                    a.owner_epoch == *epoch
                        && matches!(a.phase, ActivationPhase::Running | ActivationPhase::Claimed)
                });
                if !current {
                    out.push(key.clone());
                }
            }
            Ok(out)
        })
        .await?;
        let map = self.running.lock().unwrap_or_else(|p| p.into_inner());
        for key in stopping {
            if let Some(r) = map.get(&key) {
                let _ = r.stop.send(true);
            }
        }
        Ok(())
    }

    fn spawn_launch(self: &Arc<Self>, activation: StoredActivation) {
        let Some(launcher) = self.launcher.clone() else {
            return;
        };
        let (stop_tx, stop_rx) = watch::channel(false);
        // The task holds the supervisor weakly: dropping the runtime drops the supervisor (and
        // the process owner it holds), which is never kept alive by a launch.
        let weak: Weak<Self> = Arc::downgrade(self);
        let id = activation.id.clone();
        let root = activation.root_id.clone();
        let generation = activation.generation;
        let epoch = activation.owner_epoch;
        let lease = self.service.config().lease_ms;
        let every = Duration::from_millis(u64::try_from((lease / 3).max(1)).unwrap_or(1));
        let signal = stop_tx.clone();
        let future = launcher.launch(Launch {
            activation,
            stop: stop_rx,
        });
        let id_for_task = id.clone();
        // Held across the spawn: the task removes its entry under this lock when it ends, so it
        // can never remove it before it is inserted.
        let mut map = self.running.lock().unwrap_or_else(|p| p.into_inner());
        let key = launch_key(&id, epoch);
        let task_key = key.clone();
        let task = tokio::spawn(async move {
            let id = id_for_task;
            let mut future = future;
            let mut owned = true;
            let end = loop {
                match first(&mut future, tokio::time::sleep(every)).await {
                    Either::Left(end) => break Some(end),
                    Either::Right(()) => {
                        let Some(me) = weak.upgrade() else {
                            break None;
                        };
                        if me.stopping.load(Ordering::SeqCst) {
                            // Shutting down or dropped: no renewal; the launch was asked to stop.
                            let _ = signal.send(true);
                            continue;
                        }
                        if !owned {
                            continue;
                        }
                        let (service, x) = (me.service.clone(), id.clone());
                        match store::write(&me.store, move |tx| {
                            service.renew_activation(tx, &x, epoch)
                        })
                        .await
                        {
                            Ok(Ok(a)) if a.phase == ActivationPhase::Stopping => {
                                let _ = signal.send(true);
                            }
                            Ok(Ok(_)) => {}
                            Ok(Err(_)) => {
                                // Ownership lost (recovered, superseded): stop, record nothing.
                                owned = false;
                                let _ = signal.send(true);
                            }
                            Err(_) => {}
                        }
                    }
                }
            };
            let (Some(end), Some(me)) = (end, weak.upgrade()) else {
                return;
            };
            if owned {
                let (service, x) = (me.service.clone(), id.clone());
                let _ = store::write(&me.store, move |tx| {
                    let stopping = tx
                        .runs()
                        .activation(&x)?
                        .is_some_and(|a| a.phase == ActivationPhase::Stopping);
                    if stopping && let Err(f) = service.acknowledge_stop(tx, &x, epoch)? {
                        return Ok(Err(f));
                    }
                    match end {
                        LaunchEnd::Finished { outcome, detail } => {
                            service.finish_activation(tx, &x, epoch, outcome, detail.as_deref())
                        }
                        LaunchEnd::Failed(f) => service.report_failure(tx, &x, epoch, f),
                    }
                })
                .await;
            }
            // Only this claim's entry: a relaunch of the activation has its own key.
            me.running
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&task_key);
            me.wake.notify_one();
        });
        map.insert(
            key,
            Running {
                activation: id,
                generation,
                epoch,
                root,
                stop: stop_tx,
                task,
            },
        );
    }

    /// Ticks every `every` (or sooner when notified) until [`Self::stop`]. Transient storage
    /// failures are retried on the next tick.
    pub async fn run(self: Arc<Self>, every: Duration) {
        while !self.stopping.load(Ordering::SeqCst) {
            let _ = self.tick().await;
            // Backstop: a periodic restart of the dispatcher's eligibility scan, in case a
            // change outside supervision made a root eligible without a wakeup.
            self.wake_dispatcher();
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
            let _ = first(self.wake.notified(), tokio::time::sleep(every)).await;
        }
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }
}

/// The trees a stop could not verify gone, with what is known of each.
fn unverified(results: Vec<devworks_core::services::process::StopResult>) -> Vec<(Id, String)> {
    results
        .into_iter()
        .filter_map(|(id, r)| match r {
            Ok(StopOutcome::Reaped { .. }) => None,
            Ok(StopOutcome::Uncertain(why)) => Some((id, why)),
            Err(f) => match f.outcome {
                StopOutcome::Reaped { .. } => None,
                StopOutcome::Uncertain(why) => Some((id, why)),
            },
        })
        .collect()
}

/// A supervisor dropped without shutdown (or after it) aborts any launch it still tracks.
impl Drop for RunSupervisor {
    fn drop(&mut self) {
        let map = self.running.get_mut().unwrap_or_else(|p| p.into_inner());
        for (_, r) in map.drain() {
            let _ = r.stop.send(true);
            r.task.abort();
        }
    }
}

impl RunSupervisor {
    /// Shutdown step 1: hold live roots for manual Continue, ask their activations to stop,
    /// freeze schedules (hooks), signal every launch and wait at most `grace` for them.
    pub async fn quiesce(&self, grace: Duration) -> Result<Vec<RootId>, StorageError> {
        self.stop();
        let (service, hooks) = (self.service.clone(), self.hooks.clone());
        let held = store::write(&self.store, move |tx| {
            service.quiesce_for_shutdown(tx, &*hooks)
        })
        .await;
        let tasks: Vec<Running> = {
            let mut map = self.running.lock().unwrap_or_else(|p| p.into_inner());
            map.drain().map(|(_, r)| r).collect()
        };
        for r in &tasks {
            let _ = r.stop.send(true);
        }
        let deadline = Instant::now() + grace;
        for r in tasks {
            let left = deadline.saturating_duration_since(Instant::now());
            let mut task = r.task;
            if let Either::Left(()) = first(tokio::time::sleep(left), &mut task).await {
                task.abort();
            }
        }
        held
    }

    /// Shutdown step 4: every activation of this boot still live is recorded `Interrupted`.
    pub async fn finish(&self) -> Result<usize, StorageError> {
        let service = self.service.clone();
        store::write(&self.store, move |tx| service.finish_shutdown(tx)).await
    }

    /// The immediate stop of a confirmed project archival: every root is stopped under a new
    /// stop epoch and the project's owned trees are killed at once (no cooperative grace).
    pub async fn stop_project_now(
        &self,
        project: &ProjectId,
    ) -> Result<Vec<(RootId, OperationState)>, StorageError> {
        let (service, p) = (self.service.clone(), project.clone());
        let stopped = store::write(&self.store, move |tx| {
            let host = tx.store().host_actor.clone();
            service.stop_project(tx, &host, &p)
        })
        .await?;
        self.kill_project(project, stopped).await
    }

    /// The immediate stop after a committed `projects.archive`, whose unit of work already
    /// fenced the project and journaled one stop per root: while the project is still closed to
    /// work, its owned trees are killed at once (no cooperative grace) and each root's
    /// unfinished stop is executed and observed. A replayed or repeated call is harmless: a
    /// restored project is left alone, and finished stops are not executed again.
    pub async fn stop_archived_project(
        &self,
        project: &ProjectId,
    ) -> Result<Vec<(RootId, OperationState)>, StorageError> {
        let p = project.clone();
        let stops = store::read(&self.store, move |s| {
            let closed = s
                .runs()
                .project_lifecycle(&p)?
                .is_some_and(|l| devworks_core::services::runs::closed_to_work(&l));
            if !closed {
                return Ok(None);
            }
            let mut out = Vec::new();
            let mut after: Option<RootId> = None;
            loop {
                let page = s.runs().project_roots(&p, after.as_ref(), 500)?;
                for root in &page {
                    let control = devworks_core::services::runs::control_of(s.runs(), root)?;
                    if let Some(op) = control.stop_operation_id
                        && s.operations()
                            .operation(&op)?
                            .is_some_and(|o| !is_terminal(o.state))
                    {
                        out.push((root.clone(), op));
                    }
                }
                if page.len() < 500 {
                    return Ok(Some(out));
                }
                after = page.last().cloned();
            }
        })
        .await?;
        match stops {
            Some(stops) => self.kill_project(project, stops).await,
            None => Ok(Vec::new()),
        }
    }

    /// Kills every owned tree of `project` at once, then executes and observes each root stop.
    async fn kill_project(
        &self,
        project: &ProjectId,
        stopped: Vec<(RootId, OperationId)>,
    ) -> Result<Vec<(RootId, OperationState)>, StorageError> {
        // One immediate project-wide kill, whether or not the project has roots: trees the
        // project owns without a root are stopped too.
        let (processes, bound, scope) = (
            self.processes.clone(),
            self.stop_bound,
            StopScope::Project(project.clone()),
        );
        // What each tree's stop verified is in its process record; each root's settlement
        // reads its own unsettled trees from there.
        let _ = tokio::task::spawn_blocking(move || processes.stop(&scope, Duration::ZERO, bound))
            .await;
        let mut out = Vec::new();
        for (root, op) in stopped {
            // The roots' trees were stopped above; each stop is signalled and observed.
            let state = self.stop_root_now(&root, &op, None, Duration::ZERO).await?;
            out.push((root, state));
        }
        // Rootless trees that could not be verified stopped stay visible in their process
        // records (`Uncertain`); there is no root to hold for them.
        Ok(out)
    }
}
