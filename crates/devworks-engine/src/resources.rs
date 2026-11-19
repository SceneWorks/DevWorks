//! Resource and owned-process housekeeping for the application lifecycle (RES-03, the process
//! owner): what the composition root runs at boot, while open and at shutdown.
//!
//! * [`Housekeeper::boot`] registers the machine resources (the desktop by default), then runs
//!   the process supervisor's recovery of trees from earlier boots and the resource boot
//!   reconciliation, before any work is admitted.
//! * [`Housekeeper::tick`] sweeps expiries, transfers and promotions. Sleep/wake is detected the
//!   same way as the dispatcher's (the wall clock advanced well beyond Tokio's monotonic clock)
//!   and recorded as a `Wake` reconciliation; otherwise it is a `Sweep` that records history only
//!   when it changed something. [`Housekeeper::run`] ticks at a bounded interval until stopped.
//! * [`Housekeeper::shutdown`] cancels every owned tree within one bound and records the
//!   `Shutdown` reconciliation.
use crate::store;
use crate::workers::Ticker;
use devworks_core::locks::LockExt;
use devworks_core::ports::Clock;
use devworks_core::ports::storage::WriteOptions;
use devworks_core::ports::storage::{ApplicationStore, StorageError, StoredResource};
use devworks_core::services::process::{ProcessSupervisor, RecoveryReport, ShutdownReport};
use devworks_core::services::resources::{
    MachineResource, ReconcileCause, ReconcileReport, ResourceService, register_machine,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

/// What boot housekeeping found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BootReport {
    pub machine_resources: usize,
    pub recovery: RecoveryReport,
}

pub struct Housekeeper {
    store: Arc<dyn ApplicationStore>,
    clock: Arc<dyn Clock>,
    resources: Arc<ResourceService>,
    supervisor: Arc<ProcessSupervisor>,
    /// A wall-clock advance exceeding the monotonic advance by more than this is a sleep/wake.
    pub wake_jump_ms: i64,
    last: Mutex<Option<(Instant, i64)>>,
    ticker: Ticker,
}

impl Housekeeper {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        clock: Arc<dyn Clock>,
        resources: Arc<ResourceService>,
        supervisor: Arc<ProcessSupervisor>,
    ) -> Self {
        Self {
            store,
            clock,
            resources,
            supervisor,
            wake_jump_ms: 10_000,
            last: Mutex::new(None),
            ticker: Ticker::new(),
        }
    }

    /// The process supervisor every owned tree is launched through.
    pub fn supervisor(&self) -> &Arc<ProcessSupervisor> {
        &self.supervisor
    }

    /// Registers `machine` resources, then recovers earlier boots' trees and leases.
    pub async fn boot(&self, machine: Vec<MachineResource>) -> Result<BootReport, StorageError> {
        let registered: Vec<StoredResource> =
            store::write(&self.store, WriteOptions::recovery(), move |tx| {
                machine.iter().map(|m| register_machine(tx, m)).collect()
            })
            .await?;
        let supervisor = self.supervisor.clone();
        let recovery = tokio::task::spawn_blocking(move || supervisor.recover())
            .await
            .map_err(|_| StorageError::Cancelled)??;
        Ok(BootReport {
            machine_resources: registered.len(),
            recovery,
        })
    }

    /// One sweep; a detected sleep/wake is recorded as a `Wake` reconciliation.
    pub async fn tick(&self) -> Result<(ReconcileCause, ReconcileReport), StorageError> {
        let wall = self.clock.now_ms().get();
        let mono = Instant::now();
        let woke = {
            let mut last = self.last.locked();
            let woke = last.is_some_and(|(m0, w0)| {
                let mono_ms =
                    i64::try_from(mono.duration_since(m0).as_millis()).unwrap_or(i64::MAX);
                (wall - w0).saturating_sub(mono_ms) > self.wake_jump_ms
            });
            *last = Some((mono, wall));
            woke
        };
        let cause = if woke {
            ReconcileCause::Wake
        } else {
            ReconcileCause::Sweep
        };
        let (resources, c) = (self.resources.clone(), cause.clone());
        let report = store::write(&self.store, WriteOptions::recovery(), move |tx| {
            resources.reconcile(tx, c)
        })
        .await?;
        Ok((cause, report))
    }

    /// Ticks every `every` until [`Self::stop`], which ends the wait at once. Transient storage
    /// failures are retried on the next tick.
    pub async fn run(&self, every: Duration) {
        self.ticker
            .run(every, || async {
                let _ = self.tick().await;
            })
            .await;
    }

    pub fn stop(&self) {
        self.ticker.stop();
    }

    /// Cancels every owned tree within one bound and records the shutdown reconciliation. Trees
    /// are stopped natively even when the store refuses writes; failures are in the report.
    pub async fn shutdown(&self, grace: Duration, bound: Duration) -> ShutdownReport {
        self.stop();
        let supervisor = self.supervisor.clone();
        tokio::task::spawn_blocking(move || supervisor.shutdown(grace, bound))
            .await
            .unwrap_or_else(|_| ShutdownReport {
                stopped: Vec::new(),
                containers: None,
                unclosed_inference_attempts: Vec::new(),
                reconciliation: Err(StorageError::unavailable(
                    devworks_core::ports::storage::Unavailable::HandlerPanicked,
                    "process shutdown panicked",
                )),
            })
    }
}
