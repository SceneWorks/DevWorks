//! The native data path owns its wire/file handle through settlement. It does not publish
//! rows to the index or grant authority from observed target data.
use super::super::secrets::Secret;
use super::super::storage::Cancellation;
use crate::protocol::Id;
use crate::protocol::records::ConnectionConfig;
use crate::services::database::statement::{CheckedStatement, DataMode};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataCapabilities {
    pub read: bool,
    pub write: bool,
    pub schema_change: bool,
    pub atomic_ddl: bool,
}

/// Data values before the application publishes an authorized binary artifact.
#[derive(Clone, PartialEq)]
pub enum Scalar {
    Null,
    Boolean(bool),
    Integer(i64),
    Decimal(String),
    Text(String),
    Bytes(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalColumn {
    pub catalog: String,
    pub schema: String,
    pub table: String,
    pub column: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub data_type: String,
}

#[derive(Clone)]
pub struct ReadResult {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Scalar>>,
    pub truncated: bool,
    pub schema_digest: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MutationResult {
    pub affected_rows: Option<i64>,
    pub observed_statements: usize,
    pub schema_changed: bool,
}

pub enum DataOutcome {
    Read(ReadResult),
    Mutation(MutationResult),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    InvalidTarget,
    UnsupportedProfile,
    UnsupportedStatement,
    UnsupportedValue,
    PermissionDenied,
    StaleSchema,
    Constraint,
    Cancelled,
    Deadline,
    Unavailable,
    ResultLimit,
}

/// A failure retains only durable or driver-proven outcomes. An interrupted possible send with
/// no terminal observation is unknown even when a driver dropped the session locally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataFailure {
    Refused(FailureKind),
    RolledBack(FailureKind),
    Partial {
        reason: FailureKind,
        known: MutationResult,
    },
    Unknown {
        known: MutationResult,
    },
}

#[derive(Clone, Copy)]
pub struct DataLimits {
    pub deadline: Instant,
    pub rows: usize,
    pub bytes: usize,
    pub columns: usize,
}

pub struct DataRequest {
    pub id: Id,
    pub mode: DataMode,
    pub config: ConnectionConfig,
    pub password: Option<Secret>,
    pub ca_certificate: Option<Vec<u8>>,
    pub statements: Vec<CheckedStatement>,
    pub parameters: Vec<Scalar>,
    pub expected_schema_digest: Option<String>,
    pub include_schema_digest: bool,
    pub schema_targets: Vec<crate::services::database::statement::Relation>,
    pub limits: DataLimits,
    pub cancellation: Cancellation,
    /// Protected app-owned locations and the identity of a selected existing file.
    pub file_authority: Option<crate::authority::destructive::FileIdentity>,
}

/// Checks current application authority at the native effect boundary. Driver-proven physical
/// identities are interpreted by the owner, never used as a substitute for the authority check.
pub trait DispatchGuard: Send + Sync {
    fn check_columns(&self, columns: &[PhysicalColumn]) -> Result<(), DataFailure>;
    fn before_statement(&self, position: usize) -> Result<(), DataFailure>;
    fn before_commit(&self) -> Result<(), DataFailure>;
    fn observed_statement(
        &self,
        position: usize,
        mutation: MutationResult,
    ) -> Result<(), DataFailure>;
}

pub type DataFuture = Pin<Box<dyn Future<Output = Result<DataOutcome, DataFailure>> + Send>>;

pub trait DataAdapter: Send + Sync {
    fn capabilities(&self, engine: &crate::protocol::records::DbEngine) -> DataCapabilities;
    fn execute(&self, request: DataRequest, guard: Arc<dyn DispatchGuard>) -> DataFuture;
}
