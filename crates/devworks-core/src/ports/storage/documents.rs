//! Document index metadata and immutable extracted passages. Artifact rows still own the
//! original bytes, versions and access. A checkpoint is separate from a published index.
use super::StorageError;
use crate::protocol::{Id, Revision, data::Coverage};
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

pub trait DocumentRead {
    /// Explicit retrieval preference. Live documents default to enabled before their first index.
    fn document_retrieval_enabled(&self, artifact: &Id) -> Result<bool, StorageError>;
    fn document_index(&self, version: &Id) -> Result<Option<DocumentIndexVersion>, StorageError>;
    fn document_checkpoint(
        &self,
        version: &Id,
        revision: &Revision,
    ) -> Result<Option<DocumentCheckpoint>, StorageError>;
    /// Read only after authorizing the exact retained version and checkpoint.
    fn document_passage(
        &self,
        checkpoint: &Id,
        block: &Id,
    ) -> Result<Option<ExtractedBlock>, StorageError>;
}

pub trait DocumentRepository: DocumentRead {
    fn set_document_retrieval(&mut self, artifact: &Id, enabled: bool) -> Result<(), StorageError>;
    fn put_document_index(&mut self, index: &DocumentIndexVersion) -> Result<(), StorageError>;
    fn remove_document_index(&mut self, artifact: &Id) -> Result<bool, StorageError>;
    fn insert_document_checkpoint(
        &mut self,
        checkpoint: &DocumentCheckpoint,
        blocks: &[ExtractedBlock],
    ) -> Result<(), StorageError>;
}
