//! Versioned convention metadata and retrieval indexes. Each revision is immutable; the
//! current head moves by CAS. Scope filters precede paging and retrieval ranking.
use super::{KnowledgeScope, StorageError};
use crate::protocol::{ActorId, Id};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct StoredConvention {
    pub id: Id,
    pub scope: KnowledgeScope,
    pub revision: i64,
    pub slug: String,
    pub title: String,
    pub parent_id: Option<Id>,
    pub sort_order: i64,
    /// Active, Retired or Deleted.
    pub lifecycle: String,
    pub content_ref: Option<Value>,
    pub raw_content_ref: Option<Value>,
    pub applicability: Vec<String>,
    pub provenance: Value,
    pub author_id: ActorId,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub legacy_approved: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredConventionResource {
    pub id: Id,
    pub convention_id: Id,
    pub revision: i64,
    /// Exact owner revision that created this resource snapshot.
    pub convention_revision: i64,
    pub name: String,
    /// Exact original resource media type, up to the retained source bound of 200 scalars.
    pub media_type: String,
    pub content_ref: Value,
    pub author_id: ActorId,
    pub created_at_ms: i64,
    pub deleted_at_ms: Option<i64>,
    /// Active, Retired or Deleted.
    pub lifecycle: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ConventionOrder {
    #[default]
    Id,
    Title,
    Organization,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConventionPosition {
    pub sort_order: i64,
    pub title: String,
    pub id: Id,
}
impl StoredConvention {
    pub fn position(&self) -> ConventionPosition {
        ConventionPosition {
            sort_order: self.sort_order,
            title: self.title.clone(),
            id: self.id.clone(),
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConventionQuery {
    pub scopes: Vec<KnowledgeScope>,
    pub order: ConventionOrder,
    pub lifecycles: Vec<String>,
    /// All requested applicability labels must be present; an empty filter matches all.
    pub applicability: Vec<String>,
    /// None leaves parents unfiltered; Some(None) selects roots.
    pub parent_id: Option<Option<Id>>,
    pub slug: Option<String>,
    /// Literal case-folded title substring, rather than caller-supplied SQL/FTS syntax.
    pub title: Option<String>,
    /// Scoped source-compatible LIKE pattern over folded slug OR title (% and _ wildcards).
    pub name_pattern: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConventionResourceQuery {
    pub scopes: Vec<KnowledgeScope>,
    pub convention_id: Option<Id>,
    /// Requires convention_id. None lists current resource associations.
    pub convention_revision: Option<i64>,
    /// Empty matches all lifecycle states.
    pub lifecycles: Vec<String>,
}

pub trait ConventionRead: ConventionIndexRead + ConventionImportRead {
    /// The current head and the requested historical snapshot must both have an allowed scope.
    fn convention(
        &self,
        id: &Id,
        revision: Option<i64>,
        scopes: &[KnowledgeScope],
    ) -> Result<Option<StoredConvention>, StorageError>;
    /// Selected-order keyset page, exclusive after; limit 1..=MAX_PAGE.
    fn conventions(
        &self,
        query: &ConventionQuery,
        after: Option<&ConventionPosition>,
        limit: u16,
    ) -> Result<Vec<StoredConvention>, StorageError>;
    /// Descending immutable revision page, with current and historical scope filtering.
    fn convention_versions(
        &self,
        id: &Id,
        scopes: &[KnowledgeScope],
        before_revision: Option<i64>,
        limit: u16,
    ) -> Result<Vec<StoredConvention>, StorageError>;
    fn convention_resource(
        &self,
        convention: &Id,
        resource: &Id,
        convention_revision: Option<i64>,
        scopes: &[KnowledgeScope],
    ) -> Result<Option<StoredConventionResource>, StorageError>;
    fn convention_resources(
        &self,
        query: &ConventionResourceQuery,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredConventionResource>, StorageError>;
    /// Positive scope generation; the absent scope has revision one.
    fn convention_scope_revision(&self, scope: &KnowledgeScope) -> Result<i64, StorageError>;
}

pub trait ConventionRepository:
    ConventionRead + ConventionIndexRepository + ConventionImportRepository
{
    fn create_convention(&mut self, row: &StoredConvention) -> Result<(), StorageError>;
    fn revise_convention(
        &mut self,
        row: &StoredConvention,
        expected: i64,
    ) -> Result<(), StorageError>;
    fn add_convention_resource(
        &mut self,
        row: &StoredConventionResource,
        expected_convention: i64,
    ) -> Result<(), StorageError>;
    fn revise_convention_resource(
        &mut self,
        row: &StoredConventionResource,
        expected_resource: i64,
        expected_convention: i64,
    ) -> Result<(), StorageError>;
    fn bump_convention_scope(&mut self, scope: &KnowledgeScope) -> Result<i64, StorageError>;
}

/// Immutable embedding-space identity captured from the actual embedding route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredConventionEmbeddingSpace {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub configuration_digest: String,
    pub dimensions: u32,
    pub metric: String,
    pub element_type: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredConventionIndex {
    pub generation_id: Id,
    pub convention_id: Id,
    pub convention_revision: i64,
    /// Exact immutable authored body ArtifactRef, including version identity.
    pub source_ref: Value,
    pub operation_id: crate::protocol::OperationId,
    /// Draft or Published. Source fields remain immutable in either state.
    pub state: String,
    /// Queued, Indexing, Ready, LexicalOnly, Stale, Failed or Cancelled.
    pub status: String,
    pub error: Option<String>,
    pub embedding_space: Option<String>,
    pub source_digest: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConventionTerm {
    pub term: String,
    pub frequency: u32,
}
/// Root title is inline; H1-H3 headings always retain exact byte spans in source_ref.
/// A child may cache its exact source segment once when Content-valid and at most 64KiB
/// UTF-8; the publisher verifies that slice against the immutable source outside the writer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredConventionSection {
    pub generation_id: Id,
    pub id: Id,
    pub parent_id: Option<Id>,
    pub heading: Option<String>,
    pub level: u8,
    pub source_offset: Option<i64>,
    pub source_length: Option<i64>,
    /// Own segment tokens only, never repeated ancestor tokens.
    pub token_count: i64,
}
#[derive(Clone, Debug, PartialEq)]
pub struct StoredConventionChunk {
    pub generation_id: Id,
    pub id: Id,
    pub convention_id: Id,
    pub convention_revision: i64,
    pub index: i64,
    pub section_id: Id,
    pub text: String,
    /// Body tokens only. Effective length adds the at-most-four section ancestors.
    pub token_count: i64,
    pub vector: Option<Vec<f32>>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConventionChunkMatch {
    pub generation_id: Id,
    pub id: Id,
    pub convention_id: Id,
    pub convention_revision: i64,
    pub index: i64,
    pub section_id: Id,
    pub source_digest: String,
    /// At most 360 Unicode scalars, without materializing full ranked bodies.
    pub excerpt: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConventionLexicalStatistics {
    pub documents: u64,
    pub total_tokens: u64,
    pub document_frequencies: Vec<ConventionTerm>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ConventionWeightedTerm {
    pub term: String,
    pub idf: f64,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ConventionRankedChunk {
    pub chunk: ConventionChunkMatch,
    pub lexical_score: f64,
    pub dense_score: f64,
    pub score: f64,
}

/// Exact private embedding-input artifact identity, independent of mutable provider results.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredConventionEmbeddingInputs {
    pub request_id: Id,
    pub actor_id: ActorId,
    pub project_id: Option<crate::protocol::ProjectId>,
    pub context_digest: String,
    pub embedding_space: String,
    /// SHA256(JCS(ordered [{sha256,byteCount}])) from exact UTF-8 input bytes.
    pub input_digest: String,
    pub input_refs: Vec<Value>,
    pub created_at_ms: i64,
}

pub trait ConventionIndexRead {
    fn convention_section(
        &self,
        generation: &Id,
        section: &Id,
    ) -> Result<Option<StoredConventionSection>, StorageError>;
    fn convention_sections(
        &self,
        generation: &Id,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredConventionSection>, StorageError>;
    fn convention_chunk(
        &self,
        generation: &Id,
        chunk: &Id,
    ) -> Result<Option<StoredConventionChunk>, StorageError>;
    fn convention_chunk_count(&self, generation: &Id) -> Result<u64, StorageError>;
    /// Current eligible pending draft, newest first; otherwise the published head's current validity.
    fn convention_index_status(
        &self,
        convention: &Id,
    ) -> Result<Option<StoredConventionIndex>, StorageError>;
    /// Positive monotonic generation of the exact requested scopes, including publication/status changes.
    fn convention_index_revision(&self, scopes: &[KnowledgeScope]) -> Result<i64, StorageError>;
    fn convention_embedding_inputs(
        &self,
        request: &Id,
    ) -> Result<Option<StoredConventionEmbeddingInputs>, StorageError>;
    fn convention_embedding_space(
        &self,
        id: &str,
    ) -> Result<Option<StoredConventionEmbeddingSpace>, StorageError>;
    fn convention_index_head(
        &self,
        convention: &Id,
    ) -> Result<Option<StoredConventionIndex>, StorageError>;
    fn convention_index_generation(
        &self,
        generation: &Id,
    ) -> Result<Option<StoredConventionIndex>, StorageError>;
    fn convention_index_generations(
        &self,
        convention: Option<&Id>,
        operation: Option<&crate::protocol::OperationId>,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredConventionIndex>, StorageError>;
    /// Chunk index order, strictly after the cursor; trusted index worker/history reads.
    fn convention_chunks(
        &self,
        generation: &Id,
        after_index: Option<i64>,
        limit: u16,
    ) -> Result<Vec<StoredConventionChunk>, StorageError>;
    /// Full scoped current-active corpus statistics, before any candidate truncation.
    fn convention_lexical_statistics(
        &self,
        query: &ConventionQuery,
        terms: &[String],
    ) -> Result<ConventionLexicalStatistics, StorageError>;
    /// Frozen BM25 (k1=1.2,b=.75); positive dense top 4*K; union with ALL lexical matches.
    /// Final score=.35*lexical/full_corpus_max+.65*dense, including lexical-only fallback.
    /// Scope/current-version filters precede statistics/ranking. Limit is 1..=50; ties use
    /// current title (binary Unicode), then stable convention/generation/chunk identity.
    fn convention_ranked_chunks(
        &self,
        query: &ConventionQuery,
        terms: &[ConventionWeightedTerm],
        average_length: f64,
        space: Option<&str>,
        vector: Option<&[f32]>,
        limit: u16,
    ) -> Result<Vec<ConventionRankedChunk>, StorageError>;
}

pub trait ConventionIndexRepository: ConventionIndexRead {
    fn stage_convention_section(
        &mut self,
        section: &StoredConventionSection,
    ) -> Result<(), StorageError>;
    /// SHA256(JCS(ordered ConventionTerm objects)); additive TF, exact batch replay is a no-op.
    /// Each batch contains at most 500 actual tokens. Term strings are interned once/generation.
    fn stage_convention_section_terms(
        &mut self,
        generation: &Id,
        section: &Id,
        batch_index: i64,
        digest: &str,
        terms: &[ConventionTerm],
    ) -> Result<(), StorageError>;
    fn stage_convention_chunk_terms(
        &mut self,
        generation: &Id,
        chunk: &Id,
        batch_index: i64,
        digest: &str,
        terms: &[ConventionTerm],
    ) -> Result<(), StorageError>;
    /// Returns the winning immutable artifact refs for an exact request/context/space/batch.
    /// Newly staged equivalent UUID refs do not replace a prior winner. Caller retires its
    /// unused candidate refs atomically; this stores no provider outcome or dispatch state.
    fn insert_convention_embedding_inputs(
        &mut self,
        inputs: &StoredConventionEmbeddingInputs,
    ) -> Result<StoredConventionEmbeddingInputs, StorageError>;
    fn put_convention_embedding_space(
        &mut self,
        space: &StoredConventionEmbeddingSpace,
    ) -> Result<(), StorageError>;
    /// Immutable source identity, exact real conventions.index operation, idempotent retry.
    fn stage_convention_index(&mut self, index: &StoredConventionIndex)
    -> Result<(), StorageError>;
    fn stage_convention_chunks(
        &mut self,
        generation: &Id,
        chunks: &[StoredConventionChunk],
    ) -> Result<(), StorageError>;
    fn set_convention_index_status(
        &mut self,
        generation: &Id,
        operation: &crate::protocol::OperationId,
        expected_status: &str,
        status: &str,
        error: Option<&str>,
    ) -> Result<(), StorageError>;
    /// Rechecks actual current owner/source/live artifact and real operation, then
    /// flips one head atomically. Ready requires complete vectors; otherwise LexicalOnly.
    fn publish_convention_index(
        &mut self,
        generation: &Id,
        expected_owner_revision: i64,
        expected_head: Option<&Id>,
        expected_chunk_count: i64,
    ) -> Result<(), StorageError>;
    /// Explicit bounded cleanup; current heads are refused. Keeps the immutable source header
    /// and authored artifact refs; no guessed retention or artifact deletion is performed here.
    fn discard_convention_index_chunks(
        &mut self,
        generation: &Id,
        operation: &crate::protocol::OperationId,
        limit: u16,
    ) -> Result<bool, StorageError>;
}

/// A new-only portable import. Its prospective catalog/artifact rows stay private until publication.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredConventionImport {
    pub operation_id: crate::protocol::OperationId,
    pub convention_id: Id,
    pub scope: KnowledgeScope,
    pub author_id: ActorId,
    pub source_ref: Value,
    pub expected_scope_revision: i64,
    pub revision: i64,
    /// Queued, Parsing, Ready, Published, Failed or Cancelled.
    pub phase: String,
    /// Closed core-owned resumable parser locator/checkpoint, never an authority context.
    pub checkpoint: Value,
    pub processed_resources: i64,
    pub total_resources: Option<i64>,
    pub error: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredConventionImportArtifact {
    pub operation_id: crate::protocol::OperationId,
    pub ordinal: i64,
    /// Markdown, Raw or Resource. A document has one Markdown and at most one Raw source.
    pub kind: String,
    /// Fresh native resource identity, never a manifest-selected existing identity.
    pub resource_id: Option<Id>,
    /// Portable source entry key, unique within the import; never a native identity.
    pub resource_key: Option<String>,
    pub resource_name: Option<String>,
    pub resource_lifecycle: Option<String>,
    /// Original source media type (up to 200 scalars); native ArtifactRef media type keeps
    /// its existing bound and uses an honest binary fallback when necessary.
    pub resource_media_type: Option<String>,
    pub artifact: super::StoredArtifact,
    pub version: super::StoredVersion,
    pub staging_operation_id: crate::protocol::OperationId,
    pub staging_fence: super::AttemptFence,
    pub managed_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConventionImportStorageUsage {
    pub max_entry_bytes: i64,
    pub additional_scope_bytes: i64,
}
pub trait ConventionImportRead {
    /// Distinct prospective blobs not already charged to the target Explicit artifact scope.
    fn convention_import_storage_usage(
        &self,
        operation: &crate::protocol::OperationId,
    ) -> Result<ConventionImportStorageUsage, StorageError>;
    fn convention_import(
        &self,
        operation: &crate::protocol::OperationId,
    ) -> Result<Option<StoredConventionImport>, StorageError>;
    /// Nonpublished active imports and terminal imports with private captures awaiting cleanup.
    fn convention_imports_pending(
        &self,
        after: Option<&crate::protocol::OperationId>,
        limit: u16,
    ) -> Result<Vec<StoredConventionImport>, StorageError>;
    fn convention_import_metadata(
        &self,
        operation: &crate::protocol::OperationId,
    ) -> Result<Option<StoredConvention>, StorageError>;
    fn convention_import_artifacts(
        &self,
        operation: &crate::protocol::OperationId,
        after_ordinal: Option<i64>,
        limit: u16,
    ) -> Result<Vec<StoredConventionImportArtifact>, StorageError>;
}
pub trait ConventionImportRepository: ConventionImportRead {
    fn begin_convention_import(
        &mut self,
        draft: &StoredConventionImport,
    ) -> Result<StoredConventionImport, StorageError>;
    /// The caller binds actual verified staged bytes into blobs first, and resolves the actual
    /// staging journal after this private capture in the same UoW. No public artifacts exist yet.
    fn stage_convention_import_artifact(
        &mut self,
        artifact: &StoredConventionImportArtifact,
    ) -> Result<StoredConventionImport, StorageError>;
    /// Immutable prospective revision one, including the exact captured parent revision.
    fn set_convention_import_metadata(
        &mut self,
        operation: &crate::protocol::OperationId,
        prototype: &StoredConvention,
        expected_parent_revision: Option<i64>,
    ) -> Result<StoredConventionImport, StorageError>;
    fn set_convention_import_phase(
        &mut self,
        operation: &crate::protocol::OperationId,
        expected_revision: i64,
        phase: &str,
        total_resources: Option<i64>,
        error: Option<&str>,
        checkpoint: Option<&Value>,
    ) -> Result<StoredConventionImport, StorageError>;
    /// One atomic publication of all private artifact/version rows and convention/resources.
    /// No resource-count cap, no full resource Vec, and no file I/O inside the writer.
    fn publish_convention_import(
        &mut self,
        operation: &crate::protocol::OperationId,
        expected_revision: i64,
        expected_scope_revision: i64,
        expected_resource_count: i64,
    ) -> Result<StoredConvention, StorageError>;
    /// Failed/cancelled imports only. Caller releases the removed private blob references
    /// through the existing lifecycle collector; caller-owned source artifacts are untouched.
    fn discard_convention_import_artifacts(
        &mut self,
        operation: &crate::protocol::OperationId,
        limit: u16,
    ) -> Result<bool, StorageError>;
}
