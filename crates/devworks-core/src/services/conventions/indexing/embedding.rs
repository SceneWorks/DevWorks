//! Trusted embedding execution for convention indexing and retrieval. Implementations must
//! use the actual routed/accounted producer, retain exact input identities for replay, and
//! never turn an unavailable/unknown/cancelled response into a successful vector.
use crate::{
    context::InvocationContext,
    ports::{
        AuthorityFence,
        storage::{Cancellation, StorageError, StoredConventionEmbeddingSpace},
    },
    protocol::{Id, OperationId, provider::ArtifactRef},
};

#[derive(Clone, Debug)]
pub enum EmbeddingOrigin {
    Query {
        query_id: Id,
        fingerprint: String,
        scope_digest: String,
    },
    Index {
        operation_id: OperationId,
        generation_id: Id,
        source_digest: String,
        first_chunk: i64,
    },
}
#[derive(Clone, Debug)]
pub struct EmbeddingRequest {
    /// Host-derived from exact origin/context/space/input identity, never an authority token.
    pub request_id: Id,
    pub origin: EmbeddingOrigin,
    pub space: StoredConventionEmbeddingSpace,
    /// Current selected-configuration witness, including unavailable/selection revisions.
    pub configuration_source_digest: String,
    /// The real derived-input preparation operation, bound to the original caller.
    pub preparation_operation: OperationId,
    pub input_refs: Vec<ArtifactRef>,
    pub fences: Vec<AuthorityFence>,
    pub cancellation: Cancellation,
    /// Absolute store-clock deadline; the executor also establishes a monotonic bound.
    pub deadline_ms: Option<i64>,
}
#[derive(Clone, Debug)]
pub struct EmbeddedBatch {
    pub request_id: Id,
    pub space: StoredConventionEmbeddingSpace,
    pub vectors: Vec<Vec<f32>>,
    pub attempt_id: Id,
}
#[derive(Debug)]
pub enum EmbeddingFailure {
    Unavailable(String),
    Unknown { request_id: Id, reason: String },
    NotStarted { request_id: Id },
    Cancelled,
    Storage(StorageError),
}
impl From<StorageError> for EmbeddingFailure {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::Cancelled => Self::Cancelled,
            error => Self::Storage(error),
        }
    }
}
#[derive(Clone, Debug)]
pub struct EmbeddingConfiguration {
    pub source_digest: String,
    pub space: Option<StoredConventionEmbeddingSpace>,
    pub reason: Option<String>,
}
pub trait ConventionEmbeddings: Send + Sync {
    /// Only the explicitly configured, currently compatible embedding space. No conversational
    /// model, first route, unqualified vector dimension or automatic paid default is allowed.
    fn configured_space(
        &self,
        context: &InvocationContext,
    ) -> Result<EmbeddingConfiguration, EmbeddingFailure>;
    /// Called outside any SQL snapshot/writer. At most 32 exact text inputs per native request.
    fn embed(
        &self,
        context: &InvocationContext,
        request: EmbeddingRequest,
    ) -> Result<EmbeddedBatch, EmbeddingFailure>;
    /// Observe the exact original native journal only; absence is NotStarted, never a send.
    fn observe(
        &self,
        context: &InvocationContext,
        request: EmbeddingRequest,
    ) -> Result<EmbeddedBatch, EmbeddingFailure>;
}
