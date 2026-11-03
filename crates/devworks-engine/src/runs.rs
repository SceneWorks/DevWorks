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
use crate::dispatch::{Either, RootEligibility, first};
use crate::store;
use devworks_core::ports::Clock;
use devworks_core::ports::storage::{
    ActivationOutcome, ActivationPhase, ApplicationStore, QueueEntry, StorageError,
    StoredActivation, WriteTransaction,
};
use devworks_core::protocol::{Id, OperationId, OperationState, ProjectId, RootId};
use devworks_core::services::process::{ProcessSupervisor, StopOutcome, StopScope};
use devworks_core::services::runs::ControlService;
use devworks_core::services::runs::recovery::{
    BootRecovery, LifecycleHooks, dispatch_stop_operation,
};
use devworks_core::services::runs::substrate::ActivationFailure;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
    pub launched: Vec<Id>,
}

struct Running {
    root: RootId,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
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
        let mut ids: Vec<Id> = map.keys().filter_map(|k| Id::new(k.clone()).ok()).collect();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        ids
    }

    /// Boot recovery of run roots (before any delivery, claim or command).
    pub async fn boot(&self) -> Result<BootRecovery, StorageError> {
        let (service, hooks) = (self.service.clone(), self.hooks.clone());
        store::write(&self.store, move |tx| service.recover_boot(tx, &*hooks)).await
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
        self.signal_stopping().await?;
        report.stops = self.execute_stops().await?;
        if self.launcher.is_some() && !self.stopping.load(Ordering::SeqCst) {
            loop {
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
                report.launched.push(running.id.clone());
                self.spawn_launch(running);
            }
        }
        Ok(report)
    }

    /// Executes every journaled stop still to be observed: the stop is marked dispatching, the
    /// root's owned trees are stopped (bounded), and the outcome is recorded.
    async fn execute_stops(&self) -> Result<Vec<(RootId, OperationState)>, StorageError> {
        let pending = store::read(&self.store, |s| s.runs().pending_stops(None, 500)).await?;
        let mut out = Vec::new();
        for c in pending {
            let Some(op) = c.stop_operation_id.clone() else {
                continue;
            };
            let state = self
                .stop_root_now(
                    &c.root_id,
                    &op,
                    StopScope::Root(c.root_id.clone()),
                    self.stop_grace,
                )
                .await?;
            out.push((c.root_id, state));
        }
        Ok(out)
    }

    async fn stop_root_now(
        &self,
        root: &RootId,
        op: &OperationId,
        scope: StopScope,
        grace: Duration,
    ) -> Result<OperationState, StorageError> {
        let o = op.clone();
        store::write(&self.store, move |tx| dispatch_stop_operation(tx, &o)).await?;
        // A launched activation of the root is asked to stop as well.
        self.signal_root(root);
        let (processes, bound) = (self.processes.clone(), self.stop_bound);
        let results = tokio::task::spawn_blocking(move || processes.stop(&scope, grace, bound))
            .await
            .unwrap_or_default();
        let uncertain: Vec<(Id, String)> = results
            .into_iter()
            .filter_map(|(id, r)| match r {
                Ok(StopOutcome::Reaped { .. }) => None,
                Ok(StopOutcome::Uncertain(why)) => Some((id, why)),
                Err(f) => match f.outcome {
                    StopOutcome::Reaped { .. } => None,
                    StopOutcome::Uncertain(why) => Some((id, why)),
                },
            })
            .collect();
        let (service, root, op) = (self.service.clone(), root.clone(), op.clone());
        store::write(&self.store, move |tx| {
            let mut uncertain = uncertain;
            // Trees recorded but not owned here (an earlier boot's, or never confirmed) are not
            // verified stopped either.
            for p in devworks_core::services::runs::recovery::unsettled_processes(tx, &root)? {
                if !uncertain.iter().any(|(id, _)| id == &p.0) {
                    uncertain.push(p);
                }
            }
            service.settle_stop(tx, &root, &op, &uncertain)
        })
        .await
    }

    fn signal_root(&self, root: &RootId) {
        let map = self.running.lock().unwrap_or_else(|p| p.into_inner());
        for r in map.values().filter(|r| &r.root == root) {
            let _ = r.stop.send(true);
        }
    }

    /// Signals every launch whose activation was asked to stop (a pause, stop or archival
    /// committed since its last renewal).
    async fn signal_stopping(&self) -> Result<(), StorageError> {
        let ids = self.launched();
        if ids.is_empty() {
            return Ok(());
        }
        let stopping: Vec<RootId> = store::read(&self.store, move |s| {
            let mut out = Vec::new();
            for id in &ids {
                if let Some(a) = s.runs().activation(id)?
                    && a.phase != ActivationPhase::Running
                    && a.phase != ActivationPhase::Claimed
                {
                    out.push(a.root_id);
                }
            }
            Ok(out)
        })
        .await?;
        for root in stopping {
            self.signal_root(&root);
        }
        Ok(())
    }

    fn spawn_launch(self: &Arc<Self>, activation: StoredActivation) {
        let Some(launcher) = self.launcher.clone() else {
            return;
        };
        let (stop_tx, stop_rx) = watch::channel(false);
        let me = self.clone();
        let id = activation.id.clone();
        let root = activation.root_id.clone();
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
        let task = tokio::spawn(async move {
            let id = id_for_task;
            let mut future = future;
            let mut owned = true;
            let end = loop {
                match first(&mut future, tokio::time::sleep(every)).await {
                    Either::Left(end) => break end,
                    Either::Right(()) => {
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
            me.running
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(id.as_str());
            me.wake.notify_one();
        });
        let key = id.as_str().to_owned();
        map.insert(
            key,
            Running {
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
        let mut out = Vec::new();
        let mut first_root = true;
        for (root, op) in stopped {
            // One immediate project-wide kill covers every root's trees.
            let scope = if first_root {
                StopScope::Project(project.clone())
            } else {
                StopScope::Root(root.clone())
            };
            first_root = false;
            let state = self
                .stop_root_now(&root, &op, scope, Duration::ZERO)
                .await?;
            out.push((root, state));
        }
        Ok(out)
    }
}
