//! Local application health (OPS-01): one host check per component, distinct from repository
//! health (`analysis.*`) and from model history.
//!
//! | Component | Source | Degraded / failed when |
//! | --- | --- | --- |
//! | `health.readiness` | the essential checks below | any essential check is not healthy (the retained readiness word) |
//! | `storage.migrations` | the migration ledger | the store is read-only for this build, or newer than it |
//! | `storage.schema` | schema objects against a freshly migrated store | an expected table, index, trigger or view is missing |
//! | `storage.wal` | journal mode, database, WAL and free-list sizes | not in WAL mode, or the WAL exceeds [`WAL_DEGRADED_BYTES`] |
//! | `storage.disk` | free space on the data-root volume | below [`DISK_DEGRADED_BYTES`] or 5 %; failed below [`DISK_FAILED_BYTES`] |
//! | `storage.integrity` | the monitor's last integrity and foreign-key check | any problem |
//! | `storage.write` | the monitor's last write-owner probe | the probe failed |
//! | `queue.<consumer>` | queue consumer status and progress | failed (poison) or held deliveries remain |
//! | `process.owned` | owned-process records | a tree of an earlier boot is unsettled, or one is `Uncertain`/`Lost` |
//! | sources | registered [`HealthSource`]s (boot recovery, artifact collection) | as each reports |
//! | `indexing.*` | the latest record of each indexing channel | its run failed, was cancelled, partial, stale or retryable |
//! | `provider` | provider records of the last 24 hours | any provider error |
//! | `diagnostics.retention` | log size, the retention settings and the last pass | the last pass failed |
//! | `access.local` | — | never: a single local user, no sign-in, remote control plane or issued MCP tokens |
//!
//! A metric the host cannot measure is reported `Unavailable` with its reason; a component with
//! nothing to report yet (no indexing run, no provider attempt, no monitor pass) is
//! `Unavailable`, never healthy with zero counts.
use super::{
    DAY_MS, DiagnosticService, audience, channels, chars, label, metric, recovery_action,
    severity_of,
};
use crate::authority::redact::Redactor;
use crate::context::InvocationContext;
use crate::ports::diagnostics::Metric;
use crate::ports::storage::{
    AccessMode, DiagnosticAudience, DiagnosticFilter, ProcessFilter, ProcessPhase, ReadSnapshot,
    StorageError, StoredDiagnostic,
};
use crate::protocol::records::{DiagnosticEntry, DiagnosticOrigin, HealthState, RecoveryAction};
use crate::protocol::support::{BoundedList, BoundedText};
use crate::protocol::{ActorId, Counter, Id};
use crate::services::artifacts::replies::REPLY_CONSUMER;

/// A WAL larger than this means checkpoints are starved (SQLite checkpoints at about 4 MiB).
pub const WAL_DEGRADED_BYTES: u64 = 128 * 1024 * 1024;
/// Free space below this on the data-root volume is degraded.
pub const DISK_DEGRADED_BYTES: u64 = 1024 * 1024 * 1024;
/// Free space below this is failed: writes may stop at any time.
pub const DISK_FAILED_BYTES: u64 = 256 * 1024 * 1024;

/// One measured value of a check or record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetricReading {
    pub name: String,
    pub unit: Option<String>,
    pub value: Metric,
}
impl MetricReading {
    pub fn new(name: &str, unit: Option<&str>, value: Metric) -> Self {
        Self {
            name: name.to_owned(),
            unit: unit.map(str::to_owned),
            value,
        }
    }
    fn count(name: &str, v: i64) -> Self {
        Self::new(name, Some("count"), Metric::Available(v.max(0) as u64))
    }
}

/// A link to a safe recovery action: a catalog operation (and its target) a person or agent may
/// invoke through the gateway under its own authority. The link grants nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryLink {
    pub operation: &'static str,
    pub label: String,
    pub target_kind: Option<String>,
    pub target_id: Option<Id>,
}
impl RecoveryLink {
    pub fn new(operation: &'static str, label: &str) -> Self {
        Self {
            operation,
            label: label.to_owned(),
            target_kind: None,
            target_id: None,
        }
    }
    pub fn on(mut self, kind: &str, id: Id) -> Self {
        self.target_kind = Some(kind.to_owned());
        self.target_id = Some(id);
        self
    }
}

/// One component's health.
#[derive(Clone, Debug)]
pub struct HealthCheck {
    pub component: String,
    pub code: String,
    pub health: HealthState,
    pub message: String,
    pub metrics: Vec<MetricReading>,
    pub recovery: Vec<RecoveryLink>,
    /// When the underlying observation was made (`None`: now).
    pub checked_at_ms: Option<i64>,
}
impl HealthCheck {
    pub fn new(component: &str, health: HealthState, message: impl Into<String>) -> Self {
        let code = match health {
            HealthState::Healthy => "healthy",
            HealthState::Degraded => "degraded",
            HealthState::Failed => "failed",
            HealthState::Unavailable => "unavailable",
        };
        Self {
            component: component.to_owned(),
            code: code.to_owned(),
            health,
            message: message.into(),
            metrics: Vec::new(),
            recovery: Vec::new(),
            checked_at_ms: None,
        }
    }
    pub fn metric(mut self, m: MetricReading) -> Self {
        self.metrics.push(m);
        self
    }
    pub fn link(mut self, l: RecoveryLink) -> Self {
        self.recovery.push(l);
        self
    }
    pub fn at(mut self, ms: i64) -> Self {
        self.checked_at_ms = Some(ms);
        self
    }
}

/// A host component that reports its own health (boot recovery and artifact collection from
/// the runtime; later the indexer helper, the provider router, the knowledge write path).
pub trait HealthSource: Send + Sync {
    fn checks(&self, now_ms: i64) -> Vec<HealthCheck>;
}
impl<F: Fn(i64) -> Vec<HealthCheck> + Send + Sync> HealthSource for F {
    fn checks(&self, now_ms: i64) -> Vec<HealthCheck> {
        self(now_ms)
    }
}

fn rank(h: &HealthState) -> u8 {
    match h {
        HealthState::Healthy => 0,
        HealthState::Unavailable => 1,
        HealthState::Degraded => 2,
        HealthState::Failed => 3,
    }
}
fn same(a: &HealthState, b: &HealthState) -> bool {
    rank(a) == rank(b)
}

fn unavailable(component: &str, why: &str) -> HealthCheck {
    HealthCheck::new(component, HealthState::Unavailable, why)
}
fn storage_error(e: &StorageError) -> String {
    Redactor::new().redact_text(&format!("{e:?}"))
}

fn storage_checks(svc: &DiagnosticService, now: i64, out: &mut Vec<HealthCheck>) {
    let Some(probe) = svc.storage.as_ref() else {
        for c in [
            "storage.migrations",
            "storage.schema",
            "storage.wal",
            "storage.disk",
        ] {
            out.push(unavailable(
                c,
                "No storage probe is attached to this service.",
            ));
        }
        storage_monitor(svc, out);
        return;
    };
    let files = probe.files();
    let database = probe.database();
    match &database {
        Ok(db) => {
            let c = &db.compatibility;
            let applied = c.migrations.len() as i64;
            let (health, message) = match &c.access {
                AccessMode::ReadOnly { reason } => (
                    HealthState::Failed,
                    format!("The store is open read-only: {reason}"),
                ),
                AccessMode::ReadWrite if c.store_schema_version > c.build_schema_version => (
                    HealthState::Degraded,
                    format!(
                        "The store schema ({}) is newer than this build ({}); it stays writable because the newer migrations are additive.",
                        c.store_schema_version, c.build_schema_version
                    ),
                ),
                AccessMode::ReadWrite => (
                    HealthState::Healthy,
                    format!(
                        "Schema version {} with {applied} recorded migrations; checksums match this build.",
                        c.store_schema_version
                    ),
                ),
            };
            out.push(
                HealthCheck::new("storage.migrations", health, message)
                    .metric(MetricReading::count(
                        "storeSchemaVersion",
                        c.store_schema_version,
                    ))
                    .metric(MetricReading::count(
                        "buildSchemaVersion",
                        c.build_schema_version,
                    ))
                    .metric(MetricReading::count("minimumReader", c.minimum_reader))
                    .metric(MetricReading::count("minimumWriter", c.minimum_writer))
                    .metric(MetricReading::count("appliedMigrations", applied)),
            );
            let missing = db.missing_objects.len() as u64;
            let schema = if missing == 0 {
                HealthCheck::new(
                    "storage.schema",
                    HealthState::Healthy,
                    format!(
                        "All {} expected tables, indexes, triggers and views are present.",
                        db.expected_objects
                    ),
                )
            } else {
                let mut names: Vec<&str> = db.missing_objects.iter().map(String::as_str).collect();
                let more = names.len().saturating_sub(20);
                names.truncate(20);
                HealthCheck::new(
                    "storage.schema",
                    HealthState::Degraded,
                    format!(
                        "Missing schema objects: {}{}",
                        names.join(", "),
                        if more > 0 {
                            format!(" and {more} more")
                        } else {
                            String::new()
                        }
                    ),
                )
            };
            out.push(
                schema
                    .metric(MetricReading::new(
                        "expectedObjects",
                        Some("count"),
                        Metric::Available(db.expected_objects),
                    ))
                    .metric(MetricReading::new(
                        "missingObjects",
                        Some("count"),
                        Metric::Available(missing),
                    )),
            );
        }
        Err(e) => {
            let why = format!("The store could not be inspected: {}", storage_error(e));
            out.push(unavailable("storage.migrations", &why));
            out.push(unavailable("storage.schema", &why));
        }
    }
    // Journal and sizes.
    let mut wal = match (&files.wal_bytes, &database) {
        (Metric::Unavailable(why), _) => unavailable(
            "storage.wal",
            &format!("The write-ahead log size is unavailable: {why}"),
        ),
        (_, Ok(db)) if db.journal_mode != "wal" => HealthCheck::new(
            "storage.wal",
            HealthState::Degraded,
            format!("The journal mode is {}, not wal.", db.journal_mode),
        ),
        (Metric::Available(w), _) if *w > WAL_DEGRADED_BYTES => HealthCheck::new(
            "storage.wal",
            HealthState::Degraded,
            format!("The write-ahead log holds {w} bytes: checkpoints are not keeping up."),
        ),
        (Metric::Available(w), _) => HealthCheck::new(
            "storage.wal",
            HealthState::Healthy,
            format!("Write-ahead logging; {w} bytes awaiting checkpoint."),
        ),
    }
    .metric(MetricReading::new(
        "walBytes",
        Some("bytes"),
        files.wal_bytes.clone(),
    ))
    .metric(MetricReading::new(
        "databaseBytes",
        Some("bytes"),
        files.database_bytes.clone(),
    ));
    match &database {
        Ok(db) => {
            wal = wal
                .metric(MetricReading::new(
                    "pageSize",
                    Some("bytes"),
                    Metric::Available(db.page_size),
                ))
                .metric(MetricReading::new(
                    "pages",
                    Some("count"),
                    Metric::Available(db.page_count),
                ))
                .metric(MetricReading::new(
                    "freePages",
                    Some("count"),
                    Metric::Available(db.freelist_pages),
                ));
        }
        Err(e) => {
            let why = storage_error(e);
            for name in ["pageSize", "pages", "freePages"] {
                wal = wal.metric(MetricReading::new(
                    name,
                    None,
                    Metric::Unavailable(why.clone()),
                ));
            }
        }
    }
    out.push(wal);
    let disk = match (&files.free_bytes, &files.total_bytes) {
        (Metric::Unavailable(why), _) => unavailable(
            "storage.disk",
            &format!("Free space on the data-root volume is unavailable: {why}"),
        ),
        (Metric::Available(free), _) if *free < DISK_FAILED_BYTES => HealthCheck::new(
            "storage.disk",
            HealthState::Failed,
            format!("Only {free} bytes are free on the data-root volume; writes may fail."),
        ),
        (Metric::Available(free), total)
            if *free < DISK_DEGRADED_BYTES
                || total.value().is_some_and(|t| free.saturating_mul(20) < t) =>
        {
            HealthCheck::new(
                "storage.disk",
                HealthState::Degraded,
                format!("{free} bytes are free on the data-root volume."),
            )
        }
        (Metric::Available(free), _) => HealthCheck::new(
            "storage.disk",
            HealthState::Healthy,
            format!("{free} bytes are free on the data-root volume."),
        ),
    }
    .metric(MetricReading::new(
        "freeBytes",
        Some("bytes"),
        files.free_bytes,
    ))
    .metric(MetricReading::new(
        "totalBytes",
        Some("bytes"),
        files.total_bytes,
    ));
    out.push(disk);
    let _ = now;
    storage_monitor(svc, out);
}

fn storage_monitor(svc: &DiagnosticService, out: &mut Vec<HealthCheck>) {
    let m = svc.monitor();
    out.push(match &m.integrity {
        None => unavailable(
            "storage.integrity",
            "The integrity check has not run yet in this boot.",
        ),
        Some((at, Ok(problems))) if problems.is_empty() => HealthCheck::new(
            "storage.integrity",
            HealthState::Healthy,
            "The integrity and foreign-key checks found no problems.",
        )
        .metric(MetricReading::count("problems", 0))
        .at(*at),
        Some((at, Ok(problems))) => {
            let shown: Vec<&str> = problems.iter().take(10).map(String::as_str).collect();
            HealthCheck::new(
                "storage.integrity",
                HealthState::Failed,
                format!("Integrity problems: {}", shown.join("; ")),
            )
            .metric(MetricReading::count("problems", problems.len() as i64))
            .link(RecoveryLink::new(
                "workspace.restore",
                "Restore a verified manual backup",
            ))
            .at(*at)
        }
        Some((at, Err(why))) => HealthCheck::new(
            "storage.integrity",
            HealthState::Unavailable,
            format!("The integrity check could not run: {why}"),
        )
        .metric(MetricReading::new(
            "problems",
            Some("count"),
            Metric::Unavailable(why.clone()),
        ))
        .at(*at),
    });
    out.push(match &m.write_probe {
        None => unavailable(
            "storage.write",
            "The write-owner probe has not run yet in this boot.",
        ),
        Some((at, Ok(ms))) => HealthCheck::new(
            "storage.write",
            HealthState::Healthy,
            "The storage write owner committed a probe unit of work.",
        )
        .metric(MetricReading::new(
            "latency",
            Some("ms"),
            Metric::Available(*ms),
        ))
        .at(*at),
        Some((at, Err(why))) => HealthCheck::new(
            "storage.write",
            HealthState::Failed,
            format!("The storage write owner refused a probe: {why}"),
        )
        .metric(MetricReading::new(
            "latency",
            Some("ms"),
            Metric::Unavailable(why.clone()),
        ))
        .at(*at),
    });
}

fn queue_checks(svc: &DiagnosticService, snapshot: &dyn ReadSnapshot, out: &mut Vec<HealthCheck>) {
    let consumers = svc
        .consumers
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let Some(mut consumers) = consumers else {
        out.push(unavailable(
            "queue",
            "No delivery dispatcher is attached, so its consumers are unknown.",
        ));
        return;
    };
    if !consumers.iter().any(|c| c == REPLY_CONSUMER) {
        consumers.insert(0, REPLY_CONSUMER.to_owned());
    }
    for consumer in consumers {
        let component = format!("queue.{consumer}");
        let status = snapshot.queue().status(&consumer);
        let progress = snapshot.queue().progress(&consumer);
        let (status, progress) = match (status, progress) {
            (Ok(s), Ok(p)) => (s, p),
            (Err(e), _) | (_, Err(e)) => {
                out.push(unavailable(
                    &component,
                    &format!("Queue status is unavailable: {}", storage_error(&e)),
                ));
                continue;
            }
        };
        let id = Id::new(consumer.as_str()).ok();
        let mut check = if status.failed > 0 || status.reconciling > 0 {
            HealthCheck::new(
                &component,
                HealthState::Degraded,
                format!(
                    "{} failed and {} held deliveries need an operator decision.",
                    status.failed, status.reconciling
                ),
            )
        } else {
            HealthCheck::new(
                &component,
                HealthState::Healthy,
                format!(
                    "{} ready and {} running deliveries; nothing failed or held.",
                    status.ready, status.claimed
                ),
            )
        };
        check = check
            .metric(MetricReading::count("ready", status.ready))
            .metric(MetricReading::count("running", status.claimed))
            .metric(MetricReading::count("held", status.reconciling))
            .metric(MetricReading::count("failed", status.failed))
            .metric(MetricReading::count("completed", status.completed))
            .metric(MetricReading::count("watermark", progress.watermark));
        if let Some(t) = status.oldest_ready_at_ms {
            check = check.metric(MetricReading::new(
                "oldestReadyAgeMs",
                Some("ms"),
                Metric::Available(svc.now().saturating_sub(t).max(0) as u64),
            ));
        }
        if let Some(id) = id {
            if status.failed > 0 {
                check = check
                    .link(
                        RecoveryLink::new("queue.inspect", "Inspect the failed deliveries")
                            .on("queue_consumer", id.clone()),
                    )
                    .link(RecoveryLink::new(
                        "queue.retry",
                        "Retry a failed delivery with a fresh budget",
                    ))
                    .link(RecoveryLink::new(
                        "queue.resolve",
                        "Dismiss a delivery that can never succeed",
                    ));
            }
            if status.reconciling > 0 {
                check = check
                    .link(
                        RecoveryLink::new("queue.inspect", "Inspect the held deliveries")
                            .on("queue_consumer", id),
                    )
                    .link(RecoveryLink::new(
                        "queue.reconcile",
                        "Reconcile a held delivery once its outcome is known",
                    ));
            }
        }
        out.push(check);
    }
}

fn process_checks(
    svc: &DiagnosticService,
    snapshot: &dyn ReadSnapshot,
    audience: &DiagnosticAudience,
    out: &mut Vec<HealthCheck>,
) {
    let boot = svc.store.info().boot_id;
    let processes = match snapshot
        .resources()
        .processes(&ProcessFilter::Unsettled, None, 500)
    {
        Ok(p) => p,
        Err(e) => {
            out.push(unavailable(
                "process.owned",
                &format!(
                    "Owned-process records are unavailable: {}",
                    storage_error(&e)
                ),
            ));
            return;
        }
    };
    let bound = processes.len() == 500;
    let visible: Vec<_> = processes
        .into_iter()
        .filter(|p| match audience {
            DiagnosticAudience::All => true,
            DiagnosticAudience::Project(project) => {
                p.project_id.is_none() || p.project_id.as_ref() == Some(project)
            }
        })
        .collect();
    let orphaned: Vec<_> = visible
        .iter()
        .filter(|p| boot.as_ref() != Some(&p.boot_id))
        .collect();
    let uncertain = visible
        .iter()
        .filter(|p| p.phase == ProcessPhase::Uncertain)
        .count() as i64;
    let lost = visible
        .iter()
        .filter(|p| p.phase == ProcessPhase::Lost)
        .count() as i64;
    let running = visible.len() as i64 - orphaned.len() as i64;
    let at_least = if bound { "at least " } else { "" };
    let mut check = if orphaned.is_empty() && uncertain == 0 && lost == 0 {
        HealthCheck::new(
            "process.owned",
            HealthState::Healthy,
            format!("{at_least}{running} owned process trees of this boot; none orphaned."),
        )
    } else {
        HealthCheck::new(
            "process.owned",
            HealthState::Degraded,
            format!(
                "{at_least}{} process trees of earlier boots are unsettled, {uncertain} could not be verified stopped and {lost} launches recorded no identity; only a person can confirm an unverifiable tree stopped.",
                orphaned.len()
            ),
        )
    };
    check = check
        .metric(MetricReading::count("running", running))
        .metric(MetricReading::count("orphaned", orphaned.len() as i64))
        .metric(MetricReading::count("uncertain", uncertain))
        .metric(MetricReading::count("lost", lost));
    for p in visible
        .iter()
        .filter(|p| {
            boot.as_ref() != Some(&p.boot_id)
                || matches!(p.phase, ProcessPhase::Uncertain | ProcessPhase::Lost)
        })
        .take(8)
    {
        check = check.link(
            RecoveryLink::new("process.cancel", &format!("Stop {}", p.program))
                .on("owned_process", p.id.clone()),
        );
    }
    out.push(check);
}

fn latest(
    snapshot: &dyn ReadSnapshot,
    audience: &DiagnosticAudience,
    component: &str,
) -> Result<Option<StoredDiagnostic>, StorageError> {
    Ok(snapshot
        .diagnostics()
        .diagnostics(
            &DiagnosticFilter {
                component: Some(component.to_owned()),
                descending: true,
                audience: audience.clone(),
                ..DiagnosticFilter::default()
            },
            1,
        )?
        .into_iter()
        .next())
}
fn count(
    snapshot: &dyn ReadSnapshot,
    audience: &DiagnosticAudience,
    component: &str,
    min_severity: Option<u8>,
    since_ms: i64,
) -> Result<i64, StorageError> {
    snapshot.diagnostics().count_diagnostics(&DiagnosticFilter {
        component: Some(component.to_owned()),
        min_severity,
        since_ms: Some(since_ms),
        audience: audience.clone(),
        ..DiagnosticFilter::default()
    })
}

fn indexing_checks(
    snapshot: &dyn ReadSnapshot,
    audience: &DiagnosticAudience,
    now: i64,
    out: &mut Vec<HealthCheck>,
) {
    for channel in channels::INDEXING {
        let found = latest(snapshot, audience, channel).and_then(|l| {
            Ok((
                l,
                count(snapshot, audience, channel, Some(2), now - DAY_MS)?,
            ))
        });
        let (latest, failures) = match found {
            Ok(v) => v,
            Err(e) => {
                out.push(unavailable(
                    channel,
                    &format!("The indexing log is unavailable: {}", storage_error(&e)),
                ));
                continue;
            }
        };
        let Some(d) = latest else {
            out.push(
                unavailable(channel, "No run has been recorded on this channel yet.").metric(
                    MetricReading::new(
                        "failuresLast24h",
                        Some("count"),
                        Metric::Unavailable("no run has been recorded".into()),
                    ),
                ),
            );
            continue;
        };
        let health = match d.outcome.as_deref() {
            Some("Succeeded") | Some("Running") | None if d.severity != "Error" => {
                HealthState::Healthy
            }
            Some("Failed") => HealthState::Failed,
            Some(_) => HealthState::Degraded,
            None => HealthState::Failed,
        };
        let what = [
            d.operation.as_deref(),
            d.target_id.as_ref().map(Id::as_str),
            d.stage.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" / ");
        let mut check = HealthCheck::new(
            channel,
            health,
            format!(
                "Latest {}: {}{}",
                d.outcome.as_deref().unwrap_or("record").to_lowercase(),
                if what.is_empty() {
                    String::new()
                } else {
                    format!("{what}: ")
                },
                d.message
            ),
        )
        .metric(MetricReading::count("failuresLast24h", failures))
        .at(d.recorded_at_ms);
        if let Some(a) = d.attempt {
            check = check.metric(MetricReading::count("attempt", a));
        }
        if let (Some(c), Some(t)) = (d.progress_completed, d.progress_total) {
            check = check
                .metric(MetricReading::count("completed", c))
                .metric(MetricReading::count("total", t));
        }
        if let Ok(links) = serde_json::from_str::<Vec<RecoveryAction>>(&d.recovery_json) {
            for l in links {
                if let Some(op) = crate::registry::find(&l.operation.0) {
                    check.recovery.push(RecoveryLink {
                        operation: op.name,
                        label: l.label.0,
                        target_kind: l.target_kind.map(|k| k.0),
                        target_id: l.target_id,
                    });
                }
            }
        }
        out.push(check);
    }
}

fn provider_check(
    snapshot: &dyn ReadSnapshot,
    audience: &DiagnosticAudience,
    now: i64,
    out: &mut Vec<HealthCheck>,
) {
    let since = now - DAY_MS;
    let counts = count(snapshot, audience, channels::PROVIDER, None, since).and_then(|total| {
        Ok((
            total,
            count(snapshot, audience, channels::PROVIDER, Some(2), since)?,
        ))
    });
    out.push(match counts {
        Err(e) => unavailable(
            channels::PROVIDER,
            &format!("The provider log is unavailable: {}", storage_error(&e)),
        ),
        Ok((0, _)) => unavailable(
            channels::PROVIDER,
            "No provider attempt has been recorded in the last 24 hours.",
        )
        .metric(MetricReading::new(
            "failuresLast24h",
            Some("count"),
            Metric::Unavailable("no provider attempt has been recorded".into()),
        )),
        Ok((total, errors)) => HealthCheck::new(
            channels::PROVIDER,
            if errors > 0 {
                HealthState::Degraded
            } else {
                HealthState::Healthy
            },
            format!("{errors} of {total} provider records in the last 24 hours are errors."),
        )
        .metric(MetricReading::count("recordsLast24h", total))
        .metric(MetricReading::count("failuresLast24h", errors))
        .link(RecoveryLink::new(
            "router.connections",
            "Check provider connections and credentials",
        ))
        .link(RecoveryLink::new(
            "router.availability",
            "Check model availability and cooldowns",
        )),
    });
}

fn retention_check(
    svc: &DiagnosticService,
    snapshot: &dyn ReadSnapshot,
    now: i64,
    out: &mut Vec<HealthCheck>,
) {
    let stats = snapshot.diagnostics().diagnostic_stats();
    let settings = super::retention(snapshot.configuration());
    let (stats, settings) = match (stats, settings) {
        (Ok(s), Ok(r)) => (s, r),
        (Err(e), _) | (_, Err(e)) => {
            out.push(unavailable(
                channels::RETENTION,
                &format!(
                    "The log or its settings are unavailable: {}",
                    storage_error(&e)
                ),
            ));
            return;
        }
    };
    let last = svc.monitor().retention;
    let health = match &last {
        Some((_, Err(_))) => HealthState::Failed,
        _ => HealthState::Healthy,
    };
    let mut check = HealthCheck::new(
        channels::RETENTION,
        health,
        format!(
            "{} records retained for up to {} days and at most {} records.",
            stats.records, settings.days, settings.max_records
        ),
    )
    .metric(MetricReading::count("records", stats.records))
    .metric(MetricReading::count(
        "latestSequence",
        stats.latest_sequence,
    ))
    .metric(MetricReading::count("retentionDays", settings.days))
    .metric(MetricReading::count("maxRecords", settings.max_records));
    if let Some(oldest) = stats.oldest_recorded_at_ms {
        check = check.metric(MetricReading::new(
            "oldestAgeMs",
            Some("ms"),
            Metric::Available(now.saturating_sub(oldest).max(0) as u64),
        ));
    }
    check = check.metric(MetricReading::new(
        "lastPassAgeMs",
        Some("ms"),
        match &last {
            Some((at, _)) => Metric::Available(now.saturating_sub(*at).max(0) as u64),
            None => Metric::Unavailable("no retention pass has run in this boot".into()),
        },
    ));
    if let Some((_, Err(why))) = &last {
        check.message = format!("The last retention pass failed: {why}");
    }
    out.push(check);
}

fn access_check(ctx: &InvocationContext, out: &mut Vec<HealthCheck>) {
    out.push(HealthCheck::new(
        "access.local",
        HealthState::Healthy,
        format!(
            "Single-user local workspace: no sign-in, no central account or remote control plane, and no issued MCP server tokens. Agents, packages and MCP clients act through local authority bindings. This request is {} ({:?}).",
            ctx.principal().0.as_str(),
            ctx.principal_kind()
        ),
    ));
}

/// Components whose health decides readiness.
fn essential(component: &str) -> bool {
    matches!(
        component,
        "storage.migrations" | "storage.write" | "storage.integrity" | "storage.disk"
    ) || super::under(component, "queue")
        || super::under(component, "boot")
        || component == "process.owned"
}

/// Every check `ctx` may see, readiness first.
pub(crate) fn checks(
    svc: &DiagnosticService,
    ctx: &InvocationContext,
    snapshot: &dyn ReadSnapshot,
    now: i64,
) -> Vec<HealthCheck> {
    let audience = match audience(snapshot.authority(), ctx) {
        Ok(Some(a)) => a,
        // The handler refuses an unbound principal before this; a native caller sees nothing.
        Ok(None) => return Vec::new(),
        Err(e) => {
            return vec![unavailable(
                "health.readiness",
                &format!("Authority is unavailable: {}", storage_error(&e)),
            )];
        }
    };
    let mut out = Vec::new();
    storage_checks(svc, now, &mut out);
    queue_checks(svc, snapshot, &mut out);
    process_checks(svc, snapshot, &audience, &mut out);
    for source in svc.sources.read().unwrap_or_else(|p| p.into_inner()).iter() {
        out.extend(source.checks(now));
    }
    indexing_checks(snapshot, &audience, now, &mut out);
    provider_check(snapshot, &audience, now, &mut out);
    retention_check(svc, snapshot, now, &mut out);
    access_check(ctx, &mut out);
    let worst = out
        .iter()
        .filter(|c| essential(&c.component))
        .map(|c| match c.health {
            HealthState::Unavailable => HealthState::Degraded,
            ref h => h.clone(),
        })
        .max_by_key(rank)
        .unwrap_or(HealthState::Healthy);
    let attention: Vec<&str> = out
        .iter()
        .filter(|c| essential(&c.component) && !same(&c.health, &HealthState::Healthy))
        .map(|c| c.component.as_str())
        .collect();
    let word = match worst {
        HealthState::Healthy => "Healthy",
        HealthState::Failed => "Unhealthy",
        _ => "Degraded",
    };
    let readiness = HealthCheck::new(
        "health.readiness",
        worst,
        if attention.is_empty() {
            word.to_owned()
        } else {
            format!("{word}: {} need attention.", attention.join(", "))
        },
    )
    .metric(MetricReading::count("checks", out.len() as i64))
    .metric(MetricReading::count(
        "needAttention",
        attention.len() as i64,
    ));
    out.insert(0, readiness);
    out
}

fn health_id(component: &str) -> Id {
    let mut s: String = format!("health-{component}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    s.truncate(128);
    Id::new(s).unwrap_or_else(|_| Id::new("health").expect("valid"))
}

/// The wire record of a check (origin `Host`, attributed to the host).
pub fn wire(c: &HealthCheck, host: &ActorId, now: i64) -> Result<DiagnosticEntry, StorageError> {
    let redactor = Redactor::new();
    let at = c.checked_at_ms.unwrap_or(now).max(0);
    Ok(DiagnosticEntry {
        id: health_id(&c.component),
        severity: severity_of(&c.health),
        component: label(&c.component),
        code: label(&c.code),
        message: BoundedText(chars(&redactor.redact_text(&c.message), 65_536)),
        correlation_id: None,
        evidence: None,
        sequence: None,
        recorded_at_ms: Some(Counter::new(at).map_err(|e| StorageError::InvalidRecord(e.into()))?),
        actor_id: Some(host.clone()),
        origin: Some(DiagnosticOrigin::Host),
        project_id: None,
        operation: None,
        operation_id: None,
        target_kind: None,
        target_id: None,
        stage: None,
        source_revision: None,
        attempt: None,
        started_at_ms: None,
        finished_at_ms: None,
        progress_completed: None,
        progress_total: None,
        outcome: None,
        error_class: None,
        health: Some(c.health.clone()),
        output: None,
        output_omitted_bytes: None,
        redactions: None,
        metrics: (!c.metrics.is_empty())
            .then(|| BoundedList(c.metrics.iter().take(128).map(metric).collect())),
        recovery: {
            let links: Vec<RecoveryAction> = c
                .recovery
                .iter()
                .filter_map(recovery_action)
                .take(128)
                .collect();
            (!links.is_empty()).then_some(BoundedList(links))
        },
    })
}
