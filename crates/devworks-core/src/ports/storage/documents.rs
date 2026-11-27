//! Document index metadata and immutable extracted passages. Artifact rows still own the
//! original bytes, versions and access. A checkpoint is separate from a published index.
use super::{ArtifactScope, StorageError};
use crate::kernel::document_evidence::{ExtractedBlock, VisionTarget, VisionUnit};
use crate::protocol::records::DocumentTransferState;
use crate::protocol::{ActorId, AssignmentId, Id, ProjectId, Revision, RootId, data::Coverage};
use serde::{Deserialize, Serialize};

/// The exact project and primary coordinator custody when a transfer was proposed.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct DocumentTransferParty {
    pub project: ProjectId,
    pub archive_epoch: i64,
    pub root: RootId,
    pub assignment: AssignmentId,
    pub assignment_generation: i64,
    pub session: Id,
    pub activation: Id,
    pub activation_owner_epoch: i64,
    pub actor: ActorId,
    pub authority_revision: i64,
}

/// The immutable proposal covers one exact document and two authenticated parties.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct DocumentTransferProposal {
    pub document: Id,
    pub version: Id,
    pub document_revision: Revision,
    pub scope_revision: Revision,
    pub source: DocumentTransferParty,
    pub destination: DocumentTransferParty,
    pub source_needs_document: bool,
}

#[derive(Clone, Debug)]
pub struct StoredDocumentTransfer {
    pub id: Id,
    pub proposal: DocumentTransferProposal,
    pub revision: Revision,
    pub state: DocumentTransferState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum DocumentIndexStage {
    Queued,
    Extracted,
    Ready,
    Partial,
    Failed,
    Cancelled,
    Removed,
}

#[derive(Clone, Debug)]
pub struct DocumentIndexVersion {
    pub version: Id,
    pub latest: Id,
    pub stage: DocumentIndexStage,
    pub diagnostic: Option<Id>,
    /// The published generation still available for this exact version, even after a failed
    /// replacement attempt. Generation and extraction revision are set or cleared together.
    pub published: Option<(Id, Revision)>,
    pub published_at_ms: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct DocumentCheckpoint {
    pub id: Id,
    pub version: Id,
    pub revision: Revision,
    pub source_digest: String,
    pub coverage: Coverage,
    pub blocks: u32,
}

#[derive(Clone, Copy)]
pub struct DocumentListFilter {
    pub retrieval_enabled: Option<bool>,
    /// Older result revisions require a live current version.
    pub current_only: bool,
}

pub struct DocumentRankingQuery<'a> {
    pub scope: &'a ArtifactScope,
    pub owner: Option<&'a ActorId>,
    /// Exact IDs already validated under the current snapshot, or current versions only.
    pub exact_versions: Option<&'a [Id]>,
    /// Safe, quoted FTS queries made from distinct search words.
    pub words: &'a [String],
    pub dense: Option<DocumentVectorQuery<'a>>,
    pub max_blocks: u32,
    pub max_results: u16,
}

/// The exact qualified cosine space and finite, nonzero vector selected for one query.
pub struct DocumentVectorQuery<'a> {
    pub space: &'a Id,
    pub vector: &'a [f32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentEmbeddingRun {
    pub run: Id,
    pub extraction: Id,
    pub space: Option<Id>,
    /// The selected configuration witness from the original preparation, even if unavailable.
    pub configuration_digest: String,
    pub complete: bool,
}

#[derive(Clone, Debug)]
pub struct DocumentCandidate {
    pub checkpoint: Id,
    pub block: Id,
    pub generation: Id,
}

pub struct DocumentRankingBatch {
    pub candidates: Vec<DocumentCandidate>,
    /// A compatible vector was scored inside the bounded eligible prefix.
    pub used_vectors: bool,
    pub work_limited: bool,
}

pub trait DocumentRead {
    fn document_transfer(&self, id: &Id) -> Result<Option<StoredDocumentTransfer>, StorageError>;

    /// Identity of the authorized published source versions. The tool-response writer uses
    /// it to reject a ranking if even a non-returned source lost or gained visibility.
    fn document_corpus_digest(
        &self,
        scope: &ArtifactScope,
        owner: Option<&ActorId>,
        exact_versions: Option<&[Id]>,
    ) -> Result<String, StorageError>;

    /// Explicit retrieval preference. Live documents default to enabled before their first index.
    fn document_retrieval_enabled(&self, artifact: &Id) -> Result<bool, StorageError>;
    fn document_index(&self, version: &Id) -> Result<Option<DocumentIndexVersion>, StorageError>;
    /// Recent exact native scan occurrences and their durable outcomes for a visible version.
    fn document_vision_occurrences(
        &self,
        version: &Id,
        limit: usize,
    ) -> Result<Vec<(VisionTarget, Option<VisionUnit>)>, StorageError>;
    /// Already source-bound vision or embedding attempts in the audience's authorized project.
    fn document_version_attempts(
        &self,
        version: &Id,
        project: Option<&ProjectId>,
        limit: u16,
    ) -> Result<Vec<Id>, StorageError>;
    fn document_checkpoint(
        &self,
        version: &Id,
        revision: &Revision,
    ) -> Result<Option<DocumentCheckpoint>, StorageError>;
    /// Most recent retained extraction for display when this version has no published index.
    fn latest_document_checkpoint(
        &self,
        version: &Id,
    ) -> Result<Option<DocumentCheckpoint>, StorageError>;
    /// Read only after authorizing the exact retained version and checkpoint.
    fn document_passage(
        &self,
        checkpoint: &Id,
        block: &Id,
    ) -> Result<Option<ExtractedBlock>, StorageError>;
    /// Direct passages in their native order for an authorized exact derivative compilation.
    fn document_blocks(&self, checkpoint: &Id) -> Result<Vec<ExtractedBlock>, StorageError>;
    /// Whether this authorized published population has any vectors in the exact space.
    fn has_document_vectors(
        &self,
        scope: &ArtifactScope,
        owner: Option<&ActorId>,
        exact_versions: Option<&[Id]>,
        space: &Id,
    ) -> Result<bool, StorageError>;
    /// Filter the authorized, published exact versions before matching and ranking. Return
    /// identities only. The caller opens selected passages within the same snapshot.
    fn rank_document_blocks(
        &self,
        query: &DocumentRankingQuery<'_>,
    ) -> Result<DocumentRankingBatch, StorageError>;
}

pub trait DocumentRepository: DocumentRead {
    fn insert_document_transfer(
        &mut self,
        id: &Id,
        proposal: &DocumentTransferProposal,
    ) -> Result<(), StorageError>;
    fn resolve_document_transfer(
        &mut self,
        id: &Id,
        expected: &Revision,
        outcome: &DocumentTransferState,
    ) -> Result<(), StorageError>;

    /// A producer's identity is distinct from the immutable citation revision it published.
    fn document_extraction_by_key(
        &self,
        version: &Id,
        key: &str,
    ) -> Result<Option<DocumentCheckpoint>, StorageError>;
    fn latest_document_direct_extraction(
        &self,
        version: &Id,
        algorithm: &Revision,
    ) -> Result<Option<DocumentCheckpoint>, StorageError>;
    fn document_vision_targets(&self, extraction: &Id) -> Result<Vec<VisionTarget>, StorageError>;
    fn next_document_extraction_revision(&self, version: &Id) -> Result<Revision, StorageError>;
    fn bind_document_extraction(
        &mut self,
        extraction: &Id,
        key: &str,
        targets: &[VisionTarget],
    ) -> Result<(), StorageError>;
    fn document_vision_unit(
        &self,
        extraction: &Id,
        target: &Id,
    ) -> Result<Option<VisionUnit>, StorageError>;
    fn put_document_vision_unit(&mut self, unit: &VisionUnit) -> Result<(), StorageError>;
    /// Capture the run's immutable route once, before any native embedding is started.
    fn insert_document_embedding_run(
        &mut self,
        run: &DocumentEmbeddingRun,
    ) -> Result<(), StorageError>;
    fn document_embedding_run(
        &self,
        run: &Id,
    ) -> Result<Option<DocumentEmbeddingRun>, StorageError>;
    /// Up to 32 exact retained blocks missing vectors from the selected run's space.
    fn pending_document_vectors(
        &self,
        run: &DocumentEmbeddingRun,
        limit: u16,
    ) -> Result<Vec<(Id, String)>, StorageError>;
    fn insert_document_vectors(
        &mut self,
        run: &DocumentEmbeddingRun,
        vectors: &[(Id, Vec<f32>)],
    ) -> Result<(), StorageError>;
    /// The full vector population may become visible in the index publication transaction.
    fn complete_document_vectors(&mut self, run: &DocumentEmbeddingRun)
    -> Result<(), StorageError>;
    fn set_document_retrieval(&mut self, artifact: &Id, enabled: bool) -> Result<(), StorageError>;
    fn put_document_index(&mut self, index: &DocumentIndexVersion) -> Result<(), StorageError>;
    fn remove_document_index(&mut self, artifact: &Id) -> Result<bool, StorageError>;
    fn insert_document_checkpoint(
        &mut self,
        checkpoint: &DocumentCheckpoint,
        blocks: &[ExtractedBlock],
    ) -> Result<(), StorageError>;
}
