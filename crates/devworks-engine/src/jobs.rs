//! The schedule runner (JOB-01..09): what the engine runs while the app is open on top of core's
//! [`JobService`] steps.
//!
//! [`Scheduler::tick`] claims due occurrences, settles native runs whose accepted operation
//! ended, executes requested cancellations of runs it is not executing (owned process trees of the
//! run's operations are stopped through the process owner, an inference run through its
//! pipeline, then the verified outcome is recorded), reclaims started runs no task executes any
//! more, and starts every startable run it is not already executing. Each run executes on its own
//! task: start → dispatching → dispatch (off the async thread, holding no lock) → settle, each
//! step one unit of work. A step after the start that fails (the writer busy, say) is retried
//! with backoff, never abandoned: the run is durably started. In-flight runs are keyed by run
//! identity, so a cancellation or settle can never reach a later run of the job.
//!
//! Pacing uses the monotonic clock (`tokio::time`); whether an occurrence is due uses the store's
//! wall clock. A sleep, a wall-clock jump or a slow tick never runs an occurrence twice (claims
//! advance strictly past the occurrence, whose identity is unique) or late (the grace rule).
//!
//! [`Scheduler::stop`] freezes claims and starts at once (shutdown step 1, before admission
//! closes); [`Scheduler::drain`] waits for in-flight runs within the shutdown bound and then
//! abandons them: a run whose dispatch did not finish is reconciled at the next boot.
use crate::dispatch::{Either, first};
use crate::store;
use devworks_core::ports::storage::{ApplicationStore, StorageError, StoredJobRun};
use devworks_core::protocol::Id;
use devworks_core::services::jobs::JobService;
use devworks_core::services::jobs::runner::{
    Begun, InferenceJobs, NativeJobDispatch, Step, StopVerdict,
};
use devworks_core::services::process::{ProcessSupervisor, StopOutcome, StopScope};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// Identical resubmissions of a native command whose acknowledgment was lost, before the run is
/// recorded `OutcomeUnknown` (the gateway replays a committed receipt; it never applies twice).
pub const UNKNOWN_RESUBMISSIONS: usize = 3;
/// The longest wait between retries of a started run's failed step.
pub const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(30);

/// What one tick did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SchedulerTick {
    /// Runs queued by due occurrences.
    pub queued: Vec<Id>,
    /// Runs started on their own task.
    pub started: Vec<Id>,
    /// Runs whose requested cancellation was settled.
    pub cancelled: Vec<Id>,
    /// Native runs settled from their accepted operation.
    pub settled: usize,
    /// Started runs no task executed any more, queued again or settled ([`JobService::reclaim`]).
    pub reclaimed: Vec<Id>,
}

/// The schedule runner.
pub struct Scheduler {
    store: Arc<dyn ApplicationStore>,
    service: Arc<JobService>,
    native: Arc<dyn NativeJobDispatch>,
    inference: Arc<dyn InferenceJobs>,
    processes: Arc<ProcessSupervisor>,
    /// Cooperative grace and bound of one cancellation's process stop.
    pub stop_grace: Duration,
    pub stop_bound: Duration,
    /// The first wait before retrying a started run's failed step (doubling up to
    /// [`MAX_RETRY_BACKOFF`]).
    pub retry_backoff: Duration,
    running: Mutex<HashMap<String, JoinHandle<()>>>,
    /// The runs a task is executing, each removed by its own task when it ends (or is
    /// abandoned): unlike the join handles, which [`Self::drain`] takes while it waits for them,
    /// this never shows a live execution as gone, so a tick never reclaims or cancels a run its
    /// task still executes.
    live: Mutex<HashSet<String>>,
    /// One tick at a time (the loop's and an explicit one never interleave, so a cancellation's
    /// native stop runs once per tick).
    ticking: tokio::sync::Mutex<()>,
    wake: Notify,
    stopping: AtomicBool,
}

impl Scheduler {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        service: Arc<JobService>,
        native: Arc<dyn NativeJobDispatch>,
        inference: Arc<dyn InferenceJobs>,
        processes: Arc<ProcessSupervisor>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            service,
            native,
            inference,
            processes,
            stop_grace: Duration::from_secs(5),
            stop_bound: Duration::from_secs(30),
            retry_backoff: Duration::from_millis(100),
            running: Mutex::new(HashMap::new()),
            live: Mutex::new(HashSet::new()),
            ticking: tokio::sync::Mutex::new(()),
            wake: Notify::new(),
            stopping: AtomicBool::new(false),
        })
    }
    pub fn service(&self) -> &Arc<JobService> {
        &self.service
    }
    /// Runs currently executing on their own task.
    pub fn in_flight(&self) -> Vec<Id> {
        let live = self.live.lock().unwrap_or_else(|p| p.into_inner());
        let mut ids: Vec<Id> = live
            .iter()
            .filter_map(|k| Id::new(k.clone()).ok())
            .collect();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        ids
    }
    fn executing(&self, run: &Id) -> bool {
        self.live
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(run.as_str())
    }
    /// Requests a tick now (a committed Run now, cancel, enable or edit).
    pub fn notify(&self) {
        self.wake.notify_one();
    }

    /// One pass (see the module documentation).
    pub async fn tick(self: &Arc<Self>) -> Result<SchedulerTick, StorageError> {
        let mut report = SchedulerTick::default();
        let _one = self.ticking.lock().await;
        if self.stopping.load(Ordering::SeqCst) {
            return Ok(report);
        }
        let svc = self.service.clone();
        report.queued = store::write(&self.store, move |tx| svc.claim_due(tx))
            .await?
            .into_iter()
            .map(|r| r.id)
            .collect();
        let svc = self.service.clone();
        report.settled = store::write(&self.store, move |tx| svc.poll_inner(tx)).await?;
        let svc = self.service.clone();
        let cancels = store::write(&self.store, move |tx| svc.cancellations(tx)).await?;
        for run in cancels {
            if self.executing(&run.id) {
                // Its own task settles it once its dispatch returns (the run knows it was asked).
                continue;
            }
            if let Some(done) = self.cancel(run).await? {
                report.cancelled.push(done);
            }
        }
        if self.stopping.load(Ordering::SeqCst) {
            return Ok(report);
        }
        // A start that committed after its task ended (its acknowledgment lost, or the task
        // gone): queued again, or settled from what is known of its effect.
        let svc = self.service.clone();
        let unsettled = store::write(&self.store, move |tx| svc.unsettled_starts(tx)).await?;
        for run in unsettled {
            if self.executing(&run.id) {
                continue;
            }
            let (svc, id) = (self.service.clone(), run.id.clone());
            if store::write(&self.store, move |tx| svc.reclaim(tx, &id))
                .await?
                .is_some()
            {
                report.reclaimed.push(run.id);
            }
        }
        let svc = self.service.clone();
        let runnable = store::write(&self.store, move |tx| svc.runnable(tx)).await?;
        for run in runnable {
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
            if self.executing(&run.id) {
                continue;
            }
            report.started.push(run.id.clone());
            self.spawn(run.id);
        }
        Ok(report)
    }

    /// Stops what a cancelled run left running and records the verified outcome.
    async fn cancel(&self, run: StoredJobRun) -> Result<Option<Id>, StorageError> {
        let mut verdict = StopVerdict::Stopped;
        let scopes: Vec<StopScope> = run
            .operation_id
            .iter()
            .chain(run.inner_operation_id.iter())
            .cloned()
            .map(StopScope::Operation)
            .collect();
        let (procs, grace, bound) = (self.processes.clone(), self.stop_grace, self.stop_bound);
        let uncertain = tokio::task::spawn_blocking(move || {
            let mut uncertain = Vec::new();
            for scope in scopes {
                for (id, r) in procs.stop(&scope, grace, bound) {
                    let outcome = match r {
                        Ok(o) => o,
                        Err(f) => f.outcome,
                    };
                    if let StopOutcome::Uncertain(why) = outcome {
                        uncertain.push(format!("{}: {why}", id.as_str()));
                    }
                }
            }
            uncertain
        })
        .await
        .unwrap_or_else(|_| vec!["the process stop task failed".into()]);
        if !uncertain.is_empty() {
            verdict = StopVerdict::Uncertain(uncertain.join("; "));
        }
        if run.agent_actor.is_some() || run.session_id.is_some() {
            let (inference, id) = (self.inference.clone(), run.id.clone());
            let stopped = tokio::task::spawn_blocking(move || inference.cancel(&id))
                .await
                .unwrap_or_else(|_| Err("the pipeline cancel task failed".into()));
            match stopped {
                Ok(true) => {}
                Ok(false) => {
                    verdict = StopVerdict::Uncertain("the agent has not stopped yet".into())
                }
                Err(why) => verdict = StopVerdict::Uncertain(why),
            }
        }
        let (svc, id, revision) = (self.service.clone(), run.id.clone(), run.revision);
        let settled = store::write(&self.store, move |tx| {
            svc.settle_cancel(tx, &id, revision, &verdict)
        })
        .await?;
        Ok(settled.map(|r| r.id))
    }

    fn spawn(self: &Arc<Self>, run: Id) {
        let me = self.clone();
        let key = run.as_str().to_owned();
        if !self
            .live
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key.clone())
        {
            return;
        }
        let mut map = self.running.lock().unwrap_or_else(|p| p.into_inner());
        map.retain(|_, t| !t.is_finished());
        let k = key.clone();
        let task = tokio::spawn(async move {
            let _live = Executing(&me, k);
            let _ = me.execute(run).await;
        });
        map.insert(key, task);
    }

    /// Runs one transactional step of a started run, retrying it with backoff while it fails:
    /// the run is durably started, so giving up would strand it. Every step re-reads the run and
    /// is fenced by its identity and attempt, so a retry after a commit whose acknowledgment was
    /// lost changes nothing. Once shutdown began it gives up; the next boot reconciles the run.
    async fn persist<T, F>(&self, step: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: Fn(&mut dyn devworks_core::ports::storage::WriteTransaction) -> Result<T, StorageError>
            + Clone
            + Send
            + 'static,
    {
        let mut wait = self.retry_backoff;
        loop {
            match store::write(&self.store, step.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) if self.stopping.load(Ordering::SeqCst) => return Err(e),
                Err(_) => {
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(MAX_RETRY_BACKOFF);
                }
            }
        }
    }

    /// Executes one startable run: start, mark dispatching, dispatch, settle.
    pub async fn execute(&self, run: Id) -> Result<(), StorageError> {
        let (svc, inference, id) = (self.service.clone(), self.inference.clone(), run.clone());
        // Not retried: a start that failed never started (the next tick starts it again), and
        // one whose acknowledgment was lost is reclaimed by the next tick.
        let Some(begun) =
            store::write(&self.store, move |tx| svc.begin(tx, &id, &*inference)).await?
        else {
            return Ok(());
        };
        if self.stopping.load(Ordering::SeqCst) {
            return self.release(begun).await;
        }
        let (svc, b) = (self.service.clone(), Arc::new(begun.clone()));
        if !self.persist(move |tx| svc.dispatching(tx, &b)).await? {
            return self.release(begun).await;
        }
        match begun.step.clone() {
            Step::Native {
                ctx,
                operation,
                frame,
            } => {
                let native = self.native.clone();
                let dispatched = tokio::task::spawn_blocking(move || {
                    let mut outcome = native.command(&ctx, operation, &frame);
                    for _ in 0..UNKNOWN_RESUBMISSIONS {
                        if !matches!(
                            outcome,
                            Ok(devworks_core::protocol::CommandReply::CommandStatusUnknown { .. })
                        ) {
                            break;
                        }
                        outcome = native.command(&ctx, operation, &frame);
                    }
                    outcome
                })
                .await;
                let (svc, begun) = (self.service.clone(), Arc::new(begun));
                match dispatched {
                    Ok(outcome) => {
                        let outcome = Arc::new(outcome);
                        self.persist(move |tx| svc.settle_native(tx, &begun, &outcome))
                            .await?;
                    }
                    // The call did not return: whether it committed is established from its
                    // receipt, never assumed.
                    Err(_) => {
                        self.persist(move |tx| svc.reconcile_dispatch(tx, &begun))
                            .await?;
                    }
                }
            }
            Step::Inference(start) => {
                let inference = self.inference.clone();
                let started = tokio::task::spawn_blocking(move || inference.start(&start))
                    .await
                    .unwrap_or_else(|_| {
                        Err(devworks_core::services::jobs::runner::InferenceRefusal {
                            dispatched: true,
                            reason: "the inference handoff panicked".into(),
                        })
                    });
                let (svc, begun, started) =
                    (self.service.clone(), Arc::new(begun), Arc::new(started));
                self.persist(move |tx| svc.settle_inference_start(tx, &begun, &started))
                    .await?;
            }
        }
        self.notify();
        Ok(())
    }

    async fn release(&self, begun: Begun) -> Result<(), StorageError> {
        let (svc, begun) = (self.service.clone(), Arc::new(begun));
        self.persist(move |tx| svc.undispatched(tx, &begun)).await
    }

    /// Ticks every `every` (or sooner when notified) until [`Self::stop`].
    pub async fn run(self: Arc<Self>, every: Duration) {
        while !self.stopping.load(Ordering::SeqCst) {
            let _ = self.tick().await;
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
            let _ = first(self.wake.notified(), tokio::time::sleep(every)).await;
        }
    }

    /// Freezes claims and starts (shutdown step 1).
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }

    /// Waits for in-flight runs until `deadline`, then abandons the rest (their dispatch, if it
    /// happened, is reconciled at the next boot).
    pub async fn drain(&self, deadline: Instant) {
        let tasks: Vec<JoinHandle<()>> = {
            let mut map = self.running.lock().unwrap_or_else(|p| p.into_inner());
            map.drain().map(|(_, t)| t).collect()
        };
        for mut task in tasks {
            let left = deadline.saturating_duration_since(Instant::now());
            if let Either::Left(()) = first(tokio::time::sleep(left), &mut task).await {
                task.abort();
            }
        }
    }
}

/// Marks a run executing for as long as its task holds it (dropped when the task ends or is
/// aborted).
struct Executing<'a>(&'a Scheduler, String);
impl Drop for Executing<'_> {
    fn drop(&mut self) {
        self.0
            .live
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.1);
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        let map = self.running.get_mut().unwrap_or_else(|p| p.into_inner());
        for (_, t) in map.drain() {
            t.abort();
        }
    }
}
