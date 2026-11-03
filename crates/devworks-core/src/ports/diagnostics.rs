//! Host probes behind local diagnostics (OPS-01) and the staging seam of diagnostic exports.
//!
//! * [`StorageHealthProbe`] is implemented by the storage adapter: the migration ledger, the
//!   journal mode and write-ahead log, database and free-disk sizes of the data-root volume,
//!   schema objects missing against a freshly migrated store, and the full integrity check. A
//!   value the platform or the store cannot report is [`Metric::Unavailable`] with its reason,
//!   never a zero.
//! * [`ExportStaging`] turns rendered export bytes into an artifact source for `artifacts.save`
//!   (the production adapter writes a host-owned file and mints an upload ticket at user
//!   ingress), so an export becomes an ordinary artifact through the one artifact service.
use crate::context::InvocationContext;
use crate::ports::storage::{CompatibilityReport, IntegrityReport, StorageError};
use crate::protocol::data::ArtifactSource;

/// A measured value, or why it could not be measured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Metric {
    Available(u64),
    Unavailable(String),
}
impl Metric {
    pub fn value(&self) -> Option<u64> {
        match self {
            Self::Available(v) => Some(*v),
            Self::Unavailable(_) => None,
        }
    }
    /// A measurement that may have failed.
    pub fn of<E: std::fmt::Display>(r: Result<u64, E>) -> Self {
        match r {
            Ok(v) => Self::Available(v),
            Err(e) => Self::Unavailable(e.to_string()),
        }
    }
}

/// File-level storage facts (no database connection needed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageFiles {
    /// Bytes of the main database file.
    pub database_bytes: Metric,
    /// Bytes of the write-ahead log (0 when the log file does not exist: nothing is pending).
    pub wal_bytes: Metric,
    /// Bytes available to this user on the data-root volume.
    pub free_bytes: Metric,
    /// Size of the data-root volume.
    pub total_bytes: Metric,
}

/// Database-level storage facts read on one connection.
#[derive(Clone, Debug)]
pub struct StorageDatabase {
    pub compatibility: CompatibilityReport,
    /// `wal` in normal operation.
    pub journal_mode: String,
    pub page_size: u64,
    pub page_count: u64,
    pub freelist_pages: u64,
    /// Tables, indexes, triggers and views a freshly migrated store at this store's schema
    /// version has.
    pub expected_objects: u64,
    /// Those of them this store lacks (sorted names).
    pub missing_objects: Vec<String>,
}

/// The storage adapter's health probes. Implementations must be cheap except
/// [`Self::integrity`], which reads the whole database and is run by the health monitor, not
/// per query.
pub trait StorageHealthProbe: Send + Sync {
    fn files(&self) -> StorageFiles;
    fn database(&self) -> Result<StorageDatabase, StorageError>;
    /// `PRAGMA integrity_check` plus the foreign-key check.
    fn integrity(&self) -> Result<IntegrityReport, StorageError>;
}

/// Why an export could not be staged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExportRefused {
    /// Exports are written by a person's own action (an upload ticket is minted at user ingress).
    HumanOnly,
    /// The export needs a project to own its artifact.
    NoProject,
    /// No staging is attached to the diagnostics service.
    Unavailable,
    Io(String),
}

/// Stages rendered export bytes as an artifact source a later `artifacts.save` by the same
/// invocation resolves.
pub trait ExportStaging: Send + Sync {
    fn stage(
        &self,
        ctx: &InvocationContext,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<ArtifactSource, ExportRefused>;
    /// The save that read `source` was decided (its bytes were copied, or it was refused for
    /// good): remove the staged file.
    fn release(&self, source: &ArtifactSource);
    /// Removes staged files left past their ticket's lifetime; returns how many.
    fn sweep(&self) -> usize;
}
