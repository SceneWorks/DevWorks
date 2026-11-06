//! Immutable eval reports, stored separately from the session and the work they measure.
use super::StorageError;
use crate::protocol::{Id, ProjectId};
use serde_json::Value;

/// Maximum serialized report size admitted by the repository.
pub const MAX_EVAL_REPORT_BYTES: usize = 1_048_576;

/// A report binds its measurement definition to the exact source snapshot it graded.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredEvalRun {
    pub id: Id,
    pub suite_id: Id,
    pub suite_revision: i64,
    pub project_id: Option<ProjectId>,
    pub session_id: Id,
    pub session_revision: i64,
    /// Last durable transcript sequence captured by the grading snapshot.
    pub session_sequence: i64,
    /// Lowercase SHA-256 over the measurement definition.
    pub measurement_digest: String,
    pub created_at_ms: i64,
    pub source_watermark: i64,
    pub report: Value,
}

pub trait EvalRead {
    fn eval_run(&self, id: &Id) -> Result<Option<StoredEvalRun>, StorageError>;
    /// Ascending opaque ID order, exclusive cursor; limit is `1..=MAX_PAGE`.
    fn eval_runs(
        &self,
        session_id: &Id,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredEvalRun>, StorageError>;
}

pub trait EvalRepository: EvalRead {
    /// Inserts once; duplicate identities never replace or amend a report.
    fn insert_eval_run(&mut self, run: &StoredEvalRun) -> Result<(), StorageError>;
}
