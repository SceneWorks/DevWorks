//! Retained diagnostic log records (LOG-01..03): the durable channel host work, the UI, agent
//! tools and packages write diagnostics to, kept apart from history events, transcripts and
//! model history.
//!
//! Every record carries a store-assigned `sequence` (strictly increasing, never reused), so a
//! reader tails the log by "strictly after sequence N" and a missed live hint is repaired by the
//! same durable query. The repository enforces record invariants (bounded text, ordered
//! lifecycle times and progress, JSON arrays for metrics and recovery links) and bounded
//! listings; attribution, redaction, output bounds, reserved components and visibility belong to
//! `crate::services::diagnostics`, which writes inside a unit of work.
use super::StorageError;
use crate::protocol::{ActorId, Id, OperationId, ProjectId};

/// Severity ranks, lowest first (`Info` < `Warning` < `Error`).
pub const SEVERITY_RANKS: [&str; 3] = ["Info", "Warning", "Error"];

/// The rank of a stored severity name, if it is one.
pub fn severity_rank(name: &str) -> Option<u8> {
    SEVERITY_RANKS
        .iter()
        .position(|s| *s == name)
        .map(|i| i as u8)
}

/// One retained diagnostic. Enumerated members are stored by their wire names
/// (`severity`: `Info`/`Warning`/`Error`; `outcome`, `error_class`, `origin` likewise).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredDiagnostic {
    /// Store-assigned, strictly increasing; 0 in a record that was not appended yet.
    pub sequence: i64,
    pub id: Id,
    pub recorded_at_ms: i64,
    pub severity: String,
    pub component: String,
    pub code: String,
    /// Redacted and bounded.
    pub message: String,
    /// `Host` or `Reported`.
    pub origin: String,
    pub actor: ActorId,
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
    pub progress_completed: Option<i64>,
    pub progress_total: Option<i64>,
    pub outcome: Option<String>,
    pub error_class: Option<String>,
    /// Redacted, bounded helper output (the retained tail).
    pub output: Option<String>,
    /// Output bytes not retained (bounded away, or withheld because they may hold source rows).
    pub output_omitted_bytes: i64,
    /// Credential occurrences the host replaced before persistence.
    pub redactions: i64,
    /// The evidence artifact reference (canonical JSON of the wire `ArtifactRef`).
    pub evidence_json: Option<String>,
    /// JSON array of wire `DiagnosticMetric`s.
    pub metrics_json: String,
    /// JSON array of wire `RecoveryAction`s.
    pub recovery_json: String,
}

/// Which records a project-bound reader may see.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DiagnosticAudience {
    /// Every record (the user, the host and workspace-wide principals).
    #[default]
    All,
    /// Records of this project and records that belong to no project.
    Project(ProjectId),
}

/// A bounded listing of retained diagnostics. Every member narrows the result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiagnosticFilter {
    /// Records strictly after this sequence.
    pub after_sequence: i64,
    /// Records strictly before this sequence (a descending page's continuation).
    pub before_sequence: Option<i64>,
    /// Newest first instead of oldest first.
    pub descending: bool,
    /// Exactly this component, or a component beneath it (`indexing` matches
    /// `indexing.repository`).
    pub component: Option<String>,
    /// Minimum severity rank ([`severity_rank`]).
    pub min_severity: Option<u8>,
    pub outcome: Option<String>,
    pub operation: Option<String>,
    pub operation_id: Option<OperationId>,
    pub correlation_id: Option<Id>,
    pub target_id: Option<Id>,
    pub stage: Option<String>,
    /// Records of exactly this project.
    pub project_id: Option<ProjectId>,
    pub audience: DiagnosticAudience,
    /// Case-insensitive substring of code, message, stage, operation or output.
    pub search: Option<String>,
    /// Recorded at or after.
    pub since_ms: Option<i64>,
    /// Recorded at or before.
    pub until_ms: Option<i64>,
}

/// Size of the retained log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiagnosticStats {
    pub records: i64,
    /// Highest sequence ever assigned (0 before the first record); survives pruning.
    pub latest_sequence: i64,
    pub oldest_recorded_at_ms: Option<i64>,
}

/// What one retention pass removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrunedDiagnostics {
    /// Records older than the retention window.
    pub expired: i64,
    /// The oldest records beyond the record cap.
    pub over_cap: i64,
}

/// The durable intent of one diagnostic export: what a request key exported, and the exact
/// `artifacts.save` command it submitted, so a retry with that key resubmits the same command
/// (the gateway replays its receipt) and a different selection under the key is a conflict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportIntent {
    pub actor: ActorId,
    /// The receipt identity's scope key and request key.
    pub scope_key: String,
    pub request_key: String,
    pub project_id: ProjectId,
    /// SHA-256 of the export's selection and destination.
    pub selection_sha256: String,
    /// The submitted `artifacts.save` command frame (JSON).
    pub command: String,
    pub created_at_ms: i64,
}

pub trait DiagnosticRead {
    fn diagnostic(&self, id: &Id) -> Result<Option<StoredDiagnostic>, StorageError>;
    /// Matching records in sequence order (descending when asked), at most `limit` (1..=500).
    fn diagnostics(
        &self,
        filter: &DiagnosticFilter,
        limit: u16,
    ) -> Result<Vec<StoredDiagnostic>, StorageError>;
    /// How many records match (the same predicate as [`Self::diagnostics`]).
    fn count_diagnostics(&self, filter: &DiagnosticFilter) -> Result<i64, StorageError>;
    fn diagnostic_stats(&self) -> Result<DiagnosticStats, StorageError>;
    /// The export intent of one receipt identity.
    fn export_intent(
        &self,
        actor: &ActorId,
        scope_key: &str,
        request_key: &str,
    ) -> Result<Option<ExportIntent>, StorageError>;
}

pub trait DiagnosticRepository: DiagnosticRead {
    /// Appends `record` (its `sequence` is ignored) and returns it with its assigned sequence.
    /// A duplicate ID is [`StorageError::Duplicate`].
    fn append_diagnostic(
        &mut self,
        record: &StoredDiagnostic,
    ) -> Result<StoredDiagnostic, StorageError>;
    /// Records (or replaces) the export intent of its receipt identity.
    fn put_export_intent(&mut self, intent: &ExportIntent) -> Result<(), StorageError>;
    /// Removes records recorded before `older_than_ms`, then the oldest records beyond
    /// `keep_at_most`, and export intents created before `older_than_ms`. The sequence counter
    /// is never reset.
    fn prune_diagnostics(
        &mut self,
        older_than_ms: i64,
        keep_at_most: i64,
    ) -> Result<PrunedDiagnostics, StorageError>;
}
