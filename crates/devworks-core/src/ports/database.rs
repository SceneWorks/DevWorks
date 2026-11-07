//! Target database metadata only. No caller-authored SQL or source row values cross this port.
//! Source access is re-authorized by the owning service before dispatch and publication.
use super::secrets::Secret;
use super::storage::Cancellation;
use crate::protocol::Id;
use crate::protocol::data::Coverage;
use crate::protocol::records::{ConnectionConfig, SchemaAdapterProfile, SchemaObjectMetadata};
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

/// One exact configured target and bounded metadata request. Debug never reveals a credential.
pub struct MetadataRequest {
    pub connection_id: Id,
    pub config: ConnectionConfig,
    pub password: Option<Secret>,
    /// Verified CA material, resolved from the configured immutable artifact by the service.
    pub ca_certificate: Option<Vec<u8>>,
    pub test_only: bool,
    pub limits: MetadataLimits,
    pub cancellation: Cancellation,
}
#[derive(Clone, Debug)]
pub struct MetadataLimits {
    pub deadline: Instant,
    pub max_objects: usize,
    pub max_relationships: usize,
    pub max_bytes: usize,
}
impl Default for MetadataLimits {
    fn default() -> Self {
        Self {
            deadline: Instant::now() + Duration::from_secs(30),
            max_objects: 100_000,
            max_relationships: 500_000,
            max_bytes: 64 * 1024 * 1024,
        }
    }
}
#[derive(Clone, Debug)]
pub struct MetadataObject {
    /// Adapter-stable identity of the catalog/schema/object, independent of list order.
    pub key: String,
    pub parent_key: Option<String>,
    pub kind: String,
    pub qualified_name: String,
    pub metadata: SchemaObjectMetadata,
    pub coverage: Coverage,
}
#[derive(Clone, Debug)]
pub struct MetadataRelationship {
    pub key: String,
    pub from_key: String,
    pub to_key: String,
    pub kind: String,
    pub inferred: bool,
}
#[derive(Clone, Debug)]
pub struct MetadataSnapshot {
    pub profile: SchemaAdapterProfile,
    pub coverage: Coverage,
    pub objects: Vec<MetadataObject>,
    pub relationships: Vec<MetadataRelationship>,
}
/// Typed/redacted failure. Driver diagnostics, endpoints, SQL and credentials never form it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetadataFailure {
    Cancelled,
    Deadline,
    UnsupportedProfile,
    InvalidTarget,
    CredentialsRequired,
    AccessDenied,
    Unavailable,
    InvalidMetadata,
}
pub type MetadataFuture =
    Pin<Box<dyn Future<Output = Result<MetadataSnapshot, MetadataFailure>> + Send>>;
pub trait MetadataReader: Send + Sync {
    /// Owns connection/transaction lifetime through physical close, including cancellation.
    fn read(&self, request: MetadataRequest) -> MetadataFuture;
}
