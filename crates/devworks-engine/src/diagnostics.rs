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
use devworks_core::ports::files::FileFault;
use devworks_core::ports::storage::StorageError;
use devworks_core::protocol::OperationState;
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
        count("activationsInterrupted", r.runs.interrupted.len()),
        count("rootsHeldForContinue", r.runs.held.len()),
        count("stopsUnresolved", unresolved_stops(r)),
    ]
}

/// Journaled stops of earlier boots that recovery could not settle: an owned tree was not
/// verified stopped, so their roots stay held for reconciliation.
fn unresolved_stops(r: &BootReport) -> usize {
    r.runs
        .stops
        .iter()
        .filter(|(_, state)| matches!(state, OperationState::OutcomeUnknown))
        .count()
}

/// What recovery established that a person should know without it being a fault: roots held for
/// an explicit Continue are the designed restart behaviour, not degraded health.
fn notes(r: &BootReport) -> Vec<String> {
    let mut out = Vec::new();
    if !r.runs.held.is_empty() {
        out.push(format!(
            "{} feature or issue roots interrupted by the restart are held for an explicit Continue",
            r.runs.held.len()
        ));
    }
    out
}

/// Up to five digests, then how many more.
fn digests(all: &[String]) -> String {
    let shown: Vec<&str> = all.iter().take(5).map(String::as_str).collect();
    match all.len().saturating_sub(5) {
        0 => shown.join(", "),
        more => format!("{} and {more} more", shown.join(", ")),
    }
}

fn with_notes(message: String, r: &BootReport) -> String {
    let notes = notes(r);
    if notes.is_empty() {
        message
    } else {
        format!("{message} {}.", notes.join("; "))
    }
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
            "{} managed files are missing ({})",
            r.artifacts.missing.len(),
            digests(&r.artifacts.missing)
        ));
    }
    if !r.artifacts.quarantined.is_empty() {
        out.push(format!(
            "{} corrupt managed files were quarantined; their artifact versions cannot be read until restored ({})",
            r.artifacts.quarantined.len(),
            digests(&r.artifacts.quarantined)
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
    let unresolved = unresolved_stops(r);
    if unresolved > 0 {
        out.push(format!(
            "{unresolved} stops of earlier boots could not be verified complete; their roots are held for reconciliation"
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
    let a = &r.artifacts;
    if !a.quarantined.is_empty() || !a.missing.is_empty() {
        links.push(RecoveryLink::new(
            "workspace.restore",
            "Restore a verified manual backup holding the corrupt or missing files",
        ));
        links.push(RecoveryLink::new(
            "artifacts.list",
            "Find the artifacts whose files are affected",
        ));
    }
    if !r.runs.held.is_empty() || unresolved_stops(r) > 0 {
        links.push(RecoveryLink::new(
            "runs.inspect",
            "Inspect the roots held by recovery",
        ));
        links.push(RecoveryLink::new(
            "runs.continue",
            "Continue a held root once its work is understood",
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
    let message = with_notes(message, report);
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
        BootError::Runs(e) => ("runs", e),
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
        output: Some(cause.to_string()),
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
    check.message = with_notes(check.message, r);
    check.metrics = boot_metrics(r);
    check.recovery = boot_links(r);
    check
}

/// A file fault as text (never Debug: its escaping would hide a secret from redaction).
fn fault_text(f: &FileFault) -> String {
    match f {
        FileFault::NotFound => "not found".into(),
        FileFault::Corrupt(why) => format!("corrupt: {why}"),
        FileFault::Mismatch => "length or digest mismatch".into(),
        FileFault::Source(why) => format!("source unreadable: {why}"),
        FileFault::DiskFull => "disk full".into(),
        FileFault::Exists => "destination exists".into(),
        FileFault::Io(why) => format!("I/O: {why}"),
        FileFault::Unsupported(why) => format!("unsupported: {why}"),
    }
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
            format!("Collection pass {} failed: {e}", pass.pass),
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
                "Collection pass {} could not clear derived previews: {}",
                pass.pass,
                fault_text(e)
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
                    Metric::Unavailable(format!("the pass failed: {e}")),
                ));
            }
        }
    }
    check.metric(MetricReading::new(
        "derivedRemoved",
        Some("count"),
        match &pass.derived {
            Ok(n) => Metric::Available(*n),
            Err(e) => Metric::Unavailable(fault_text(e)),
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
    /// One pass at once, then every `interval`, until stopped. Once stopped, a pass in progress
    /// starts no further step (retention, each probe, sweep), so none writes after it was
    /// signalled and shutdown joins it as soon as the step under way returns.
    pub async fn run(self: Arc<Self>, service: Arc<DiagnosticService>, interval: Duration) {
        while !self.stopping.load(Ordering::SeqCst) {
            let (svc, me) = (service.clone(), self.clone());
            let _ = tokio::task::spawn_blocking(move || {
                let stopped = || me.stopping.load(Ordering::SeqCst);
                if !stopped() {
                    let _ = svc.prune();
                }
                if !stopped() {
                    svc.refresh_monitor_unless(&stopped);
                }
                if !stopped() {
                    svc.sweep_exports();
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use devworks_core::protocol::{Id, OperationId, RootId};
    use devworks_core::services::runs::recovery::BootRecovery;

    fn report(runs: BootRecovery) -> BootReport {
        BootReport {
            queue: Default::default(),
            artifacts: Default::default(),
            resources: Default::default(),
            runs,
            keep_awake: KeepAwakeStatus::Off,
        }
    }

    #[test]
    fn run_recovery_is_reported_and_unresolved_stops_need_attention() {
        let root = RootId(Id::new("root-a").unwrap());
        // Roots held for Continue are named, but they are the designed restart behaviour.
        let held = report(BootRecovery {
            held: vec![root.clone()],
            ..Default::default()
        });
        let check = boot_check(&held, 1);
        assert!(matches!(check.health, HealthState::Healthy));
        assert!(
            check.message.contains("held for an explicit Continue"),
            "{}",
            check.message
        );
        assert!(
            check
                .recovery
                .iter()
                .any(|l| l.operation == "runs.continue")
        );
        // A stop whose trees were not verified stopped holds its root for reconciliation.
        let unresolved = report(BootRecovery {
            held: vec![root],
            stops: vec![(
                OperationId(Id::new("op-stop").unwrap()),
                OperationState::OutcomeUnknown,
            )],
            ..Default::default()
        });
        let check = boot_check(&unresolved, 1);
        assert!(matches!(check.health, HealthState::Degraded));
        assert!(
            check.message.contains("held for reconciliation"),
            "{}",
            check.message
        );
    }
}
