//! Offline Help metadata and Unicode passages from atomically published, immutable generations.
use super::StorageError;
use crate::protocol::{Id, OperationId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A passage fits the protocol's `Content` bound, counted as Unicode scalar values.
pub const MAX_HELP_PASSAGE_CHARS: u32 = 65_536;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredHelpHead {
    /// One for the initial, unpublished sentinel; changes only when catalog content changes.
    pub revision: i64,
    /// Empty on the initial sentinel, before the host publishes its actual app version.
    pub app_version: String,
    pub registry_revision: i64,
    /// Empty on the initial sentinel; otherwise lowercase SHA-256 of the source snapshot.
    pub source_digest: String,
    pub generation_id: Option<Id>,
    /// Latest real generation operation, independent of the content revision.
    pub generation_operation_id: Option<OperationId>,
    pub updated_at_ms: i64,
}

/// Lightweight metadata; listing Help never loads the full guide bodies.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredHelpTopic {
    pub generation_id: Id,
    pub id: Id,
    pub version: String,
    pub title: String,
    pub content_digest: String,
    /// A real, committed `ArtifactRef` whose bytes are the full guide body.
    pub content_ref: Option<Value>,
    /// `Available`, `Missing`, `Stale` or `Unavailable`; absent bytes never gain a fake artifact.
    pub availability: String,
    pub reason: Option<String>,
    /// The exact typed `HelpProvenance` snapshot produced by the owning service.
    pub provenance: Value,
    pub related_operations: Vec<String>,
    pub screen_ids: Vec<String>,
    /// `Bundled`, `Package` or `Generated`.
    pub source_kind: String,
    pub package_id: Option<Id>,
    pub manifest_digest: Option<String>,
    pub character_count: i64,
    pub topic_revision: i64,
}

/// Per-content budgets, independent of the legal 256 MiB package source bound.
pub const MAX_HELP_INDEX_BYTES: usize = 64 * 1024;
pub const HELP_CHECKPOINT_BYTES: u64 = 1024 * 1024;
pub const MAX_HELP_CHECKPOINTS: usize = 258;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HelpOffset {
    pub byte_offset: u64,
    pub character_offset: i64,
}
/// Small immutable projection shared by every generation of the same exact full content.
/// Full bodies live only in their content-addressed artifact, never as duplicated SQL text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredHelpContent {
    pub digest: String,
    pub byte_count: u64,
    pub character_count: i64,
    pub indexed_bytes: u64,
    pub index_complete: bool,
    pub search_text: String,
    pub offsets: Vec<HelpOffset>,
}
impl StoredHelpContent {
    pub fn commitment(&self) -> Result<String, serde_json::Error> {
        serde_json::to_vec(self).map(|bytes| crate::fingerprint::sha256_hex(&bytes))
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct StoredHelpDocument {
    pub topic: StoredHelpTopic,
    pub content: Option<StoredHelpContent>,
}
/// Exact immutable source and the nearest preceding Unicode checkpoint; no file I/O in SQL.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredHelpPassage {
    pub content_ref: Value,
    pub content_digest: String,
    pub byte_count: u64,
    pub character_count: i64,
    pub checkpoint: HelpOffset,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HelpFilter {
    pub query: Option<String>,
    pub screen_id: Option<String>,
    pub package_id: Option<Id>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredHelpDraft {
    pub generation_id: Id,
    pub operation_id: OperationId,
}

pub trait HelpRead {
    /// Bounded host recovery inventory; public Help queries never list drafts.
    fn help_drafts(
        &self,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredHelpDraft>, StorageError>;
    fn help_head(&self) -> Result<StoredHelpHead, StorageError>;
    /// Immutable original publisher of a published generation; drafts remain invisible.
    fn help_generation_operation(
        &self,
        generation: &Id,
    ) -> Result<Option<OperationId>, StorageError>;
    /// Published generations only, including historical generations; drafts are invisible.
    fn help_topic(&self, generation: &Id, id: &Id)
    -> Result<Option<StoredHelpTopic>, StorageError>;
    /// Ascending ID keyset page, exclusive cursor; limit is `1..=MAX_PAGE`.
    /// Query is a literal lowercase substring of the bounded projection. The public Help
    /// owner scans unindexed source text before reporting a complete negative. No query
    /// means every topic.
    fn help_topics(
        &self,
        generation: &Id,
        filter: &HelpFilter,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredHelpTopic>, StorageError>;
    /// Bounded shared projection; available historical generations continue to name it.
    fn help_content(&self, digest: &str) -> Result<Option<StoredHelpContent>, StorageError>;
    /// A real live published content reference suitable for verified host reuse, if any.
    fn help_content_reference(&self, digest: &str) -> Result<Option<Value>, StorageError>;
    /// Published source metadata for a bounded Unicode passage. The Help owner verifies and
    /// reads the artifact outside its database snapshot; unavailable guides return None.
    fn help_passage_source(
        &self,
        generation: &Id,
        id: &Id,
        offset: i64,
        chars: u32,
    ) -> Result<Option<StoredHelpPassage>, StorageError>;
}

pub trait HelpRepository: HelpRead {
    /// Claims a real generation operation without changing the catalog content revision.
    /// The owning service checks the previous operation's terminal state in the same unit of work.
    fn set_help_generation_operation(
        &mut self,
        expected_help_revision: i64,
        expected_registry_revision: i64,
        operation: &OperationId,
    ) -> Result<StoredHelpHead, StorageError>;
    /// Stages one immutable document. Identical restaging is idempotent; a changed duplicate is
    /// refused. Draft generations remain invisible, allowing guides to be prepared one at a time.
    fn stage_help_document(
        &mut self,
        document: &StoredHelpDocument,
    ) -> Result<StoredHelpTopic, StorageError>;
    /// Trusted publisher/recovery metadata reads, including its staged documents.
    fn staged_help_topic(
        &self,
        generation: &Id,
        id: &Id,
    ) -> Result<Option<StoredHelpTopic>, StorageError>;
    fn staged_help_topics(
        &self,
        generation: &Id,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredHelpTopic>, StorageError>;
    /// Removes at most `limit` documents from a draft owned by a terminal real operation. The
    /// service retires that page's generation-owned artifacts in this same unit of work first.
    /// Returns true once the entire draft has been removed. Published generations are refused.
    fn discard_help_draft(
        &mut self,
        generation: &Id,
        operation: &OperationId,
        limit: u16,
    ) -> Result<bool, StorageError>;
    /// Verifies the complete staged generation, current registry and exact operation claim, then
    /// atomically publishes the head. Previously published generations remain immutable/readable.
    fn publish_help_generation(
        &mut self,
        expected_help_revision: i64,
        expected_registry_revision: i64,
        expected_operation: Option<&OperationId>,
        head: &StoredHelpHead,
        expected_topic_count: i64,
    ) -> Result<StoredHelpHead, StorageError>;
}
