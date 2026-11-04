//! `WorkspaceLifecycle` diagnostics: `workspace.diagnostics`, `workspace.logs` and
//! `workspace.record_diagnostic` (CAP-DIAGNOSTICS; LOG-01..03, OPS-01/02).
//!
//! One [`DiagnosticService`] serves every entry point. The native UI and agent tools submit to
//! the [`crate::gateway::Gateway`] with it registered in [`super::UseCases`]; host work (boot,
//! indexing, provider attempts, retention) writes through [`DiagnosticService::log`] and the
//! [`RunLog`] channel on the same records, with the same redaction and bounds.
//!
//! # What is kept, and what is not
//!
//! * **Distinct from model history.** Diagnostics live in their own retained log, never in
//!   history events, transcripts or provider request history; recording one appends no history
//!   event (the command still has its receipt).
//! * **Attributed.** The host sets the actor, the invocation correlation, the project of the
//!   invocation and the origin (`Host` for host work, `Reported` for everything else). A
//!   reporter cannot write a host-reserved component ([`RESERVED`]), so a package, agent, MCP
//!   client or the UI cannot impersonate host health. Components are canonical
//!   ([`canonical_component`]) and matched exactly and case-sensitively, by the service and the
//!   store alike, so no other spelling passes for a host channel.
//! * **Redacted and bounded before persistence.** Every free-text member passes the authority
//!   [`Redactor`] (known secret values the host registered, credential headers and
//!   credential-shaped tokens) and the record keeps the number of replacements. Messages keep
//!   [`MESSAGE_LIMIT`] bytes and helper output its last [`OUTPUT_LIMIT`] bytes; what was cut is
//!   counted, never silently dropped. Output of components that read user databases
//!   ([`ROW_SENSITIVE`]) is never retained, because it may contain source rows: only its size
//!   is. Exports are redacted again. Redaction cannot prove arbitrary text holds no secret.
//! * **Outcomes are distinguishable (LOG-03).** A record carries `Running`, `Succeeded`,
//!   `Partial`, `Stale`, `Cancelled`, `Retryable` or `Failed` (terminal), an error class, the
//!   attempt, lifecycle times and progress.
//! * **Retained across restarts** under the `diagnostics.logRetentionDays` (default 30) and
//!   `diagnostics.logMaxRecords` (default 50 000) settings, applied by [`DiagnosticService::prune`]
//!   at boot and hourly. Evidence artifacts a record names keep their own retention.
//! * **Health (OPS-01).** `workspace.diagnostics` reports one host check per component (module
//!   [`health`]). A metric the host cannot measure is `Unavailable` with its reason, never zero.
//! * **Local only (OPS-02).** No central account, remote control plane, telemetry or upload:
//!   an export is an artifact a person saves locally ([`DiagnosticService::export`]); a retry of
//!   its request key resubmits the recorded command, which the gateway replays.
//!
//! # Retained CodeGraph behavior
//!
//! * `AdminLogsController.List` (`GET api/admin/logs`) maps to `workspace.logs`: its container,
//!   level, time window and bounded search (at most 256 characters) become `component` (dotted
//!   prefix), the minimum `severity`, `sinceMs`/`untilMs` (start after end is
//!   `InvalidArgument`, the source's 400) and `search`; newest first is `order: Descending`. The
//!   source's page numbers and total pages are replaced by the sequence cursor and
//!   `afterSequence`, which also serves live tailing. Its trace and exception columns are the
//!   correlation ID and the bounded helper output.
//! * `ClientErrorsController.Report` (`POST api/client-errors`) maps to
//!   `workspace.record_diagnostic` from the UI principal (component `client`): the message is
//!   required and bounded (the source refused more than 4 096 characters; here the contract
//!   allows 64 KiB and the host keeps 8 KiB, counting the rest), the stack becomes the bounded
//!   output, and the record is persisted instead of only written to a process log.
//! * `HealthController.Get`, the local readiness endpoint and the indexer sidecar and
//!   memory-write health probes are replaced by the `health.readiness`, `indexing.*` and
//!   `storage.write` checks of `workspace.diagnostics`: there is no HTTP host, loopback probe or
//!   sidecar container, and the write probe runs on the one storage write owner.
//! * `SettingsController.GetDatabaseHealth` maps to the `storage.*` checks: the source's
//!   expected constraints and indexes become the schema objects of a freshly migrated store
//!   (`storage.schema`, degraded when one is missing, as the source's `warning`), plus the
//!   migration ledger, journal, sizes and the integrity check. Its duplicate-row groups are
//!   enforced by the store's unique constraints instead, whose corruption the integrity check
//!   reports.
//! * `AuthConfigController` and `UserMcpTokensController` are replaced by the `access.local`
//!   check: a single local user with no sign-in, no remote control plane and no issued MCP
//!   server tokens; who is asking is the invocation's principal.
pub mod health;

use crate::authority::redact::{REDACTED, Redactor};
use crate::context::{InvocationContext, PrincipalKind, ReceiptIdentity};
use crate::fingerprint::sha256_hex;
use crate::gateway::{AdmittedCommand, AdmittedQuery, CommandOutcome};
use crate::operations::OperationInput;
use crate::operations::workspace::{Input, LogsInput, RecordDiagnosticInput};
use crate::ports::diagnostics::{
    ExportRefused, ExportStaging, Metric, StorageDatabase, StorageHealthProbe,
};
use crate::ports::storage::{
    ApplicationStore, ArtifactState, AuthorityRead, ConfigScope, ConfigurationRead,
    DiagnosticAudience, DiagnosticFilter, ExportIntent, PrunedDiagnostics, ReadOptions,
    ReadSnapshot, StorageError, StoredDiagnostic, Unavailable, WriteOptions, WriteTransaction,
    read_snapshot, run_write, severity_rank,
};
use crate::ports::{
    Clock, CommandHandler, Decided, HandlerFailure, IdKind, IdSource, QueryAnswer, QueryHandler,
};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{
    DiagnosticEntry, DiagnosticErrorClass, DiagnosticMetric, DiagnosticOrigin, DiagnosticOutcome,
    DiagnosticPage, HealthState, MetricAvailability, RecoveryAction, Severity,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText};
use crate::protocol::{
    ActorId, CommandReply, CorrelationId, Counter, DomainErrorCode, DomainFault, Id, OperationId,
    ProjectId, Protocol, Rejection, RequestKey, RetryAdvice, Revision,
};
use crate::registry;
use crate::services::artifacts::{Viewer, visible};
use crate::settings::catalog::{LOG_MAX_RECORDS_KEY, LOG_RETENTION_DAYS_KEY, descriptor};
use crate::settings::model::{chain, current_chain};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

pub use health::{HealthCheck, HealthSource, MetricReading, RecoveryLink};

/// The operations this service implements.
pub const OPERATIONS: [&str; 3] = [
    "workspace.diagnostics",
    "workspace.logs",
    "workspace.record_diagnostic",
];

/// Log channels host work writes to.
pub mod channels {
    /// Repository indexing runs (LOG-01).
    pub const REPOSITORY_INDEXING: &str = "indexing.repository";
    /// Database-schema indexing runs; helper output is never retained (it may hold rows).
    pub const SCHEMA_INDEXING: &str = "indexing.database_schema";
    /// Semantic (embedding) indexing runs.
    pub const SEMANTIC_INDEXING: &str = "indexing.semantic";
    /// Every indexing channel, in the order health reports them.
    pub const INDEXING: [&str; 3] = [REPOSITORY_INDEXING, SCHEMA_INDEXING, SEMANTIC_INDEXING];
    /// Provider attempts and failures.
    pub const PROVIDER: &str = "provider";
    /// Boot recovery.
    pub const BOOT: &str = "boot";
    /// Retention passes.
    pub const RETENTION: &str = "diagnostics.retention";
    /// UI-reported errors (the retained client error report).
    pub const CLIENT: &str = "client";
}

/// Component prefixes only the host writes: a reported diagnostic naming one of them is refused
/// (`PermissionDenied`, target kind [`RESERVED_TARGET`]).
pub const RESERVED: [&str; 12] = [
    "host",
    "health",
    "storage",
    "queue",
    "process",
    "boot",
    "access",
    "diagnostics",
    "artifacts",
    "indexing",
    "provider",
    "packages",
];
/// Fault target kind of a reported diagnostic naming a host-reserved component.
pub const RESERVED_TARGET: &str = "diagnostic_component";
/// Components whose helper output may contain rows of a user's database: the output is never
/// retained, only its size.
pub const ROW_SENSITIVE: [&str; 2] = ["indexing.database_schema", "database"];
/// Bytes of a message kept.
pub const MESSAGE_LIMIT: usize = 8 * 1024;
/// Bytes of helper output kept (its tail).
pub const OUTPUT_LIMIT: usize = 16 * 1024;
/// Most records one export carries.
pub const EXPORT_MAX_RECORDS: usize = 5_000;
/// Most bytes of one export document, as written (pretty-printed, with health and envelope).
pub const EXPORT_MAX_BYTES: usize = 8 * 1024 * 1024;
/// How long an export waits for another attempt's claim on its request key.
pub const EXPORT_CLAIM_WAIT: Duration = Duration::from_secs(30);
/// Interval at which a waiting export re-reads the claim.
const EXPORT_CLAIM_POLL: Duration = Duration::from_millis(10);
/// Store-clock age after which a claim that never recorded its command is taken over.
pub const EXPORT_CLAIM_STALE_MS: i64 = 5 * 60 * 1000;
/// Format identifier of an export document.
pub const EXPORT_FORMAT: &str = "devworks.diagnostics-export/1";
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const CURSOR_VERSION: &str = "l2";

/// `component` is `prefix` or lies beneath it (`indexing.repository` under `indexing`).
pub fn under(component: &str, prefix: &str) -> bool {
    component == prefix
        || (component.len() > prefix.len()
            && component.starts_with(prefix)
            && component.as_bytes()[prefix.len()] == b'.')
}
fn reserved(component: &str) -> bool {
    RESERVED.iter().any(|p| under(component, p))
}
fn row_sensitive(component: &str) -> bool {
    ROW_SENSITIVE.iter().any(|p| under(component, p))
}

/// The wire name of an enumerated value (`Info`, `Failed`, ...).
pub fn wire_name<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}
fn parse_wire<T: DeserializeOwned>(s: &str) -> Result<T, StorageError> {
    serde_json::from_value(Value::String(s.to_owned()))
        .map_err(|_| StorageError::unavailable(Unavailable::Corrupt, format!("stored value {s}")))
}

// ---------------------------------------------------------------------------------------------
// Bounds and redaction

/// `text` cut to at most `max` bytes on a character boundary, keeping its start.
fn head(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
/// `text` cut to at most `max` bytes on a character boundary, keeping its end.
fn tail(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}
/// At most `max` characters (wire `Label`/`Text` bounds count characters).
fn chars(text: &str, max: usize) -> String {
    text.chars().filter(|c| *c != '\0').take(max).collect()
}

/// Redaction of one record: every text member through one [`Redactor`], counting replacements.
struct Scrub<'a> {
    redactor: &'a Redactor,
    count: i64,
}
impl Scrub<'_> {
    fn text(&mut self, text: &str) -> String {
        let out = self.redactor.redact_text(text);
        let before = text.matches(REDACTED).count() as i64;
        self.count += (out.matches(REDACTED).count() as i64 - before).max(0);
        out
    }
    /// A short member (`Label`): redacted, at most 256 characters, absent when empty.
    fn label(&mut self, text: Option<&str>) -> Option<String> {
        let t = chars(&self.text(text?), 256);
        (!t.trim().is_empty()).then_some(t)
    }
}

// ---------------------------------------------------------------------------------------------
// Native entries

/// One diagnostic host work writes (see [`DiagnosticService::log`]).
#[derive(Clone, Debug)]
pub struct LogEntry {
    pub component: String,
    pub code: String,
    pub severity: Severity,
    pub message: String,
    pub correlation_id: Option<Id>,
    pub project_id: Option<ProjectId>,
    pub operation: Option<String>,
    pub operation_id: Option<OperationId>,
    pub target_kind: Option<String>,
    pub target_id: Option<Id>,
    pub stage: Option<String>,
    pub source_revision: Option<String>,
    pub attempt: Option<i64>,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    /// Completed and total units.
    pub progress: Option<(i64, i64)>,
    pub outcome: Option<DiagnosticOutcome>,
    pub error_class: Option<DiagnosticErrorClass>,
    /// Helper output (stdout/stderr); bounded and redacted before persistence.
    pub output: Option<String>,
    pub evidence: Option<ArtifactRef>,
    pub metrics: Vec<MetricReading>,
    pub recovery: Vec<RecoveryLink>,
}
impl LogEntry {
    pub fn new(
        component: impl Into<String>,
        code: impl Into<String>,
        severity: Severity,
        message: impl Into<String>,
    ) -> Self {
        Self {
            component: component.into(),
            code: code.into(),
            severity,
            message: message.into(),
            correlation_id: None,
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
            progress: None,
            outcome: None,
            error_class: None,
            output: None,
            evidence: None,
            metrics: Vec::new(),
            recovery: Vec::new(),
        }
    }
}

/// Why a native entry was not recorded.
#[derive(Clone, Debug)]
pub enum LogRefusal {
    /// The component or code is empty, finish precedes start, or progress exceeds its total.
    Invalid(&'static str),
    Storage(StorageError),
}
impl From<StorageError> for LogRefusal {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}

/// Whether `component` is canonical: dot-separated segments of lowercase ASCII letters, digits,
/// `_` and `-`, at most 256 bytes. Matching is exact and case-sensitive everywhere (the
/// reserved prefixes, health and the store), so a component can never pass for another by
/// case.
pub fn canonical_component(component: &str) -> bool {
    component.len() <= 256
        && component.split('.').all(|seg| {
            !seg.is_empty()
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        })
}

fn check_entry(e: &LogEntry) -> Result<(), &'static str> {
    if !canonical_component(&e.component) {
        return Err("a component is dot-separated lowercase segments of a-z, 0-9, _ and -");
    }
    if e.code.trim().is_empty() {
        return Err("a diagnostic needs a component and a code");
    }
    if let (Some(s), Some(f)) = (e.started_at_ms, e.finished_at_ms)
        && f < s
    {
        return Err("a diagnostic cannot finish before it started");
    }
    if let Some((done, total)) = e.progress
        && (done < 0 || total < 0 || done > total)
    {
        return Err("completed progress cannot exceed its total");
    }
    if e.attempt.is_some_and(|a| a < 0) {
        return Err("an attempt is non-negative");
    }
    Ok(())
}

/// Redacts and bounds `e` into a record (not yet appended).
fn build_record(
    redactor: &Redactor,
    id: Id,
    now_ms: i64,
    actor: ActorId,
    origin: DiagnosticOrigin,
    e: &LogEntry,
) -> StoredDiagnostic {
    let mut s = Scrub { redactor, count: 0 };
    let component = chars(&e.component, 256);
    let code = chars(&s.text(&e.code), 256);
    let message = s.text(&e.message);
    let message = if message.len() > MESSAGE_LIMIT {
        let kept = head(&message, MESSAGE_LIMIT);
        format!("{kept}… [{} bytes omitted]", message.len() - kept.len())
    } else {
        message
    };
    let (output, omitted) = match e.output.as_deref().filter(|o| !o.is_empty()) {
        None => (None, 0),
        Some(o) if row_sensitive(&component) => (None, o.len() as i64),
        Some(o) => {
            let redacted = s.text(o);
            let kept = tail(&redacted, OUTPUT_LIMIT).to_owned();
            let omitted = (redacted.len() - kept.len()) as i64;
            (Some(kept), omitted)
        }
    };
    let operation = s.label(e.operation.as_deref());
    let target_kind = s.label(e.target_kind.as_deref());
    let stage = s.label(e.stage.as_deref());
    let source_revision = s.label(e.source_revision.as_deref());
    let metrics: Vec<DiagnosticMetric> = e
        .metrics
        .iter()
        .take(128)
        .map(|m| metric(&mut s, m))
        .collect();
    let recovery: Vec<RecoveryAction> = e
        .recovery
        .iter()
        .filter_map(|l| recovery_action(&mut s, l))
        .take(128)
        .collect();
    StoredDiagnostic {
        sequence: 0,
        id,
        recorded_at_ms: now_ms,
        severity: wire_name(&e.severity),
        component,
        code,
        message,
        origin: wire_name(&origin),
        actor,
        correlation_id: e.correlation_id.clone(),
        project_id: e.project_id.clone(),
        operation,
        operation_id: e.operation_id.clone(),
        target_kind,
        target_id: e.target_id.clone(),
        stage,
        source_revision,
        attempt: e.attempt,
        started_at_ms: e.started_at_ms,
        finished_at_ms: e.finished_at_ms,
        progress_completed: e.progress.map(|p| p.0),
        progress_total: e.progress.map(|p| p.1),
        outcome: e.outcome.as_ref().map(wire_name),
        error_class: e.error_class.as_ref().map(wire_name),
        output,
        output_omitted_bytes: omitted,
        redactions: s.count,
        evidence_json: e
            .evidence
            .as_ref()
            .and_then(|r| serde_json::to_string(r).ok()),
        metrics_json: serde_json::to_string(&metrics).unwrap_or_else(|_| "[]".into()),
        recovery_json: serde_json::to_string(&recovery).unwrap_or_else(|_| "[]".into()),
    }
}

/// The live-inspection seam: the highest committed record sequence, with waiting. It is a hint
/// only (a missed one is repaired by `workspace.logs` after the last sequence seen), raised after
/// commit by [`DiagnosticService::log`], [`DiagnosticService::prune`] and, for commands, the
/// runtime ([`DiagnosticService::committed`]).
#[derive(Default)]
pub struct LogWatch {
    latest: Mutex<i64>,
    changed: Condvar,
}
impl LogWatch {
    pub fn latest(&self) -> i64 {
        *self.latest.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn raise(&self, sequence: i64) {
        let mut latest = self.latest.lock().unwrap_or_else(|p| p.into_inner());
        if sequence > *latest {
            *latest = sequence;
            self.changed.notify_all();
        }
    }
    /// Blocks (a non-UI thread) until a record after `after` committed or `timeout` passed, and
    /// returns the latest sequence known.
    pub fn wait_beyond(&self, after: i64, timeout: Duration) -> i64 {
        let latest = self.latest.lock().unwrap_or_else(|p| p.into_inner());
        let (latest, _) = self
            .changed
            .wait_timeout_while(latest, timeout, |l| *l <= after)
            .unwrap_or_else(|p| p.into_inner());
        *latest
    }
}

/// Results of the periodic monitor probes (too costly or effectful for a query).
#[derive(Clone, Debug, Default)]
pub struct MonitorState {
    /// The last integrity check: when, and its problems (or why it could not run).
    pub integrity: Option<(i64, Result<Vec<String>, String>)>,
    /// The last write-owner probe: when, and its latency in ms (or why it failed).
    pub write_probe: Option<(i64, Result<u64, String>)>,
    /// The last retention pass: when, and what it removed (or why it failed).
    pub retention: Option<(i64, Result<PrunedDiagnostics, String>)>,
    /// The last store inspection (ledger, journal, sizes, schema objects): when, and what it
    /// found (or why it could not run). Read outside any caller's snapshot.
    pub database: Option<(i64, Result<StorageDatabase, String>)>,
}

/// Effective retention settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retention {
    pub days: i64,
    pub max_records: i64,
}
fn setting(read: &dyn ConfigurationRead, key: &str) -> Result<i64, StorageError> {
    let d = descriptor(key).expect("catalog setting");
    let layers = current_chain(read, &ConfigScope::Workspace)?;
    let entries = chain(d, &layers).map_err(|m| {
        StorageError::InvalidRecord(format!("{} stored under another schema", m.key))
    })?;
    Ok(entries
        .first()
        .and_then(|e| e.value.value.0.as_i64())
        .or_else(|| d.default_value().as_i64())
        .unwrap_or(0))
}
/// The effective `diagnostics.logRetentionDays` and `diagnostics.logMaxRecords`.
pub fn retention(read: &dyn ConfigurationRead) -> Result<Retention, StorageError> {
    Ok(Retention {
        days: setting(read, LOG_RETENTION_DAYS_KEY)?,
        max_records: setting(read, LOG_MAX_RECORDS_KEY)?,
    })
}

// ---------------------------------------------------------------------------------------------
// Log queries

/// A native log selection (the wire `workspace.logs` filter; also an export's selection).
#[derive(Clone, Debug, Default)]
pub struct LogQuery {
    pub after_sequence: i64,
    pub component: Option<String>,
    pub severity: Option<Severity>,
    pub outcome: Option<DiagnosticOutcome>,
    pub operation: Option<String>,
    pub operation_id: Option<OperationId>,
    pub correlation_id: Option<Id>,
    pub target_id: Option<Id>,
    pub stage: Option<String>,
    pub project_id: Option<ProjectId>,
    pub search: Option<String>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub descending: bool,
}
impl LogQuery {
    fn of(i: &LogsInput) -> Self {
        Self {
            after_sequence: i.after_sequence.get(),
            component: i.component.as_ref().map(|c| c.0.clone()),
            severity: i.severity.clone(),
            outcome: i.outcome.clone(),
            operation: i.operation.as_ref().map(|c| c.0.clone()),
            operation_id: i.operation_id.clone(),
            correlation_id: i.correlation_id.as_ref().map(|c| c.0.clone()),
            target_id: i.target_id.clone(),
            stage: i.stage.as_ref().map(|c| c.0.clone()),
            project_id: i.project_id.clone(),
            search: i.search.as_ref().map(|c| c.0.clone()),
            since_ms: i.since_ms.as_ref().map(Counter::get),
            until_ms: i.until_ms.as_ref().map(Counter::get),
            descending: i
                .order
                .as_ref()
                .is_some_and(|o| wire_name(o) == "Descending"),
        }
    }
    fn filter(&self, audience: DiagnosticAudience) -> DiagnosticFilter {
        DiagnosticFilter {
            after_sequence: self.after_sequence,
            before_sequence: None,
            descending: self.descending,
            component: self.component.clone(),
            min_severity: self
                .severity
                .as_ref()
                .and_then(|s| severity_rank(&wire_name(s))),
            outcome: self.outcome.as_ref().map(wire_name),
            operation: self.operation.clone(),
            operation_id: self.operation_id.clone(),
            correlation_id: self.correlation_id.clone(),
            target_id: self.target_id.clone(),
            stage: self.stage.clone(),
            project_id: self.project_id.clone(),
            audience,
            search: self.search.clone(),
            since_ms: self.since_ms,
            until_ms: self.until_ms,
        }
    }
    /// The selection as structured data (exports carry it, redacted; never a Debug string).
    pub fn selection(&self) -> Value {
        json!({
            "afterSequence": self.after_sequence,
            "component": self.component,
            "severity": self.severity.as_ref().map(wire_name),
            "outcome": self.outcome.as_ref().map(wire_name),
            "operation": self.operation,
            "operationId": self.operation_id.as_ref().map(|o| o.0.as_str()),
            "correlationId": self.correlation_id.as_ref().map(Id::as_str),
            "targetId": self.target_id.as_ref().map(Id::as_str),
            "stage": self.stage,
            "projectId": self.project_id.as_ref().map(|p| p.0.as_str()),
            "search": self.search,
            "sinceMs": self.since_ms,
            "untilMs": self.until_ms,
            "order": if self.descending { "Descending" } else { "Ascending" },
        })
    }
    fn digest(&self, audience: &DiagnosticAudience) -> String {
        let f = self.filter(audience.clone());
        sha256_hex(format!("{f:?}").as_bytes())[..16].to_owned()
    }
}

/// Which records `ctx` may read: `None` refuses (an unbound or inactive non-human principal).
pub fn audience(
    authority: &dyn AuthorityRead,
    ctx: &InvocationContext,
) -> Result<Option<DiagnosticAudience>, StorageError> {
    if matches!(
        ctx.principal_kind(),
        PrincipalKind::Human | PrincipalKind::Host
    ) {
        return Ok(Some(DiagnosticAudience::All));
    }
    Ok(match authority.principal(ctx.principal())? {
        Some(b) if b.active => Some(match b.project_id.or_else(|| ctx.project_id().cloned()) {
            Some(p) => DiagnosticAudience::Project(p),
            None => DiagnosticAudience::All,
        }),
        _ => None,
    })
}

// ---------------------------------------------------------------------------------------------
// Wire records

fn counter(v: i64) -> Result<Counter, StorageError> {
    Counter::new(v.max(0)).map_err(|e| StorageError::InvalidRecord(e.into()))
}
fn label(s: &str) -> BoundedText<1, 256> {
    let t = chars(s, 256);
    BoundedText(if t.is_empty() { "-".into() } else { t })
}

/// The wire record of a stored diagnostic.
pub fn entry(d: &StoredDiagnostic) -> Result<DiagnosticEntry, StorageError> {
    let corrupt = |what: &str| StorageError::unavailable(Unavailable::Corrupt, what.to_owned());
    let metrics: Vec<DiagnosticMetric> =
        serde_json::from_str(&d.metrics_json).map_err(|_| corrupt("stored diagnostic metrics"))?;
    let recovery: Vec<RecoveryAction> = serde_json::from_str(&d.recovery_json)
        .map_err(|_| corrupt("stored diagnostic recovery links"))?;
    let evidence = match &d.evidence_json {
        Some(e) => {
            Some(serde_json::from_str(e).map_err(|_| corrupt("stored diagnostic evidence"))?)
        }
        None => None,
    };
    Ok(DiagnosticEntry {
        id: d.id.clone(),
        severity: parse_wire(&d.severity)?,
        component: label(&d.component),
        code: label(&d.code),
        message: BoundedText(chars(&d.message, 65_536)),
        correlation_id: d.correlation_id.clone().map(CorrelationId),
        evidence,
        sequence: Some(counter(d.sequence)?),
        recorded_at_ms: Some(counter(d.recorded_at_ms)?),
        actor_id: Some(d.actor.clone()),
        origin: Some(parse_wire(&d.origin)?),
        project_id: d.project_id.clone(),
        operation: d.operation.as_deref().map(label),
        operation_id: d.operation_id.clone(),
        target_kind: d.target_kind.as_deref().map(label),
        target_id: d.target_id.clone(),
        stage: d.stage.as_deref().map(label),
        source_revision: d.source_revision.as_deref().map(label),
        attempt: d.attempt.map(counter).transpose()?,
        started_at_ms: d.started_at_ms.map(counter).transpose()?,
        finished_at_ms: d.finished_at_ms.map(counter).transpose()?,
        progress_completed: d.progress_completed.map(counter).transpose()?,
        progress_total: d.progress_total.map(counter).transpose()?,
        outcome: d.outcome.as_deref().map(parse_wire).transpose()?,
        error_class: d.error_class.as_deref().map(parse_wire).transpose()?,
        health: None,
        output: d.output.as_deref().map(|o| BoundedText(chars(o, 65_536))),
        output_omitted_bytes: (d.output_omitted_bytes > 0)
            .then(|| counter(d.output_omitted_bytes))
            .transpose()?,
        redactions: Some(counter(d.redactions)?),
        metrics: (!metrics.is_empty()).then_some(BoundedList(metrics)),
        recovery: (!recovery.is_empty()).then_some(BoundedList(recovery)),
    })
}

/// The wire metric of a native reading (an unavailable value is never reported as zero). Every
/// text member passes the record's redaction, which counts its replacements.
fn metric(s: &mut Scrub<'_>, m: &MetricReading) -> DiagnosticMetric {
    let name = label(&s.text(&m.name));
    let unit = m.unit.as_deref().map(|u| label(&s.text(u)));
    match &m.value {
        Metric::Available(v) => DiagnosticMetric {
            name,
            availability: MetricAvailability::Available,
            value: Some(Counter::new(i64::try_from(*v).unwrap_or(i64::MAX)).expect("positive")),
            unit,
            reason: None,
        },
        Metric::Unavailable(why) => DiagnosticMetric {
            name,
            availability: MetricAvailability::Unavailable,
            value: None,
            unit,
            reason: Some(BoundedText({
                let r = chars(&s.text(why), 4096);
                if r.is_empty() {
                    "unavailable".into()
                } else {
                    r
                }
            })),
        },
    }
}
/// The wire link of a recovery action (catalog operations only), redacted like [`metric`].
fn recovery_action(s: &mut Scrub<'_>, l: &RecoveryLink) -> Option<RecoveryAction> {
    registry::find(l.operation)?;
    Some(RecoveryAction {
        operation: label(l.operation),
        label: BoundedText({
            let t = chars(&s.text(&l.label), 4096);
            if t.is_empty() { l.operation.into() } else { t }
        }),
        target_kind: l.target_kind.as_deref().map(|k| label(&s.text(k))),
        target_id: l.target_id.clone(),
    })
}

// ---------------------------------------------------------------------------------------------
// The service

/// What [`DiagnosticService::render_export`] produced.
#[derive(Clone, Debug)]
pub struct RenderedExport {
    /// The export document (JSON, `application/json`), redacted.
    pub bytes: Vec<u8>,
    pub records: usize,
    /// Matching records the export leaves out ([`EXPORT_MAX_RECORDS`]/[`EXPORT_MAX_BYTES`]).
    pub omitted: usize,
    /// More records matched than the export carries.
    pub truncated: bool,
}

/// Why an export did not reach `artifacts.save`.
#[derive(Clone, Debug)]
pub enum ExportFailure {
    /// The principal may not read diagnostics.
    Denied,
    Invalid(&'static str),
    Staging(ExportRefused),
    /// Another attempt holds this request key's claim and has not recorded its command yet;
    /// retry the same request.
    Pending,
    Storage(StorageError),
}

/// The diagnostics domain service (see the module documentation).
pub struct DiagnosticService {
    store: Arc<dyn ApplicationStore>,
    ids: Arc<dyn IdSource>,
    clock: Arc<dyn Clock>,
    storage: Option<Arc<dyn StorageHealthProbe>>,
    redactor: RwLock<Redactor>,
    watch: LogWatch,
    sources: RwLock<Vec<Arc<dyn HealthSource>>>,
    consumers: RwLock<Option<Vec<String>>>,
    monitor: Mutex<MonitorState>,
    /// Where exports are staged (see [`Self::set_export_staging`]).
    export_staging: RwLock<Option<Arc<dyn ExportStaging>>>,
}

impl DiagnosticService {
    /// `storage`: the storage adapter's health probes (`None` reports storage as unavailable).
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        ids: Arc<dyn IdSource>,
        clock: Arc<dyn Clock>,
        storage: Option<Arc<dyn StorageHealthProbe>>,
    ) -> Self {
        Self {
            store,
            ids,
            clock,
            storage,
            redactor: RwLock::new(Redactor::new()),
            watch: LogWatch::default(),
            sources: RwLock::new(Vec::new()),
            consumers: RwLock::new(None),
            monitor: Mutex::new(MonitorState::default()),
            export_staging: RwLock::new(None),
        }
    }

    /// A storage error as text for health and monitor results: its Display form (never Debug,
    /// whose escaping would hide a secret from the exact-value redactor), redacted with the
    /// registered secrets.
    fn error_text(&self, e: &StorageError) -> String {
        self.redactor
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .redact_text(&e.to_string())
    }

    /// Registers a resolved secret value: every later record and export has it removed.
    pub fn register_secret(&self, value: &str) {
        self.redactor
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .register(value);
    }
    /// Adds a health source (boot recovery, collection, a later indexer helper, the router).
    pub fn register_source(&self, source: Arc<dyn HealthSource>) {
        self.sources
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .push(source);
    }
    /// The durable delivery consumers whose queue health is reported.
    pub fn set_consumers(&self, consumers: Vec<String>) {
        *self.consumers.write().unwrap_or_else(|p| p.into_inner()) = Some(consumers);
    }
    /// The live-inspection seam.
    pub fn watch(&self) -> &LogWatch {
        &self.watch
    }
    /// The monitor probes' last results.
    pub fn monitor(&self) -> MonitorState {
        self.monitor
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub fn store(&self) -> &Arc<dyn ApplicationStore> {
        &self.store
    }

    fn now(&self) -> i64 {
        self.clock.now_ms().get()
    }

    /// Redacts and bounds `e` into a record (not yet appended).
    fn record(
        &self,
        id: Id,
        now_ms: i64,
        actor: ActorId,
        origin: DiagnosticOrigin,
        e: &LogEntry,
    ) -> StoredDiagnostic {
        let redactor = self.redactor.read().unwrap_or_else(|p| p.into_inner());
        build_record(&redactor, id, now_ms, actor, origin, e)
    }

    /// Records a host diagnostic inside the caller's unit of work (for example an indexing stage
    /// change committed with the stage's own state). Origin `Host`, attributed to the store's
    /// host actor. Raise the live hint after commit with [`Self::committed`].
    pub fn log_in(
        &self,
        tx: &mut dyn WriteTransaction,
        entry: &LogEntry,
    ) -> Result<StoredDiagnostic, LogRefusal> {
        check_entry(entry).map_err(LogRefusal::Invalid)?;
        let id = tx.new_id(IdKind::Diagnostic);
        let actor = tx.store().host_actor.clone();
        let record = self.record(id, tx.now_ms(), actor, DiagnosticOrigin::Host, entry);
        Ok(tx.diagnostics().append_diagnostic(&record)?)
    }

    /// Records a host diagnostic in its own unit of work and raises the live hint.
    pub fn log(&self, entry: LogEntry) -> Result<StoredDiagnostic, LogRefusal> {
        self.log_with_id(self.ids.next_id(IdKind::Diagnostic), entry)
    }

    /// [`Self::log`] under an identity allocated earlier (a refusal that names the diagnostic
    /// carrying its detail before the diagnostic can be written).
    pub fn log_with_id(&self, id: Id, entry: LogEntry) -> Result<StoredDiagnostic, LogRefusal> {
        check_entry(&entry).map_err(LogRefusal::Invalid)?;
        let actor = self.store.info().host_actor;
        let record = self.record(id, self.now(), actor, DiagnosticOrigin::Host, &entry);
        let stored = run_write(&*self.store, WriteOptions::default(), move |tx| {
            tx.diagnostics().append_diagnostic(&record)
        })?;
        self.watch.raise(stored.sequence);
        Ok(stored)
    }

    /// Raises the live hint to the latest committed record (after a command the runtime
    /// admitted, or the caller's own unit of work, committed).
    pub fn committed(&self) {
        if let Ok(stats) = read_snapshot(&*self.store, &ReadOptions::default(), |s| {
            s.diagnostics().diagnostic_stats()
        }) {
            self.watch.raise(stats.latest_sequence);
        }
    }

    /// Opens the log of one run (an indexing run, a provider attempt, a helper process).
    pub fn run(&self, channel: &str, operation: &str) -> RunLog<'_> {
        RunLog {
            service: self,
            base: LogEntry {
                operation: Some(operation.to_owned()),
                ..LogEntry::new(channel, "run", Severity::Info, "")
            },
        }
    }

    /// Applies the retention settings: removes records older than the retention window, then the
    /// oldest beyond the record cap, and records what it removed (when it removed anything).
    pub fn prune(&self) -> Result<PrunedDiagnostics, StorageError> {
        let host = self.store.info().host_actor;
        let id = self.ids.next_id(IdKind::Diagnostic);
        let outcome = run_write(&*self.store, WriteOptions::default(), {
            let redactor = self
                .redactor
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            move |tx| {
                let r = retention(tx.configuration())?;
                let now = tx.now_ms();
                let older = now.saturating_sub(r.days.saturating_mul(DAY_MS));
                let mut pruned = tx.diagnostics().prune_diagnostics(older, r.max_records)?;
                if pruned.expired + pruned.over_cap > 0 {
                    let entry = LogEntry {
                        metrics: vec![
                            MetricReading::new(
                                "expired",
                                Some("records"),
                                Metric::Available(pruned.expired as u64),
                            ),
                            MetricReading::new(
                                "overCap",
                                Some("records"),
                                Metric::Available(pruned.over_cap as u64),
                            ),
                        ],
                        ..LogEntry::new(
                            channels::RETENTION,
                            "pruned",
                            Severity::Info,
                            format!(
                                "Removed {} records older than {} days and {} beyond the {}-record cap.",
                                pruned.expired, r.days, pruned.over_cap, r.max_records
                            ),
                        )
                    };
                    let record =
                        build_record(&redactor, id, now, host, DiagnosticOrigin::Host, &entry);
                    tx.diagnostics().append_diagnostic(&record)?;
                    // The pass's own record must not leave the log above its cap.
                    pruned.over_cap += tx
                        .diagnostics()
                        .prune_diagnostics(older, r.max_records)?
                        .over_cap;
                }
                Ok(pruned)
            }
        });
        self.monitor
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retention = Some((self.now(), outcome.clone().map_err(|e| self.error_text(&e))));
        self.committed();
        outcome
    }

    /// Runs the monitor probes: the store inspection (migration ledger, journal, sizes, schema
    /// objects), the full integrity check and a write-owner probe (an empty unit of work on the
    /// one write owner, the memory-write probe's replacement). Call it outside any read
    /// snapshot: the inspection takes a reader of its own.
    pub fn refresh_monitor(&self) -> MonitorState {
        let database = self.storage.as_ref().map(|p| {
            let r = p.database().map_err(|e| self.error_text(&e));
            (self.now(), r)
        });
        let integrity = self.storage.as_ref().map(|p| {
            let r = p
                .integrity()
                .map(|r| r.problems)
                .map_err(|e| self.error_text(&e));
            (self.now(), r)
        });
        let start = std::time::Instant::now();
        let probe = run_write(&*self.store, WriteOptions::default(), |tx| {
            tx.committed_sequence()
        })
        .map(|_| start.elapsed().as_millis() as u64)
        .map_err(|e| self.error_text(&e));
        let mut m = self.monitor.lock().unwrap_or_else(|p| p.into_inner());
        if integrity.is_some() {
            m.integrity = integrity;
        }
        if database.is_some() {
            m.database = database;
        }
        m.write_probe = Some((self.now(), probe));
        m.clone()
    }

    /// The current health checks visible to `ctx` (as `workspace.diagnostics` reports them).
    pub fn health(&self, ctx: &InvocationContext) -> Result<Vec<HealthCheck>, StorageError> {
        read_snapshot(&*self.store, &ReadOptions::default(), |s| {
            Ok(health::checks(self, ctx, s, self.now()))
        })
    }

    /// The current health checks as wire records (what `workspace.diagnostics` returns at
    /// revision 2), redacted with the registered secrets.
    pub fn health_entries(
        &self,
        ctx: &InvocationContext,
    ) -> Result<Vec<DiagnosticEntry>, StorageError> {
        let now = self.now();
        let host = self.store.info().host_actor;
        let redactor = self.redactor.read().unwrap_or_else(|p| p.into_inner());
        self.health(ctx)?
            .iter()
            .map(|c| health::wire(c, &host, now, &redactor))
            .collect()
    }

    /// Reads one page of the log for `ctx` (the native form of `workspace.logs`).
    pub fn logs(
        &self,
        ctx: &InvocationContext,
        query: &LogQuery,
        limit: u16,
    ) -> Result<Option<Vec<DiagnosticEntry>>, StorageError> {
        read_snapshot(&*self.store, &ReadOptions::default(), |s| {
            let Some(audience) = audience(s.authority(), ctx)? else {
                return Ok(None);
            };
            let pruned = s.diagnostics().diagnostic_stats()?.pruned_through;
            if retention_gap(query.descending, query.after_sequence, None, pruned) {
                return Err(StorageError::CursorExpired);
            }
            let rows = s
                .diagnostics()
                .diagnostics(&query.filter(audience), limit.clamp(1, 500))?;
            rows.iter()
                .map(entry)
                .collect::<Result<Vec<_>, _>>()
                .map(Some)
        })
    }

    /// Plain text of selected records for the clipboard (already redacted records, redacted
    /// again).
    pub fn copy_text(&self, entries: &[DiagnosticEntry]) -> String {
        let redactor = self.redactor.read().unwrap_or_else(|p| p.into_inner());
        let mut out = String::new();
        for e in entries {
            let line = format!(
                "#{} {} {} {} {}{}: {}\n",
                e.sequence.as_ref().map_or(0, Counter::get),
                e.recorded_at_ms.as_ref().map_or(0, Counter::get),
                wire_name(&e.severity),
                e.component.0,
                e.code.0,
                e.stage
                    .as_ref()
                    .map_or(String::new(), |s| format!(" [{}]", s.0)),
                e.message.0,
            );
            out.push_str(&line);
            if let Some(o) = &e.output {
                out.push_str(&o.0);
                if !o.0.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
        redactor.redact_text(&out)
    }

    /// Renders a bounded, redacted export of the records `query` selects (oldest first) and the
    /// current health checks, as `ctx` may see them. The bound is the final document: its
    /// serialized bytes, health checks, envelope and redaction included, are at most
    /// [`EXPORT_MAX_BYTES`]. Records that do not fit are dropped from the end (newest), and the
    /// document says how many matched and how many it omits.
    pub fn render_export(
        &self,
        ctx: &InvocationContext,
        query: &LogQuery,
    ) -> Result<RenderedExport, ExportFailure> {
        if let (Some(a), Some(b)) = (query.since_ms, query.until_ms)
            && a > b
        {
            return Err(ExportFailure::Invalid("since must be at or before until"));
        }
        let mut q = query.clone();
        q.descending = false;
        let now = self.now();
        let redactor = self
            .redactor
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let (records, matched, health) =
            read_snapshot(&*self.store, &ReadOptions::default(), |s| {
                let Some(audience) = audience(s.authority(), ctx)? else {
                    return Ok(None);
                };
                let mut filter = q.filter(audience);
                let matched = s.diagnostics().count_diagnostics(&filter)?;
                // Candidates: redacted as they will be written. A record's compact size is a
                // lower bound of its share of the final document, so reading stops once their
                // sum alone exceeds the bound.
                let mut records = Vec::new();
                let mut compact = 0usize;
                'pages: loop {
                    let page = s.diagnostics().diagnostics(&filter, 500)?;
                    let done = page.len() < 500;
                    for d in &page {
                        if records.len() >= EXPORT_MAX_RECORDS || compact > EXPORT_MAX_BYTES {
                            break 'pages;
                        }
                        let e = redactor
                            .redact_json(&serde_json::to_value(entry(d)?).unwrap_or(Value::Null));
                        compact += e.to_string().len();
                        records.push(e);
                        filter.after_sequence = d.sequence;
                    }
                    if done {
                        break;
                    }
                }
                let health: Vec<Value> = health::checks(self, ctx, s, now)
                    .iter()
                    .filter_map(|c| {
                        health::wire(c, &self.store.info().host_actor, now, &redactor).ok()
                    })
                    .filter_map(|e| serde_json::to_value(e).ok())
                    .collect();
                Ok(Some((records, matched.max(0) as usize, health)))
            })
            .map_err(ExportFailure::Storage)?
            .ok_or(ExportFailure::Denied)?;
        // Structural, then redacted: Debug escaping (`\"`, `\\`, `\n`) would hide a registered
        // secret from the exact-value redactor.
        let selection = redactor.redact_json(&q.selection());
        let health = Value::Array(health);
        let document = |k: usize| {
            let omitted = matched.saturating_sub(k);
            serde_json::to_vec_pretty(&json!({
                "format": EXPORT_FORMAT,
                "exportedAtMs": now,
                "storeId": self.store.info().store_id.0.as_str(),
                "exportedBy": ctx.principal().0.as_str(),
                "selection": selection,
                "matchedRecords": matched,
                "omittedRecords": omitted,
                "truncated": omitted > 0,
                "health": health,
                "records": &records[..k],
            }))
            .unwrap_or_default()
        };
        // The most records whose whole document fits (its size grows with the record count).
        let mut best = document(0);
        if best.len() > EXPORT_MAX_BYTES {
            return Err(ExportFailure::Invalid(
                "the health checks alone exceed the export size limit",
            ));
        }
        let (mut lo, mut hi, mut kept) = (1usize, records.len(), 0usize);
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            let bytes = document(mid);
            if bytes.len() <= EXPORT_MAX_BYTES {
                best = bytes;
                kept = mid;
                lo = mid + 1;
            } else {
                hi = mid - 1;
            }
        }
        Ok(RenderedExport {
            bytes: best,
            records: kept,
            omitted: matched.saturating_sub(kept),
            truncated: kept < matched,
        })
    }

    /// Attaches the staging used by [`Self::export`] and swept by [`Self::sweep_exports`].
    pub fn set_export_staging(&self, staging: Arc<dyn ExportStaging>) {
        *self
            .export_staging
            .write()
            .unwrap_or_else(|p| p.into_inner()) = Some(staging);
    }
    /// Removes staged export files left past their ticket's lifetime (a save that never
    /// resolved them); the runtime's diagnostics monitor runs it every pass.
    pub fn sweep_exports(&self) -> usize {
        let staging = self
            .export_staging
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        staging.map_or(0, |s| s.sweep())
    }

    /// Exports the selection as an artifact of the invocation's project through the one
    /// artifact service: claims the export's intent, renders it, stages it (the attached
    /// [`ExportStaging`]), records the command and submits `artifacts.save` under
    /// `request_key` with `submit` (the runtime's command admission), returning that command's
    /// outcome.
    ///
    /// **One claimant per request key.** The intent is claimed atomically (insert-if-absent,
    /// re-read in the same unit of work) before anything is staged, binding the key to the
    /// selection and destination project. Only the claimant renders, stages and records the
    /// command, once; a recorded command is never replaced. Any other attempt with the key
    /// (a retry, or an export overlapping the claimant's) waits for the claimant's command, up
    /// to [`EXPORT_CLAIM_WAIT`], and resubmits it unchanged, so the gateway replays its receipt
    /// under current authority or, if the claimant's submission is still running, decides it
    /// once. A claim pending past [`EXPORT_CLAIM_STALE_MS`] (its claimant stopped) is taken over.
    /// The key used for a different selection or project, or by another command, is
    /// `IdempotencyConflict`; nothing is staged. A retry after the staged file's ticket expired
    /// is refused by the save (its source no longer resolves): export again under a new key.
    ///
    /// **Staging.** Once the save has a durable outcome, or a rejection the same command cannot
    /// overcome, the staged file is released; after a retryable rejection or an unknown outcome
    /// it stays for the retry and is swept after its ticket expires.
    pub fn export(
        &self,
        ctx: &InvocationContext,
        query: &LogQuery,
        request_key: &str,
        submit: impl FnOnce(&[u8]) -> CommandOutcome,
    ) -> Result<CommandOutcome, ExportFailure> {
        let Some(project) = ctx.project_id().cloned() else {
            return Err(ExportFailure::Staging(ExportRefused::NoProject));
        };
        let staging = self
            .export_staging
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .ok_or(ExportFailure::Staging(ExportRefused::Unavailable))?;
        let key = Id::new(request_key)
            .map_err(|_| ExportFailure::Invalid("a request key is an opaque identifier"))?;
        let identity = ReceiptIdentity::bind(ctx, RequestKey(key.clone()));
        let selection = sha256_hex(
            json!({"selection": query.selection(), "projectId": project.0.as_str()})
                .to_string()
                .as_bytes(),
        );
        let conflict = || {
            Ok(Ok(CommandReply::Rejected {
                protocol: Protocol::Draft1,
                request_key: RequestKey(key.clone()),
                code: Rejection::IdempotencyConflict,
                retry: RetryAdvice::RefreshAndNewRequest,
                correlation_id: CorrelationId(self.ids.next_id(IdKind::Correlation)),
            }))
        };
        let mut mine = ExportIntent {
            actor: identity.actor_id().clone(),
            scope_key: identity.scope_key().as_str().to_owned(),
            request_key: request_key.to_owned(),
            project_id: project.clone(),
            selection_sha256: selection,
            command: None,
            claim: self.ids.next_id(IdKind::Diagnostic).as_str().to_owned(),
            created_at_ms: self.now(),
        };
        let claim = |intent: ExportIntent| {
            let identity = identity.clone();
            run_write(&*self.store, WriteOptions::default(), move |tx| {
                let stored = tx.diagnostics().claim_export_intent(&intent)?;
                let receipt = tx.receipts().lookup(&identity)?.is_some();
                Ok((stored, receipt))
            })
            .map_err(ExportFailure::Storage)
        };
        let (mut stored, mut receipt) = claim(mine.clone())?;
        let deadline = std::time::Instant::now() + EXPORT_CLAIM_WAIT;
        while stored.claim != mine.claim {
            if stored.selection_sha256 != mine.selection_sha256 || stored.project_id != project {
                return conflict();
            }
            if let Some(command) = &stored.command {
                // The claimant's command: replayed (or decided once) by the gateway. Once it is
                // settled its staged file (if this process staged it) is no longer needed.
                let outcome = submit(command.as_bytes());
                if save_settled(&outcome)
                    && let Some(source) = staged_source(command)
                {
                    staging.release(&source);
                }
                return Ok(outcome);
            }
            if self.now().saturating_sub(stored.created_at_ms) > EXPORT_CLAIM_STALE_MS {
                let (from, takeover) = (stored.claim.clone(), mine.clone());
                let taken = run_write(&*self.store, WriteOptions::default(), move |tx| {
                    tx.diagnostics().take_over_export_intent(&takeover, &from)
                })
                .map_err(ExportFailure::Storage)?;
                if taken {
                    break;
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(ExportFailure::Pending);
            }
            std::thread::sleep(EXPORT_CLAIM_POLL);
            // Re-claim: the claimant may have recorded its command, or released its claim.
            mine.created_at_ms = self.now();
            (stored, receipt) = claim(mine.clone())?;
        }
        // This attempt holds the claim.
        let release_claim = |intent: &ExportIntent| {
            let intent = intent.clone();
            let _ = run_write(&*self.store, WriteOptions::default(), move |tx| {
                tx.diagnostics().release_export_intent(&intent)
            });
        };
        if receipt {
            // The key already belongs to another command.
            release_claim(&mine);
            return conflict();
        }
        let rendered = match self.render_export(ctx, query) {
            Ok(r) => r,
            Err(e) => {
                release_claim(&mine);
                return Err(e);
            }
        };
        let name = format!("devworks-diagnostics-{}.json", self.now());
        let source = match staging.stage(ctx, &name, &rendered.bytes) {
            Ok(s) => s,
            Err(e) => {
                release_claim(&mine);
                return Err(ExportFailure::Staging(e));
            }
        };
        let frame = json!({"protocol": "1.0-draft.1", "requestKey": request_key, "command": {
            "kind": "Save", "projectId": project.0.as_str(), "title": "Diagnostics export",
            "mediaType": "application/json", "source": source,
            "expectedDigest": sha256_hex(&rendered.bytes),
            "expectedBytes": rendered.bytes.len().to_string()}})
        .to_string();
        let (claimed, command) = (mine.clone(), frame.clone());
        let recorded = run_write(&*self.store, WriteOptions::default(), move |tx| {
            tx.diagnostics().complete_export_intent(&claimed, &command)
        });
        match recorded {
            Ok(true) => {}
            Ok(false) => {
                // The claim was taken over meanwhile: the other attempt owns the key now.
                staging.release(&source);
                return Err(ExportFailure::Pending);
            }
            Err(e) => {
                // The write may have committed (a lost acknowledgment) or not: what is durable
                // decides. Re-read the intent before cleaning anything up.
                let (actor, scope) = (identity.actor_id().clone(), mine.scope_key.clone());
                let durable = read_snapshot(&*self.store, &ReadOptions::default(), |s| {
                    s.diagnostics().export_intent(&actor, &scope, request_key)
                });
                match durable {
                    // It committed: the recorded command is this one, so submit it.
                    Ok(Some(i))
                        if i.claim == mine.claim && i.command.as_deref() == Some(&frame) => {}
                    // It did not commit and the claim is still this attempt's: nothing refers to
                    // the staged file, so both go.
                    Ok(Some(i)) if i.claim == mine.claim && i.command.is_none() => {
                        staging.release(&source);
                        release_claim(&mine);
                        return Err(ExportFailure::Storage(e));
                    }
                    // Another attempt holds the key now, and it never recorded this command.
                    Ok(Some(i)) if i.command.as_deref() != Some(&frame) => {
                        staging.release(&source);
                        return Err(ExportFailure::Pending);
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        staging.release(&source);
                        return Err(ExportFailure::Storage(e));
                    }
                    // Still unknown: keep the staged file (a recorded command may name it); a
                    // retry resolves the intent, and the sweep removes the file after its
                    // ticket expires.
                    Err(_) => return Err(ExportFailure::Storage(e)),
                }
            }
        }
        let outcome = submit(frame.as_bytes());
        if save_settled(&outcome) {
            staging.release(&source);
        }
        Ok(outcome)
    }
}

/// The staged source an export's recorded `artifacts.save` command names.
fn staged_source(command: &str) -> Option<crate::protocol::data::ArtifactSource> {
    let v: Value = serde_json::from_str(command).ok()?;
    serde_json::from_value(v.get("command")?.get("source")?.clone()).ok()
}

/// Whether a save's outcome no longer needs its staged file: it has a durable outcome (a
/// receipt), or it was rejected in a way the same command cannot overcome. A rejection the
/// same command may still pass (`SameCommand`, `AfterBlockerClears`), an unknown commit or a
/// gateway failure keeps the file for the retry; the sweep removes it once its ticket expires.
fn save_settled(outcome: &CommandOutcome) -> bool {
    match outcome {
        Ok(CommandReply::Recorded { .. }) => true,
        Ok(CommandReply::Rejected { retry, .. }) => !matches!(
            retry,
            RetryAdvice::SameCommand | RetryAdvice::AfterBlockerClears
        ),
        _ => false,
    }
}

/// The log of one run on one channel: every record carries the run's operation, target, source
/// revision, attempt and start time (LOG-01/02). Indexing writes its stages here.
pub struct RunLog<'a> {
    service: &'a DiagnosticService,
    base: LogEntry,
}
impl RunLog<'_> {
    pub fn operation_id(mut self, id: OperationId) -> Self {
        self.base.operation_id = Some(id);
        self
    }
    pub fn target(mut self, kind: &str, id: Id) -> Self {
        self.base.target_kind = Some(kind.to_owned());
        self.base.target_id = Some(id);
        self
    }
    pub fn source_revision(mut self, revision: &str) -> Self {
        self.base.source_revision = Some(revision.to_owned());
        self
    }
    pub fn attempt(mut self, attempt: i64) -> Self {
        self.base.attempt = Some(attempt);
        self
    }
    pub fn project(mut self, project: ProjectId) -> Self {
        self.base.project_id = Some(project);
        self
    }
    pub fn correlation(mut self, id: Id) -> Self {
        self.base.correlation_id = Some(id);
        self
    }
    pub fn started_at(mut self, ms: i64) -> Self {
        self.base.started_at_ms = Some(ms);
        self
    }
    /// A progress or warning record of `stage` (outcome `Running`).
    pub fn stage(
        &self,
        stage: &str,
        severity: Severity,
        message: &str,
        progress: Option<(i64, i64)>,
    ) -> Result<StoredDiagnostic, LogRefusal> {
        self.service.log(LogEntry {
            code: if matches!(severity, Severity::Info) {
                "stage".into()
            } else {
                "warning".into()
            },
            severity,
            message: message.to_owned(),
            stage: Some(stage.to_owned()),
            progress,
            outcome: Some(DiagnosticOutcome::Running),
            ..self.base.clone()
        })
    }
    /// The run's final record: its outcome, error class, helper output and the safe recovery
    /// actions it offers.
    #[allow(clippy::too_many_arguments)]
    pub fn finish(
        &self,
        stage: &str,
        outcome: DiagnosticOutcome,
        error_class: Option<DiagnosticErrorClass>,
        message: &str,
        output: Option<&str>,
        progress: Option<(i64, i64)>,
        recovery: Vec<RecoveryLink>,
    ) -> Result<StoredDiagnostic, LogRefusal> {
        let severity = match outcome {
            DiagnosticOutcome::Succeeded | DiagnosticOutcome::Running => Severity::Info,
            DiagnosticOutcome::Failed => Severity::Error,
            _ => Severity::Warning,
        };
        self.service.log(LogEntry {
            code: wire_name(&outcome).to_lowercase(),
            severity,
            message: message.to_owned(),
            stage: Some(stage.to_owned()),
            progress,
            finished_at_ms: Some(self.service.now()),
            outcome: Some(outcome),
            error_class,
            output: output.map(str::to_owned),
            recovery,
            ..self.base.clone()
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Wire handlers

fn fault_of(code: DomainErrorCode, retry: RetryAdvice, correlation: &CorrelationId) -> DomainFault {
    super::fault(code, retry, correlation)
}
fn retry_of(e: &StorageError) -> RetryAdvice {
    match e {
        StorageError::InvalidRecord(_) | StorageError::IncompatibleSchema(_) => RetryAdvice::Never,
        _ => RetryAdvice::SameCommand,
    }
}

enum CursorError {
    Malformed,
    Stale,
}
/// A continuation: its position, the retention boundary when it was issued, and the binding of
/// its store, restore epoch, filter and audience.
fn encode_cursor(position: i64, boundary: i64, binding: &str) -> BoundedText<1, 4096> {
    BoundedText(format!("{CURSOR_VERSION}.{position}.{boundary}.{binding}"))
}
fn decode_cursor(raw: &str, binding: &str) -> Result<(i64, i64), CursorError> {
    let parts: Vec<&str> = raw.split('.').collect();
    let [version, position, boundary, bound] = parts.as_slice() else {
        return Err(CursorError::Malformed);
    };
    if *version != CURSOR_VERSION {
        return Err(CursorError::Malformed);
    }
    let position = position
        .parse::<i64>()
        .map_err(|_| CursorError::Malformed)?;
    let boundary = boundary
        .parse::<i64>()
        .map_err(|_| CursorError::Malformed)?;
    if *bound != binding {
        return Err(CursorError::Stale);
    }
    Ok((position, boundary))
}

/// Whether retention removed part of a log read's unread range (shared contracts: return
/// `CursorExpired` rather than pretend a page is complete). `pruned_through` is the current
/// retention boundary.
///
/// * Oldest first, the unread range starts after the cursor position (or `afterSequence`): it
///   lost records when the boundary passed that position. Reading from 0 means "from the oldest
///   retained record" and never expires.
/// * Newest first, the unread range lies between `afterSequence` and the cursor: it lost
///   records when the boundary passed `afterSequence` since the cursor was issued, or (a first
///   page) when the boundary already lies above a nonzero `afterSequence`.
fn retention_gap(
    descending: bool,
    after: i64,
    cursor: Option<(i64, i64)>,
    pruned_through: i64,
) -> bool {
    if descending {
        match cursor {
            Some((_, issued)) => pruned_through > issued && pruned_through > after,
            None => after > 0 && pruned_through > after,
        }
    } else {
        let from = cursor.map_or(after, |(position, _)| position.max(after));
        from > 0 && pruned_through > from
    }
}

impl QueryHandler for DiagnosticService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let correlation = CorrelationId(ctx.invocation_id().clone());
        let fault = |code, retry| fault_of(code, retry, &correlation);
        let storage = |e: StorageError| fault(DomainErrorCode::StorageUnavailable, retry_of(&e));
        let audience = audience(snapshot.authority(), ctx)
            .map_err(storage)?
            .ok_or_else(|| fault(DomainErrorCode::PermissionDenied, RetryAdvice::Never))?;
        let stamp = snapshot.stamp();
        let cursor_error = |e: CursorError| match e {
            CursorError::Malformed => fault(DomainErrorCode::InvalidArgument, RetryAdvice::Never),
            CursorError::Stale => fault(
                DomainErrorCode::CursorExpired,
                RetryAdvice::RefreshAndNewRequest,
            ),
        };
        let result = match query.input() {
            OperationInput::Workspace(Input::Logs(i)) => {
                let q = LogQuery::of(i);
                if let (Some(a), Some(b)) = (q.since_ms, q.until_ms)
                    && a > b
                {
                    return Err(fault(DomainErrorCode::InvalidArgument, RetryAdvice::Never));
                }
                let binding = format!(
                    "{}{}",
                    &sha256_hex(
                        format!("{}|{}", stamp.store_id.0.as_str(), stamp.restore_epoch).as_bytes()
                    )[..8],
                    q.digest(&audience)
                );
                let mut filter = q.filter(audience);
                let stats = snapshot.diagnostics().diagnostic_stats().map_err(storage)?;
                let cursor = match &i.page.cursor {
                    Some(c) => Some(decode_cursor(&c.0, &binding).map_err(cursor_error)?),
                    None => None,
                };
                if retention_gap(
                    filter.descending,
                    filter.after_sequence,
                    cursor,
                    stats.pruned_through,
                ) {
                    return Err(fault(
                        DomainErrorCode::CursorExpired,
                        RetryAdvice::RefreshAndNewRequest,
                    ));
                }
                if let Some((at, _)) = cursor {
                    if filter.descending {
                        filter.before_sequence = Some(at);
                    } else {
                        filter.after_sequence = filter.after_sequence.max(at);
                    }
                }
                let limit = u16::try_from(i.page.limit.get()).unwrap_or(500).min(500);
                let mut rows = snapshot
                    .diagnostics()
                    .diagnostics(&filter, (limit + 1).min(500))
                    .map_err(storage)?;
                let has_more = rows.len() > usize::from(limit)
                    || (limit == 500 && rows.len() == 500 && {
                        let mut probe = filter.clone();
                        let last = rows.last().map_or(0, |r| r.sequence);
                        if probe.descending {
                            probe.before_sequence = Some(last);
                        } else {
                            probe.after_sequence = last;
                        }
                        !snapshot
                            .diagnostics()
                            .diagnostics(&probe, 1)
                            .map_err(storage)?
                            .is_empty()
                    });
                rows.truncate(usize::from(limit));
                let next_cursor = (has_more && !rows.is_empty()).then(|| {
                    encode_cursor(
                        rows.last().map_or(0, |r| r.sequence),
                        stats.pruned_through,
                        &binding,
                    )
                });
                let items = rows
                    .iter()
                    .map(entry)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(storage)?;
                OperationResult::WorkspaceLogs {
                    value: DiagnosticPage {
                        items: BoundedList(items),
                        has_more: next_cursor.is_some(),
                        next_cursor,
                        latest_sequence: Some(counter(stats.latest_sequence).map_err(storage)?),
                    },
                }
            }
            OperationInput::Workspace(Input::Diagnostics(i)) => {
                let now = self.now();
                let host = self.store.info().host_actor;
                let checks: Vec<HealthCheck> = health::checks(self, ctx, snapshot, now)
                    .into_iter()
                    .filter(|c| {
                        i.component
                            .as_ref()
                            .is_none_or(|p| under(&c.component, &p.0))
                    })
                    .collect();
                // The offset is meaningful only over the same list of checks: a source or
                // consumer that appeared or went away since the cursor was issued expires it.
                let components: Vec<&str> = checks.iter().map(|c| c.component.as_str()).collect();
                let binding = sha256_hex(
                    format!(
                        "{}|{}|{}|{}",
                        stamp.store_id.0.as_str(),
                        stamp.restore_epoch,
                        i.component.as_ref().map_or("", |c| c.0.as_str()),
                        components.join(",")
                    )
                    .as_bytes(),
                )[..16]
                    .to_owned();
                let start = match &i.page.cursor {
                    Some(c) => {
                        usize::try_from(decode_cursor(&c.0, &binding).map_err(cursor_error)?.0)
                            .unwrap_or(usize::MAX)
                    }
                    None => 0,
                };
                let limit = usize::try_from(i.page.limit.get()).unwrap_or(500).min(500);
                let page: Vec<&HealthCheck> = checks.iter().skip(start).take(limit).collect();
                let end = start + page.len();
                let next_cursor =
                    (end < checks.len()).then(|| encode_cursor(end as i64, 0, &binding));
                let items = page
                    .into_iter()
                    .map(|c| {
                        let redactor = self.redactor.read().unwrap_or_else(|p| p.into_inner());
                        health::wire(c, &host, now, &redactor)
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(storage)?;
                let stats = snapshot.diagnostics().diagnostic_stats().map_err(storage)?;
                OperationResult::WorkspaceDiagnostics {
                    value: DiagnosticPage {
                        items: BoundedList(items),
                        has_more: next_cursor.is_some(),
                        next_cursor,
                        latest_sequence: Some(counter(stats.latest_sequence).map_err(storage)?),
                    },
                }
            }
            _ => {
                return Err(fault(
                    DomainErrorCode::UnsupportedCapability,
                    RetryAdvice::Never,
                ));
            }
        };
        Ok(QueryAnswer {
            stamp: stamp.read_stamp(
                ctx.policy_revision().clone(),
                // Diagnostics are host-scoped; no project scope revision applies.
                Revision::new(1).expect("positive"),
            ),
            result,
        })
    }
}

fn refused(fault: DomainFault) -> Decided {
    Decided {
        decision: DomainDecision::Refused { fault },
        operation_id: None,
    }
}

/// The reported entry of a `workspace.record_diagnostic` input.
fn reported(i: &RecordDiagnosticInput, ctx: &InvocationContext) -> LogEntry {
    let text = |t: &Option<BoundedText<1, 256>>| t.as_ref().map(|t| t.0.clone());
    LogEntry {
        correlation_id: Some(ctx.invocation_id().clone()),
        project_id: ctx.project_id().cloned(),
        operation: text(&i.operation),
        operation_id: i.operation_id.clone(),
        target_kind: text(&i.target_kind),
        target_id: i.target_id.clone(),
        stage: text(&i.stage),
        source_revision: text(&i.source_revision),
        attempt: i.attempt.as_ref().map(Counter::get),
        started_at_ms: i.started_at_ms.as_ref().map(Counter::get),
        finished_at_ms: i.finished_at_ms.as_ref().map(Counter::get),
        progress: match (&i.progress_completed, &i.progress_total) {
            (Some(d), Some(t)) => Some((d.get(), t.get())),
            (Some(d), None) => Some((d.get(), d.get())),
            (None, Some(t)) => Some((0, t.get())),
            (None, None) => None,
        },
        outcome: i.outcome.clone(),
        error_class: i.error_class.clone(),
        output: i.output.as_ref().map(|o| o.0.clone()),
        evidence: i.evidence.clone(),
        ..LogEntry::new(
            i.component.0.clone(),
            i.code.0.clone(),
            i.severity.clone(),
            i.message.0.clone(),
        )
    }
}

/// The evidence names a live, visible artifact version with exactly that content.
fn evidence_visible(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    r: &ArtifactRef,
) -> Result<bool, StorageError> {
    let viewer = Viewer::resolve(tx.authority(), ctx)?;
    let read = tx.artifacts();
    let Some(a) = read.artifact(&r.artifact_id)? else {
        return Ok(false);
    };
    if a.state != ArtifactState::Live || !visible(read, &viewer, &a)? {
        return Ok(false);
    }
    Ok(read.version(&r.version_id)?.is_some_and(|v| {
        v.live()
            && v.artifact_id == a.id
            && v.blob_digest == String::from(r.sha256.clone())
            && v.byte_count == r.byte_length.get()
            && v.media_type == r.media_type
    }))
}

impl CommandHandler for DiagnosticService {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let correlation = CorrelationId(tx.new_id(IdKind::Correlation));
        let fault = |code, retry| fault_of(code, retry, &correlation);
        let OperationInput::Workspace(Input::RecordDiagnostic(i)) = command.input() else {
            return Ok(refused(fault(
                DomainErrorCode::UnsupportedCapability,
                RetryAdvice::Never,
            )));
        };
        let origin = if ctx.principal_kind() == PrincipalKind::Host {
            DiagnosticOrigin::Host
        } else {
            DiagnosticOrigin::Reported
        };
        if matches!(origin, DiagnosticOrigin::Reported) && reserved(&i.component.0) {
            let mut f = fault(DomainErrorCode::PermissionDenied, RetryAdvice::Never);
            f.target_kind = Some(BoundedText(RESERVED_TARGET.into()));
            return Ok(refused(f));
        }
        if audience(tx.authority(), ctx)?.is_none() {
            return Ok(refused(fault(
                DomainErrorCode::PermissionDenied,
                RetryAdvice::Never,
            )));
        }
        let entry = reported(i, ctx);
        if check_entry(&entry).is_err() {
            return Ok(refused(fault(
                DomainErrorCode::InvalidArgument,
                RetryAdvice::Never,
            )));
        }
        if let Some(r) = &i.evidence
            && !evidence_visible(tx, ctx, r)?
        {
            let mut f = fault(DomainErrorCode::NotFound, RetryAdvice::Never);
            f.target_kind = Some(BoundedText("artifact".into()));
            f.target_id = Some(r.artifact_id.clone());
            return Ok(refused(f));
        }
        let id = tx.new_id(IdKind::Diagnostic);
        let record = self.record(id, tx.now_ms(), ctx.principal().clone(), origin, &entry);
        let stored = tx.diagnostics().append_diagnostic(&record)?;
        Ok(Decided {
            decision: DomainDecision::Applied {
                result: OperationResult::WorkspaceRecordDiagnostic {
                    value: self::entry(&stored)?,
                },
            },
            operation_id: None,
        })
    }
}

/// [`HealthState`] as a severity.
pub fn severity_of(h: &HealthState) -> Severity {
    match h {
        HealthState::Healthy => Severity::Info,
        HealthState::Degraded | HealthState::Unavailable => Severity::Warning,
        HealthState::Failed => Severity::Error,
    }
}
