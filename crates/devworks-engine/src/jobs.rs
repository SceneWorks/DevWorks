//! The schedule runner (JOB-01..09): what the engine runs while the app is open on top of core's
//! [`JobService`] steps.
//!
//! [`Scheduler::tick`] claims due occurrences, settles native runs whose accepted operation
//! ended, executes requested cancellations of runs it is not executing (owned process trees of the
//! run's operations are stopped through the process owner, an inference run through its
//! pipeline, then the verified outcome is recorded) and starts every queued run it is not already
//! executing. Each run executes on its own task: start → dispatching → dispatch (off the async
//! thread, holding no lock) → settle, each step one unit of work. In-flight runs are keyed by run
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
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// Identical resubmissions of a native command whose acknowledgment was lost, before the run is
/// recorded `OutcomeUnknown` (the gateway replays a committed receipt; it never applies twice).
pub const UNKNOWN_RESUBMISSIONS: usize = 3;

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
    running: Mutex<HashMap<String, JoinHandle<()>>>,
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
            running: Mutex::new(HashMap::new()),
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
        let map = self.running.lock().unwrap_or_else(|p| p.into_inner());
        let mut ids: Vec<Id> = map
            .iter()
            .filter(|(_, t)| !t.is_finished())
            .filter_map(|(k, _)| Id::new(k.clone()).ok())
            .collect();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        ids
    }
    fn executing(&self, run: &Id) -> bool {
        let mut map = self.running.lock().unwrap_or_else(|p| p.into_inner());
        map.retain(|_, t| !t.is_finished());
        map.contains_key(run.as_str())
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
        let mut map = self.running.lock().unwrap_or_else(|p| p.into_inner());
        map.retain(|_, t| !t.is_finished());
        if map.contains_key(&key) {
            return;
        }
        let task = tokio::spawn(async move {
            let _ = me.execute(run).await;
        });
        map.insert(key, task);
    }

    /// Executes one queued run: start, mark dispatching, dispatch, settle.
    pub async fn execute(&self, run: Id) -> Result<(), StorageError> {
        let (svc, inference, id) = (self.service.clone(), self.inference.clone(), run.clone());
        let Some(begun) =
            store::write(&self.store, move |tx| svc.begin(tx, &id, &*inference)).await?
        else {
            return Ok(());
        };
        if self.stopping.load(Ordering::SeqCst) {
            return self.release(begun).await;
        }
        let (svc, b) = (self.service.clone(), begun.clone());
        if !store::write(&self.store, move |tx| svc.dispatching(tx, &b)).await? {
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
                let svc = self.service.clone();
                match dispatched {
                    Ok(outcome) => {
                        store::write(&self.store, move |tx| {
                            svc.settle_native(tx, &begun, &outcome)
                        })
                        .await?;
                    }
                    // The call did not return: whether it committed is established from its
                    // receipt, never assumed.
                    Err(_) => {
                        store::write(&self.store, move |tx| svc.reconcile_dispatch(tx, &begun))
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
                let svc = self.service.clone();
                store::write(&self.store, move |tx| {
                    svc.settle_inference_start(tx, &begun, &started)
                })
                .await?;
            }
        }
        self.notify();
        Ok(())
    }

    async fn release(&self, begun: Begun) -> Result<(), StorageError> {
        let svc = self.service.clone();
        store::write(&self.store, move |tx| svc.undispatched(tx, &begun)).await
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

impl Drop for Scheduler {
    fn drop(&mut self) {
        let map = self.running.get_mut().unwrap_or_else(|p| p.into_inner());
        for (_, t) in map.drain() {
            t.abort();
        }
    }
}
