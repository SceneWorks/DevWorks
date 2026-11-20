//! Document index metadata and immutable extracted passages. Artifact rows still own the
//! original bytes, versions and access. A checkpoint is separate from a published index.
use super::{ArtifactScope, StorageError};
use crate::protocol::{ActorId, Id, Revision, data::Coverage};
use crate::services::documents::extraction::ExtractedBlock;
use serde::{Deserialize, Serialize};

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
    pub work_limited: bool,
}

pub trait DocumentRead {
    /// Explicit retrieval preference. Live documents default to enabled before their first index.
    fn document_retrieval_enabled(&self, artifact: &Id) -> Result<bool, StorageError>;
    fn document_index(&self, version: &Id) -> Result<Option<DocumentIndexVersion>, StorageError>;
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
