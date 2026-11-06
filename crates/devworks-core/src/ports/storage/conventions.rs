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
    pub content_ref: Value,
    pub author_id: ActorId,
    pub created_at_ms: i64,
    pub deleted_at_ms: Option<i64>,
    /// Active, Retired or Deleted.
    pub lifecycle: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConventionQuery {
    pub scopes: Vec<KnowledgeScope>,
    pub lifecycles: Vec<String>,
    /// All requested applicability labels must be present; an empty filter matches all.
    pub applicability: Vec<String>,
    /// None leaves parents unfiltered; Some(None) selects roots.
    pub parent_id: Option<Option<Id>>,
    pub slug: Option<String>,
    /// Literal case-folded title substring, rather than caller-supplied SQL/FTS syntax.
    pub title: Option<String>,
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

pub trait ConventionRead: ConventionIndexRead {
    /// The current head and the requested historical snapshot must both have an allowed scope.
    fn convention(
        &self,
        id: &Id,
        revision: Option<i64>,
        scopes: &[KnowledgeScope],
    ) -> Result<Option<StoredConvention>, StorageError>;
    /// Ascending ID keyset page, exclusive after; limit 1..=MAX_PAGE.
    fn conventions(
        &self,
        query: &ConventionQuery,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredConvention>, StorageError>;
    /// Ascending immutable revision page, with current and historical scope filtering.
    fn convention_versions(
        &self,
        id: &Id,
        scopes: &[KnowledgeScope],
        after_revision: Option<i64>,
        limit: u16,
    ) -> Result<Vec<StoredConvention>, StorageError>;
    fn convention_resources(
        &self,
        query: &ConventionResourceQuery,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredConventionResource>, StorageError>;
    /// Positive scope generation; the absent scope has revision one.
    fn convention_scope_revision(&self, scope: &KnowledgeScope) -> Result<i64, StorageError>;
}

pub trait ConventionRepository: ConventionRead + ConventionIndexRepository {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredConventionIndex {
    pub generation_id: Id,
    pub convention_id: Id,
    pub convention_revision: i64,
    pub resource_id: Option<Id>,
    pub resource_revision: Option<i64>,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConventionTerm {
    pub term: String,
    pub frequency: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredConventionChunk {
    pub generation_id: Id,
    pub id: Id,
    pub convention_id: Id,
    pub convention_revision: i64,
    pub resource_id: Option<Id>,
    pub resource_revision: Option<i64>,
    pub index: i64,
    pub section_path: String,
    /// Optional exact UTF-8 breadcrumb artifact when the public inline bound is insufficient.
    pub section_path_ref: Option<Value>,
    pub text: String,
    pub embedding_text: String,
    /// Frozen ASCII token count of embedding_text, including its breadcrumb.
    pub token_count: i64,
    /// Sorted, unique ASCII term frequencies computed by the owning chunker.
    pub terms: Vec<ConventionTerm>,
    pub vector: Option<Vec<f32>>,
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
    pub chunk: StoredConventionChunk,
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
        resource: Option<&Id>,
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
    /// current title (binary Unicode), then stable convention/resource/generation/chunk identity.
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
    /// Rechecks actual current owner/resource/source/live artifact and real operation, then
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
