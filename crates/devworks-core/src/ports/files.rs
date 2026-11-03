//! Managed artifact files and host-resolved source handles.
//!
//! [`ArtifactFiles`] is the only way core touches artifact bytes. The adapter owns every path:
//! it constructs contained managed paths beneath the data root (`staging/` for in-flight files,
//! `artifacts/` for content-addressed immutable blobs) and a disposable cache for derived
//! payloads. No caller-supplied path, digest or name ever becomes a managed path; a digest is
//! validated as 64 lower-case hex characters before it names a file.
//!
//! [`ArtifactSources`] resolves an upload ticket, imported attachment or work-output file handle
//! to a registered read root and a relative path, which the verified no-follow reader
//! (`crate::authority::reads::VerifiedReader`) then reads within a byte bound. A raw path,
//! actor label or digest supplied by a caller is never a source.
use crate::authority::reads::ReadRoot;
use crate::context::InvocationContext;
use crate::protocol::Id;
use crate::protocol::data::ArtifactSource;
use std::io::Read;
use std::path::Path;

/// Why a managed file operation failed. Every variant is surfaced (never retried silently or
/// turned into deletion).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileFault {
    /// The managed file (or derived payload) does not exist.
    NotFound,
    /// Its length or digest does not match the recorded identity; it is never served or reused.
    Corrupt(String),
    /// The volume is full; nothing previously published is affected.
    DiskFull,
    /// An export destination already exists and overwrite was not requested.
    Exists,
    Io(String),
    Unsupported(String),
}

/// A blob that is durably in place at its content-addressed managed path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedBlob {
    /// Data-root-relative managed path.
    pub relative_path: String,
    /// An identical, verified file was already in place; the staged copy was discarded.
    pub reused: bool,
}

/// Managed artifact files (adapter: `devworks_adapters::fs::artifacts`).
pub trait ArtifactFiles: Send + Sync {
    /// Writes `bytes` to the staging file `staging_name`, flushes it, verifies its length and
    /// SHA-256 against `digest`, then atomically renames it to the digest location and flushes
    /// the directory. An existing digest file is verified before reuse; a corrupt one blocks
    /// publication with [`FileFault::Corrupt`].
    fn publish(
        &self,
        staging_name: &str,
        bytes: &[u8],
        digest: &str,
    ) -> Result<PublishedBlob, FileFault>;
    /// Whether the digest file exists with exactly `byte_count` bytes (metadata only).
    fn present(&self, digest: &str, byte_count: u64) -> Result<bool, FileFault>;
    /// At most `max` bytes from `offset` of a blob whose length must be `byte_count`.
    fn read_range(
        &self,
        digest: &str,
        byte_count: u64,
        offset: u64,
        max: usize,
    ) -> Result<Vec<u8>, FileFault>;
    /// Re-hashes the whole blob against its digest and length.
    fn verify(&self, digest: &str, byte_count: u64) -> Result<(), FileFault>;
    /// Opens a verified blob for streaming.
    fn open(&self, digest: &str, byte_count: u64) -> Result<Box<dyn Read + Send>, FileFault>;
    /// Removes the digest file; an already missing file is success.
    fn unlink(&self, digest: &str) -> Result<(), FileFault>;
    /// Names of files currently in staging.
    fn staged(&self) -> Result<Vec<String>, FileFault>;
    fn remove_staged(&self, name: &str) -> Result<(), FileFault>;
    /// Digests of every blob file on disk (with or without metadata).
    fn stored_digests(&self) -> Result<Vec<String>, FileFault>;
    /// Stores a derived, rebuildable payload (previews) in the disposable cache class.
    fn write_derived(&self, key: &str, bytes: &[u8]) -> Result<(), FileFault>;
    /// A derived payload of at most `max` bytes, if still cached.
    fn read_derived(&self, key: &str, max: usize) -> Result<Option<Vec<u8>>, FileFault>;
    /// Removes every derived payload (they are rebuilt on demand); returns how many.
    fn collect_derived(&self) -> Result<u64, FileFault>;
    /// Copies a verified blob to `destination` through a flushed temporary file in the same
    /// directory. Without `overwrite` an existing destination is [`FileFault::Exists`] (no-clobber
    /// link); with it the destination is replaced atomically.
    fn export(
        &self,
        digest: &str,
        byte_count: u64,
        destination: &Path,
        overwrite: bool,
    ) -> Result<(), FileFault>;
}

/// Facts the host recorded with a source handle (provenance, never authority).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceOrigin {
    /// Repository whose worktree produced a work-output file.
    pub repository_id: Option<Id>,
    /// Repository revision (commit) the work output was produced at.
    pub repository_revision: Option<String>,
    /// Original file name (an imported attachment's filename, an upload's picked name).
    pub original_name: Option<String>,
}

/// A source handle resolved by the host to a registered read root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSource {
    pub root: ReadRoot,
    /// Relative path of normal components beneath `root`.
    pub relative: String,
    pub origin: SourceOrigin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceRefusal {
    /// No such handle (or not visible to this principal; existence is not disclosed).
    NotFound,
    /// The handle exists but this invocation may not read it.
    PermissionDenied,
}

/// Host resolution of upload tickets, imported attachment handles and work-output file handles.
pub trait ArtifactSources: Send + Sync {
    fn resolve(
        &self,
        ctx: &InvocationContext,
        source: &ArtifactSource,
    ) -> Result<ResolvedSource, SourceRefusal>;
}
