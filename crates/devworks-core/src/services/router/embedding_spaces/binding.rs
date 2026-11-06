use crate::context::InvocationContext;
use crate::ports::storage::{
    AttemptFence, StoredConnection, StoredEmbeddingCandidate, StoredEmbeddingProbe,
};
use crate::protocol::data::EmbeddingSpace;
use crate::protocol::{Id, OperationId};
use crate::providers::capabilities::EmbeddingDimensions;

/// Current metadata, never an inference permission or a caller-constructible registration.
#[derive(Clone, Debug)]
pub struct QualifiedEmbeddingBinding {
    pub(super) candidate: StoredEmbeddingCandidate,
    pub(super) qualification_revision: i64,
    pub(super) connection: StoredConnection,
    pub(super) space: EmbeddingSpace,
    pub(super) source_digest: String,
}
impl QualifiedEmbeddingBinding {
    pub fn candidate_id(&self) -> &Id {
        &self.candidate.id
    }
    pub fn candidate_revision(&self) -> i64 {
        self.qualification_revision
    }
    pub fn qualification_revision(&self) -> i64 {
        self.qualification_revision
    }
    pub fn connection(&self) -> &StoredConnection {
        &self.connection
    }
    pub fn space(&self) -> &EmbeddingSpace {
        &self.space
    }
    pub fn upstream_model_id(&self) -> &str {
        &self.candidate.upstream_model_id
    }
    pub fn configuration_digest(&self) -> &str {
        &self.candidate.configuration_digest
    }
    pub fn source_digest(&self) -> &str {
        &self.source_digest
    }
    pub fn dimension_mode(&self) -> EmbeddingDimensions {
        if self.candidate.dimension_mode == "Parameter" {
            EmbeddingDimensions::Parameter
        } else {
            EmbeddingDimensions::Fixed(self.space.dimensions)
        }
    }
}

/// Includes an explicit unconfigured/unavailable witness, so A→B→A cannot revive old work.
#[derive(Clone, Debug)]
pub struct ResolvedEmbeddingConfiguration {
    pub(super) source_digest: String,
    pub(super) reason: Option<String>,
    pub(super) binding: Option<QualifiedEmbeddingBinding>,
}
impl ResolvedEmbeddingConfiguration {
    pub fn source_digest(&self) -> &str {
        &self.source_digest
    }
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }
    pub fn binding(&self) -> Option<&QualifiedEmbeddingBinding> {
        self.binding.as_ref()
    }
}

/// Host-prepared candidate probe. Deliberately not Deserialize and has no public constructor.
#[derive(Clone, Debug)]
pub struct PreparedEmbeddingProbe {
    pub(super) candidate: StoredEmbeddingCandidate,
    pub(super) probe: StoredEmbeddingProbe,
    pub(super) connection: StoredConnection,
    pub(super) space: EmbeddingSpace,
    pub(super) context: InvocationContext,
    pub(super) operation_fence: AttemptFence,
}
impl PreparedEmbeddingProbe {
    pub fn candidate_id(&self) -> &Id {
        &self.candidate.id
    }
    pub fn candidate_revision(&self) -> i64 {
        self.probe.candidate_revision
    }
    pub fn connection(&self) -> &StoredConnection {
        &self.connection
    }
    pub fn space(&self) -> &EmbeddingSpace {
        &self.space
    }
    pub fn upstream_model_id(&self) -> &str {
        &self.candidate.upstream_model_id
    }
    pub fn configuration_digest(&self) -> &str {
        &self.candidate.configuration_digest
    }
    pub fn operation_id(&self) -> &OperationId {
        &self.probe.operation_id
    }
    pub fn operation_fence(&self) -> &AttemptFence {
        &self.operation_fence
    }
    pub fn delivery_id(&self) -> &Id {
        &self.probe.delivery_id
    }
    pub fn request_id(&self) -> &Id {
        &self.probe.request_id
    }
    pub fn admission_key(&self) -> &Id {
        &self.probe.admission_key
    }
    pub fn context(&self) -> &InvocationContext {
        &self.context
    }
    pub fn dimension_mode(&self) -> EmbeddingDimensions {
        if self.candidate.dimension_mode == "Parameter" {
            EmbeddingDimensions::Parameter
        } else {
            EmbeddingDimensions::Fixed(self.space.dimensions)
        }
    }
}

#[derive(Clone, Debug)]
pub enum ProbeOutcome {
    Completed,
    Failed(String),
    Unknown(String),
    Cancelled(String),
}
#[derive(Clone, Debug)]
pub enum ProbePreparation {
    Ready(Box<PreparedEmbeddingProbe>),
    Terminal(ProbeOutcome),
    Waiting(String),
}
