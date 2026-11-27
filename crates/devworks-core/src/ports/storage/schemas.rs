//! Shared SQL metadata sources and immutable, explicitly selected structural views.
use super::StorageError;
use crate::protocol::records::{IndexRunRecord, SchemaConnection, SchemaObject, SchemaRelation};
use crate::protocol::{ActorId, Id, OperationId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredSchemaSource {
    pub configuration_revision: i64,
    pub connection: SchemaConnection,
    pub owner_id: ActorId,
    pub deleted: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}
#[derive(Clone, Debug)]
pub struct SchemaVisibility {
    pub actor_id: ActorId,
    pub workspace: bool,
    pub project_id: Option<Id>,
    /// Agent/tool exposure requires an enabled, explicitly exposed source.
    pub exposed_only: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredSchemaView {
    pub id: Id,
    pub connection_id: Id,
    pub source_revision: i64,
    pub configuration_revision: i64,
    pub coverage: crate::protocol::data::Coverage,
    pub operation_id: OperationId,
    pub digest: String,
    pub sealed: bool,
    pub created_at_ms: i64,
}
#[derive(Clone, Debug, Default)]
pub struct SchemaObjectFilter {
    pub object_id: Option<Id>,
    pub kinds: Vec<String>,
    pub parent_id: Option<Id>,
    pub text: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredSchemaPublication {
    pub operation_id: OperationId,
    pub connection_id: Id,
    pub result: IndexRunRecord,
    pub created_at_ms: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredSchemaReadPin {
    pub id: Id,
    pub owner_namespace: String,
    pub store_id: crate::protocol::StoreId,
    pub restore_epoch: i64,
    pub scope_digest: String,
    pub intent_digest: String,
    pub connection_id: Id,
    pub view_id: Id,
    pub view_digest: String,
    pub configuration_revision: i64,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
}
#[derive(Clone)]
pub struct StoredSchemaCertificate {
    pub reference: crate::protocol::provider::ArtifactRef,
    pub pem: Vec<u8>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SensitivePolicy {
    pub revision: i64,
    pub rules: Vec<SensitiveRule>,
    /// The revision-1 surface replaces only these source-local column IDs.
    pub legacy_columns: Vec<Id>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SensitiveRule {
    pub id: Id,
    pub catalog: Option<String>,
    pub schema: Option<String>,
    pub table: Option<String>,
    pub column: String,
    pub allow: bool,
    pub reason: Option<String>,
}

pub trait SchemaRead {
    fn mysql_readonly_sources(&self) -> Result<Vec<StoredSchemaSource>, StorageError>;
    fn sensitive_policy(&self, source: Option<&Id>) -> Result<SensitivePolicy, StorageError>;
    fn schema_certificate(
        &self,
        connection: &Id,
    ) -> Result<Option<StoredSchemaCertificate>, StorageError>;
    fn schema_pin(&self, id: &Id) -> Result<Option<StoredSchemaReadPin>, StorageError>;
    fn schema_active_pin(
        &self,
        intent: &str,
        now: i64,
    ) -> Result<Option<StoredSchemaReadPin>, StorageError>;
    /// True only for a currently retained source; no secret value is stored here.
    fn schema_credential_in_use(&self, handle: &Id) -> Result<bool, StorageError>;
    fn schema_inventory_revision(&self) -> Result<i64, StorageError>;
    fn schema_source(&self, id: &Id) -> Result<Option<StoredSchemaSource>, StorageError>;
    /// Authority-derived selection precedes keyset paging. The service also checks its context.
    fn schema_sources(
        &self,
        visibility: &SchemaVisibility,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredSchemaSource>, StorageError>;
    fn schema_view(&self, id: &Id) -> Result<Option<StoredSchemaView>, StorageError>;
    fn schema_objects(
        &self,
        view: &Id,
        filter: &SchemaObjectFilter,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<SchemaObject>, StorageError>;
    fn schema_relations(
        &self,
        view: &Id,
        objects: &[Id],
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<SchemaRelation>, StorageError>;
    fn schema_publication(
        &self,
        operation: &OperationId,
    ) -> Result<Option<StoredSchemaPublication>, StorageError>;
    /// The earliest expiry time (epoch ms) among live read pins, so retention can wake then.
    fn next_schema_pin_expiry(&self) -> Result<Option<i64>, StorageError> {
        Ok(None)
    }
}
pub trait SchemaRepository: SchemaRead {
    fn set_sensitive_policy(
        &mut self,
        source: Option<&Id>,
        expected: i64,
        policy: &SensitivePolicy,
    ) -> Result<(), StorageError>;
    fn set_schema_certificate(
        &mut self,
        connection: &Id,
        material: Option<&StoredSchemaCertificate>,
    ) -> Result<(), StorageError>;
    fn create_schema_pin(&mut self, pin: &StoredSchemaReadPin) -> Result<(), StorageError>;
    fn expire_schema_pins(&mut self, now: i64, limit: u16) -> Result<usize, StorageError>;
    /// Prune at most limit obsolete metadata rows, preserving current heads, live pins and two newest sealed views.
    fn prune_schema_views(&mut self, limit: u16) -> Result<usize, StorageError>;
    fn create_schema_source(&mut self, row: &StoredSchemaSource) -> Result<(), StorageError>;
    fn revise_schema_source(
        &mut self,
        row: &StoredSchemaSource,
        expected: i64,
    ) -> Result<(), StorageError>;
    /// Delete at most limit abandoned unsealed rows, then empty staging headers.
    /// Returns true when no unsealed view remains for this source. Sealed views are retained.
    fn discard_schema_staging(&mut self, connection: &Id, limit: u16)
    -> Result<bool, StorageError>;
    fn create_schema_view(&mut self, row: &StoredSchemaView) -> Result<(), StorageError>;
    /// Bounded staging batches are not visible through any read method before sealing.
    fn append_schema_rows(
        &mut self,
        view: &Id,
        objects: &[SchemaObject],
        relations: &[SchemaRelation],
    ) -> Result<(), StorageError>;
    fn seal_schema_view(&mut self, view: &Id) -> Result<(), StorageError>;
    fn remove_schema_views(&mut self, connection: &Id) -> Result<(), StorageError>;
    /// Immutable publication witness remains recoverable after later source edits/removal.
    fn record_schema_publication(
        &mut self,
        row: &StoredSchemaPublication,
    ) -> Result<(), StorageError>;
}
