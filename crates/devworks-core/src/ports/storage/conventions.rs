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
    pub include_deleted: bool,
}

pub trait ConventionRead {
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

pub trait ConventionRepository: ConventionRead {
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
