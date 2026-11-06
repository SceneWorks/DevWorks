//! Immutable embedding identities and host-owned qualification (DW-119).
//! Candidate configuration is never edited or relabelled. Qualification and explicit selection
//! have independent revisions. No storage method performs provider work or reads vector bytes.
use super::{AttemptFence, StorageError};
use crate::ports::AuthorityFence;
use crate::protocol::data::DataScope;
use crate::protocol::provider::ArtifactRef;
use crate::protocol::{ActorId, Id, OperationId, StoreId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredEmbeddingCandidate {
    pub id: Id,
    pub name: String,
    pub space_id: Id,
    pub connection_id: Id,
    pub account_revision: i64,
    pub account_digest: Option<String>,
    pub endpoint: String,
    pub endpoint_digest: String,
    pub upstream_model_id: String,
    /// Native or Parameter; never inferred from an endpoint hostname.
    pub dimension_mode: String,
    pub dimensions: i64,
    /// L2 or Cosine.
    pub metric: String,
    /// Float32.
    pub element_type: String,
    /// Exact host encoder configuration; no credentials or mutable discovery/pricing fields.
    pub encoder_config: Value,
    pub configuration_digest: String,
    pub model_digest: String,
    pub created_by: ActorId,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredEmbeddingProbe {
    pub operation_id: OperationId,
    pub delivery_id: Id,
    pub request_id: Id,
    pub admission_key: Id,
    pub candidate_revision: i64,
    pub store_id: StoreId,
    pub restore_epoch: i64,
    pub actor_id: ActorId,
    pub scope: DataScope,
    pub connection_revision: i64,
    /// Versioned original source context, without grants. Never accepted from a wire caller.
    pub source_context: Value,
    pub authority_fences: Vec<AuthorityFence>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredEmbeddingEvidence {
    pub request_id: Id,
    /// The native router request owns a separate operation from the qualification effect.
    pub native_operation_id: OperationId,
    pub attempt_id: Id,
    pub result_artifact: ArtifactRef,
    /// Exactly one finite, dimension-checked probe vector, verified off the writer.
    pub vector_artifact: ArtifactRef,
    pub reported_model: Option<String>,
    pub registration_digest: String,
    pub connection_revision: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EmbeddingQualificationState {
    Unqualified,
    Qualifying,
    Qualified,
    Failed,
    Unknown,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredEmbeddingQualification {
    pub candidate_id: Id,
    pub revision: i64,
    pub state: EmbeddingQualificationState,
    pub reason: Option<String>,
    pub probe: Option<StoredEmbeddingProbe>,
    pub evidence: Option<StoredEmbeddingEvidence>,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredEmbeddingSelection {
    /// Implicit initial state is revision 1 and no selection.
    pub revision: i64,
    pub candidate_id: Option<Id>,
    pub space_id: Option<Id>,
}

pub trait EmbeddingRead {
    /// Bounded durable change fence for inventory cursors, including status dependencies that
    /// can change without a history append. Independent of the selection/candidate revisions.
    fn embedding_inventory_revision(&self) -> Result<i64, StorageError>;
    fn embedding_candidate(
        &self,
        id: &Id,
    ) -> Result<Option<StoredEmbeddingCandidate>, StorageError>;
    /// Exclusive candidate-ID order; limit 1..=MAX_PAGE, never loads vector bytes.
    fn embedding_candidates(
        &self,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredEmbeddingCandidate>, StorageError>;
    fn embedding_qualification(
        &self,
        id: &Id,
    ) -> Result<Option<StoredEmbeddingQualification>, StorageError>;
    fn embedding_selection(&self) -> Result<StoredEmbeddingSelection, StorageError>;
}

pub trait EmbeddingRepository: EmbeddingRead {
    /// Inserts a new immutable candidate plus its Unqualified revision-1 state. Duplicate IDs
    /// are refused. Connection revision and exact captured identity are checked atomically.
    fn insert_embedding_candidate(
        &mut self,
        candidate: &StoredEmbeddingCandidate,
        expected_connection_revision: i64,
    ) -> Result<(), StorageError>;
    /// Binds newly minted durable intent once, before provider dispatch. An unresolved previous
    /// operation cannot be replaced. Unknown work is reconciled, never automatically retried.
    fn start_embedding_qualification(
        &mut self,
        candidate_id: &Id,
        expected_revision: i64,
        probe: &StoredEmbeddingProbe,
    ) -> Result<StoredEmbeddingQualification, StorageError>;
    /// Stores the host-verified native evidence while the real operation is Dispatching, or on exact same-attempt Unknown/NeedsInput recovery. The
    /// latest fence and native journal/artifact metadata are rechecked. Repeated identical
    /// staging is allowed only at the current revision; changed evidence is refused.
    fn stage_embedding_probe_evidence(
        &mut self,
        candidate_id: &Id,
        expected_revision: i64,
        operation_id: &OperationId,
        fence: &AttemptFence,
        evidence: &StoredEmbeddingEvidence,
    ) -> Result<StoredEmbeddingQualification, StorageError>;
    /// The host verifies raw manifest/vector content before calling this method. The writer
    /// rechecks the exact journal fence, original store/authority, current candidate source,
    /// usage attempt, quiescent balance and artifact metadata. Qualified requires real terminal
    /// ObservedComplete; failed/unknown/cancelled work never creates a vector space.
    fn finish_embedding_qualification(
        &mut self,
        candidate_id: &Id,
        expected_revision: i64,
        operation_id: &OperationId,
        fence: &AttemptFence,
        state: EmbeddingQualificationState,
        reason: Option<&str>,
    ) -> Result<StoredEmbeddingQualification, StorageError>;
    /// CAS both selection and candidate qualification. None clears explicitly; Some selects
    /// only current verified qualification. Does not rewrite any historical vector identity.
    fn select_embedding_candidate(
        &mut self,
        expected_selection_revision: i64,
        candidate: Option<(&Id, i64)>,
    ) -> Result<StoredEmbeddingSelection, StorageError>;
}
