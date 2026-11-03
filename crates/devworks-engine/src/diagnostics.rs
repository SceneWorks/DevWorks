//! Runtime side of local diagnostics (OPS-01, LOG-03): what [`crate::runtime::boot`] records and
//! reports through the core `DiagnosticService`.
//!
//! * [`record_boot`] writes one `boot` record of what recovery established (queue and journal,
//!   artifacts, owned processes and leases, keep-awake), and [`record_boot_failure`] one for a
//!   boot that stopped, so a failed or partial recovery stays visible after the restart.
//! * [`sources`] registers the runtime's health sources: `boot.recovery` (from the
//!   [`BootReport`]), `artifacts.collection` (the latest collection pass) and `queue.dispatcher`.
//! * [`Monitor`] runs the retention pass, the monitor probes (store inspection, integrity check,
//!   write-owner probe) and the sweep of stale export staging files at boot and then every
//!   `RuntimeConfig::diagnostics_interval`.
use crate::dispatch::{self, Dispatcher};
use crate::runtime::{BootError, BootReport, CollectionPass};
use devworks_core::ports::diagnostics::Metric;
use devworks_core::ports::storage::StorageError;
use devworks_core::protocol::records::{
    DiagnosticErrorClass, DiagnosticOutcome, HealthState, Severity,
};
use devworks_core::services::diagnostics::{
    DiagnosticService, HealthCheck, LogEntry, MetricReading, RecoveryLink, channels,
};
use devworks_core::services::power::KeepAwakeStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{Notify, watch};

fn count(name: &str, n: usize) -> MetricReading {
    MetricReading::new(name, Some("count"), Metric::Available(n as u64))
}

fn boot_metrics(r: &BootReport) -> Vec<MetricReading> {
    let p = &r.resources.recovery;
    vec![
        count("operationsClassified", r.queue.classified.len()),
        count("deliveriesRequeued", r.queue.requeued.len()),
        count("deliveriesHeld", r.queue.held.len()),
        count("deliveriesCancelled", r.queue.cancelled.len()),
        count("stagingCompleted", r.artifacts.completed.len()),
        count("stagingFailed", r.artifacts.failed.len()),
        count("orphanFilesRemoved", r.artifacts.removed_orphans.len()),
        count("missingBlobs", r.artifacts.missing.len()),
        count("quarantined", r.artifacts.quarantined.len()),
        count(
            "removalFailures",
            r.artifacts.failed_unlinks.len() + r.artifacts.quarantine_failed.len(),
        ),
        count("machineResources", r.resources.machine_resources),
        count("processesReaped", p.reaped),
        count("processesKilled", p.killed),
        count("processesUncertain", p.uncertain),
        count("processesLost", p.lost),
    ]
}

/// What needs attention after a boot's recovery (empty: nothing).
fn attention(r: &BootReport) -> Vec<String> {
    let mut out = Vec::new();
    let p = &r.resources.recovery;
    if !r.queue.held.is_empty() {
        out.push(format!(
            "{} deliveries whose operation may have run are held for reconciliation",
            r.queue.held.len()
        ));
    }
    if !r.artifacts.missing.is_empty() {
        out.push(format!(
            "{} managed files are missing",
            r.artifacts.missing.len()
        ));
    }
    let failed = r.artifacts.failed_unlinks.len() + r.artifacts.quarantine_failed.len();
    if failed > 0 {
        out.push(format!("{failed} file removals or quarantines failed"));
    }
    if p.uncertain + p.lost > 0 {
        out.push(format!(
            "{} process trees of earlier boots could not be verified stopped",
            p.uncertain + p.lost
        ));
    }
    if let KeepAwakeStatus::Unavailable(why) = &r.keep_awake {
        out.push(format!("keep-awake is unavailable: {why}"));
    }
    out
}

fn boot_links(r: &BootReport) -> Vec<RecoveryLink> {
    let mut links = Vec::new();
    if !r.queue.held.is_empty() {
        links.push(RecoveryLink::new(
            "queue.inspect",
            "Inspect the deliveries held for reconciliation",
        ));
        links.push(RecoveryLink::new(
            "queue.reconcile",
            "Reconcile a held delivery once its outcome is known",
        ));
    }
    let p = &r.resources.recovery;
    if p.uncertain + p.lost > 0 {
        links.push(RecoveryLink::new(
            "process.cancel",
            "Stop a process tree that could not be verified stopped",
        ));
    }
    links
}

/// Records what boot recovery established (component `boot`).
pub fn record_boot(service: &DiagnosticService, report: &BootReport) {
    let attention = attention(report);
    let (severity, outcome, message) = if attention.is_empty() {
        (
            Severity::Info,
            DiagnosticOutcome::Succeeded,
            "Boot recovery completed; nothing needs attention.".to_owned(),
        )
    } else {
        (
            Severity::Warning,
            DiagnosticOutcome::Partial,
            format!("Boot recovery completed: {}.", attention.join("; ")),
        )
    };
    let entry = LogEntry {
        operation: Some("runtime.boot".into()),
        stage: Some("recovery".into()),
        outcome: Some(outcome),
        metrics: boot_metrics(report),
        recovery: boot_links(report),
        ..LogEntry::new(channels::BOOT, "recovered", severity, message)
    };
    // Best effort: a store that refuses the record already reports itself through storage health.
    let _ = service.log(entry);
}

/// Records why boot stopped (component `boot`, outcome `Failed`), if the store still accepts it.
pub fn record_boot_failure(service: &DiagnosticService, error: &BootError) {
    let (stage, cause) = match error {
        BootError::Composition(e) => ("composition", e),
        BootError::Queue(e) => ("queue", e),
        BootError::Artifacts(e) => ("artifacts", e),
        BootError::Resources(e) => ("resources", e),
        BootError::Settings(e) => ("settings", e),
    };
    let class = match cause {
        StorageError::Busy | StorageError::CommitUnknown => DiagnosticErrorClass::Transient,
        StorageError::IncompatibleSchema(_) => DiagnosticErrorClass::Configuration,
        StorageError::InvalidRecord(_) => DiagnosticErrorClass::InvalidInput,
        StorageError::StorageUnavailable { .. } => DiagnosticErrorClass::Dependency,
        StorageError::Cancelled => DiagnosticErrorClass::Cancelled,
        _ => DiagnosticErrorClass::Internal,
    };
    let entry = LogEntry {
        operation: Some("runtime.boot".into()),
        stage: Some(stage.into()),
        outcome: Some(DiagnosticOutcome::Failed),
        error_class: Some(class),
        output: Some(format!("{cause:?}")),
        ..LogEntry::new(
            channels::BOOT,
            "failed",
            Severity::Error,
            format!("Boot stopped during {stage} recovery; nothing was started."),
        )
    };
    let _ = service.log(entry);
}

fn boot_check(r: &BootReport, at: i64) -> HealthCheck {
    let attention = attention(r);
    let check = if attention.is_empty() {
        HealthCheck::new(
            "boot.recovery",
            HealthState::Healthy,
            "Boot recovery completed; nothing needs attention.",
        )
    } else {
        HealthCheck::new(
            "boot.recovery",
            HealthState::Degraded,
            format!("Boot recovery completed: {}.", attention.join("; ")),
        )
    };
    let mut check = check.at(at);
    check.metrics = boot_metrics(r);
    check.recovery = boot_links(r);
    check
}

fn collection_check(pass: Option<&CollectionPass>) -> HealthCheck {
    let Some(pass) = pass else {
        return HealthCheck::new(
            "artifacts.collection",
            HealthState::Unavailable,
            "The boot collection pass has not finished yet.",
        )
        .metric(MetricReading::new(
            "passes",
            Some("count"),
            Metric::Unavailable("no pass has finished".into()),
        ));
    };
    let mut check = match (&pass.blobs, &pass.derived) {
        (Err(e), _) => HealthCheck::new(
            "artifacts.collection",
            HealthState::Failed,
            format!("Collection pass {} failed: {e:?}", pass.pass),
        ),
        (Ok(r), _) if !r.failed.is_empty() => HealthCheck::new(
            "artifacts.collection",
            HealthState::Degraded,
            format!(
                "Collection pass {} could not remove {} files; they stay retired and are retried.",
                pass.pass,
                r.failed.len()
            ),
        ),
        (Ok(_), Err(e)) => HealthCheck::new(
            "artifacts.collection",
            HealthState::Degraded,
            format!(
                "Collection pass {} could not clear derived previews: {e:?}",
                pass.pass
            ),
        ),
        (Ok(_), Ok(_)) => HealthCheck::new(
            "artifacts.collection",
            HealthState::Healthy,
            format!("Collection pass {} completed.", pass.pass),
        ),
    }
    .metric(MetricReading::new(
        "passes",
        Some("count"),
        Metric::Available(pass.pass),
    ));
    match &pass.blobs {
        Ok(r) => {
            check = check
                .metric(count("removed", r.unlinked.len()))
                .metric(count("deferredPinned", r.deferred_pinned.len()))
                .metric(count("deferredReading", r.deferred_reading.len()))
                .metric(count("failed", r.failed.len()));
        }
        Err(e) => {
            for name in ["removed", "deferredPinned", "deferredReading", "failed"] {
                check = check.metric(MetricReading::new(
                    name,
                    Some("count"),
                    Metric::Unavailable(format!("the pass failed: {e:?}")),
                ));
            }
        }
    }
    check.metric(MetricReading::new(
        "derivedRemoved",
        Some("count"),
        match &pass.derived {
            Ok(n) => Metric::Available(*n),
            Err(e) => Metric::Unavailable(format!("{e:?}")),
        },
    ))
}

/// Registers the runtime's health sources on `service`.
pub fn sources(
    service: &DiagnosticService,
    report: BootReport,
    booted_at_ms: i64,
    collections: watch::Receiver<Option<CollectionPass>>,
    dispatcher: Dispatcher,
) {
    service.register_source(Arc::new(move |_now: i64| {
        let pass = collections.borrow().clone();
        vec![
            boot_check(&report, booted_at_ms),
            HealthCheck::new(
                "queue.dispatcher",
                HealthState::Healthy,
                "The delivery dispatcher is running.",
            )
            .metric(count("inFlight", dispatcher.in_flight()))
            .metric(MetricReading::new(
                "passes",
                Some("count"),
                Metric::Available(dispatcher.passes()),
            )),
            collection_check(pass.as_ref()),
        ]
    }));
}

/// The periodic retention and monitor task.
#[derive(Default)]
pub struct Monitor {
    stopping: AtomicBool,
    wake: Notify,
}
impl Monitor {
    /// One pass at once, then every `interval`, until stopped.
    pub async fn run(&self, service: Arc<DiagnosticService>, interval: Duration) {
        while !self.stopping.load(Ordering::SeqCst) {
            let svc = service.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let _ = svc.prune();
                svc.refresh_monitor();
                svc.sweep_exports();
            })
            .await;
            let _ = dispatch::first(self.wake.notified(), tokio::time::sleep(interval)).await;
        }
    }
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }
}
