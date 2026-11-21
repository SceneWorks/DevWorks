//! Exact previews and publication records for operational retention.
use super::StorageError;
use crate::protocol::records::PruneKind;
use crate::protocol::{ActorId, Id, OperationId, StoreId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetentionKind {
    Diagnostics,
    IndexRuns,
    Messages,
    Sessions,
}

impl RetentionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Diagnostics => "Diagnostics",
            Self::IndexRuns => "IndexRuns",
            Self::Messages => "Messages",
            Self::Sessions => "Sessions",
        }
    }
    pub fn operation(self) -> &'static str {
        match self {
            Self::Diagnostics => "maintenance.prune_diagnostics",
            Self::IndexRuns => "maintenance.prune_index_runs",
            Self::Messages => "maintenance.prune_messages",
            Self::Sessions => "maintenance.prune_sessions",
        }
    }
    pub fn wire(self) -> PruneKind {
        match self {
            Self::Diagnostics => PruneKind::Diagnostics,
            Self::IndexRuns => PruneKind::IndexRuns,
            Self::Messages => PruneKind::Messages,
            Self::Sessions => PruneKind::Sessions,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionWindow {
    pub kind: RetentionKind,
    pub requested_cutoff_ms: i64,
    pub effective_cutoff_ms: i64,
    pub captured_at_ms: i64,
    pub limit: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetentionHold {
    AuthoredHistory,
    PublishedEvidence,
    LiveOperation,
    CurrentWitness,
    UnresolvedFailure,
    OpenConversation,
    PendingDelivery,
    HandoverWindow,
    ReplayHead,
    Referenced,
    UnclassifiedReference,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionWitness {
    pub id: Id,
    pub revision: i64,
    pub activity_ms: i64,
    pub hold: Option<RetentionHold>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionSelection {
    pub candidates: Vec<RetentionWitness>,
    pub held: Vec<RetentionWitness>,
    pub more_candidates: bool,
    pub more_held: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledRetention {
    pub job_id: Id,
    pub run_id: Id,
    pub job_revision: i64,
    pub definition_digest: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetentionPhase {
    Previewed,
    Queued,
    Applied,
    Cancelled,
    Refused,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionOutcome {
    pub records_removed: i64,
    pub owned_rows_removed: i64,
    pub settled_at_ms: i64,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredRetention {
    pub id: Id,
    pub revision: i64,
    pub owner: ActorId,
    pub store: StoreId,
    pub restore_epoch: i64,
    pub window: RetentionWindow,
    pub expires_at_ms: i64,
    pub selection: RetentionSelection,
    pub scheduled: Option<ScheduledRetention>,
    pub operation: Option<OperationId>,
    pub phase: RetentionPhase,
    pub outcome: Option<RetentionOutcome>,
}

pub trait RetentionRead {
    fn retention_preview(&self, id: &Id) -> Result<Option<StoredRetention>, StorageError>;
    fn retention_for_job_run(&self, id: &Id) -> Result<Option<StoredRetention>, StorageError>;
    fn retention_for_operation(
        &self,
        id: &OperationId,
    ) -> Result<Option<StoredRetention>, StorageError>;
    fn preview_retention(
        &self,
        window: &RetentionWindow,
    ) -> Result<RetentionSelection, StorageError>;
    /// Reads exactly the named records in their supplied order. An absent record is significant.
    fn retention_witnesses(
        &self,
        window: &RetentionWindow,
        ids: &[Id],
    ) -> Result<Vec<RetentionWitness>, StorageError>;
}

pub trait RetentionRepository: RetentionRead {
    fn put_retention_preview(
        &mut self,
        value: &StoredRetention,
        expected_revision: Option<i64>,
    ) -> Result<(), StorageError>;

    /// Deletes the exact still-eligible witnesses through the existing owners. The caller records
    /// the terminal outcome and consumes its preview in this same write transaction.
    fn apply_retention(
        &mut self,
        window: &RetentionWindow,
        selected: &[RetentionWitness],
    ) -> Result<RetentionOutcome, StorageError>;
}
