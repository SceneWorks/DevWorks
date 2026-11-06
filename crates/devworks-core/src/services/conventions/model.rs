//! The registered portable import format and retained source bounds. Bounds apply to complete
//! source entries; protocol passage bounds never narrow a legal authored document.
use crate::fingerprint::sha256_hex;
use crate::ports::storage::{
    KnowledgeScope, StorageError, StoredConvention, StoredConventionResource,
};
use crate::protocol::Revision;
use crate::protocol::data::DataScope;
use crate::protocol::provider::{Digest, QualifiedId, SchemaRef};
use crate::protocol::records::{
    ConventionIndexPhase, ConventionIndexStatus, ConventionRecord, ConventionResourceRecord,
};
use crate::protocol::support::{BoundedList, BoundedText, Check, GraphText, SignedCounter};
use crate::services::sessions::counter;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// MariaDB MEDIUMTEXT's retained UTF-8 byte bound, independent of response passage size.
pub const MAX_MARKDOWN_BYTES: u64 = 16_777_215;
/// The retained wiki permits root plus three child levels.
pub const MAX_DEPTH: usize = 3;
pub const MANIFEST_SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/architecture/imports/convention-manifest.v1.schema.json"
));

/// Exact registered format identity. Imports validate this reference and the real source
/// artifact; exports produce the same version, without foreign principal assertions.
pub fn manifest_schema() -> SchemaRef {
    SchemaRef {
        schema_id: QualifiedId::try_from("devworks/convention-manifest".to_owned())
            .expect("owned qualified schema ID"),
        revision: Revision::new(1).expect("positive schema revision"),
        sha256: Digest(sha256_hex(MANIFEST_SCHEMA.as_bytes())),
    }
}

pub fn scope(scope: &KnowledgeScope) -> DataScope {
    match scope {
        KnowledgeScope::Workspace => DataScope::Workspace,
        KnowledgeScope::Project(project_id) => DataScope::Project {
            project_id: project_id.clone(),
        },
    }
}

/// Absence of an index head is not a successful empty index or a queued operation.
pub fn unindexed() -> ConventionIndexStatus {
    ConventionIndexStatus {
        status: ConventionIndexPhase::NotIndexed,
        generation_id: None,
        source_revision: None,
        source_digest: None,
        embedding_space: None,
        indexed_chunks: None,
        operation: None,
        reason: None,
    }
}

fn parse<T: DeserializeOwned + Check>(value: Value) -> Result<T, StorageError> {
    let value: T = serde_json::from_value(value)
        .map_err(|error| StorageError::InvalidRecord(format!("stored convention: {error}")))?;
    value.check().map_err(StorageError::InvalidRecord)?;
    Ok(value)
}

pub fn record(
    row: &StoredConvention,
    index: ConventionIndexStatus,
) -> Result<ConventionRecord, StorageError> {
    let result = ConventionRecord {
        id: row.id.clone(),
        revision: Revision::new(row.revision).map_err(|e| StorageError::InvalidRecord(e.into()))?,
        scope: scope(&row.scope),
        slug: GraphText(row.slug.clone()),
        title: GraphText(row.title.clone()),
        parent_id: row.parent_id.clone(),
        sort_order: SignedCounter(row.sort_order.to_string()),
        lifecycle: parse(Value::String(row.lifecycle.clone()))?,
        content: row.content_ref.clone().map(parse).transpose()?,
        raw_content: row.raw_content_ref.clone().map(parse).transpose()?,
        applicability: BoundedList(row.applicability.iter().cloned().map(BoundedText).collect()),
        provenance: parse(row.provenance.clone())?,
        author_id: row.author_id.clone(),
        created_at_ms: counter(row.created_at_ms),
        updated_at_ms: counter(row.updated_at_ms),
        legacy_approved: row.legacy_approved,
        index,
    };
    result.check().map_err(StorageError::InvalidRecord)?;
    Ok(result)
}

pub fn resource(
    row: &StoredConventionResource,
    owner: &StoredConvention,
) -> Result<ConventionResourceRecord, StorageError> {
    let result = ConventionResourceRecord {
        id: row.id.clone(),
        revision: Revision::new(row.revision).map_err(|e| StorageError::InvalidRecord(e.into()))?,
        convention_id: row.convention_id.clone(),
        convention_revision: Revision::new(row.convention_revision)
            .map_err(|e| StorageError::InvalidRecord(e.into()))?,
        scope: scope(&owner.scope),
        name: BoundedText(row.name.clone()),
        lifecycle: parse(Value::String(row.lifecycle.clone()))?,
        content: parse(row.content_ref.clone())?,
        applicability: BoundedList(
            owner
                .applicability
                .iter()
                .cloned()
                .map(BoundedText)
                .collect(),
        ),
        author_id: row.author_id.clone(),
        created_at_ms: counter(row.created_at_ms),
        deleted_at_ms: row.deleted_at_ms.map(counter),
        media_type: GraphText(row.media_type.clone()),
    };
    result.check().map_err(StorageError::InvalidRecord)?;
    Ok(result)
}

#[path = "manifest_writer.rs"]
mod manifest_writer;
pub use manifest_writer::{EditorConvention, EditorResource, EditorText, write_editor_manifest};
