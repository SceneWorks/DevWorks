//! Managed artifact files and host-resolved source handles.
//!
//! [`ArtifactFiles`] is the only way core touches artifact bytes. The adapter owns every path:
//! it constructs contained managed paths beneath the data root (`staging/` for in-flight files,
//! `artifacts/` for content-addressed immutable blobs, `quarantine/` for corrupt files kept as
//! evidence) and a disposable cache for derived payloads. No caller-supplied path, digest or
//! name ever becomes a managed path; a digest is validated as 64 lower-case hex characters
//! before it names a file.
//!
//! [`ArtifactSources`] resolves an upload ticket, imported attachment or work-output file handle
//! to a registered read root and a relative path, which the verified no-follow reader
//! (`crate::authority::reads::VerifiedReader`) then opens within a byte bound. A raw path,
//! actor label or digest supplied by a caller is never a source.
use crate::authority::reads::ReadRoot;
use crate::context::InvocationContext;
use crate::protocol::data::ArtifactSource;
use crate::protocol::{Id, ProjectId};
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
    /// The staged source did not have the expected length and digest (the source changed, or
    /// the caller's expectation was wrong); nothing was published.
    Mismatch,
    /// The source exceeded the byte bound; nothing was published.
    TooLarge,
    /// Reading the source failed; nothing was published.
    Source(String),
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
///
/// Removing a digest file ([`Self::unlink`]) is a physical step only: the artifact service
/// first claims the digest in the store (blob state `Unlinking`), so a concurrent save can never
/// commit metadata for a file that is about to disappear.
pub trait ArtifactFiles: Send + Sync {
    /// Streams `source` in bounded chunks into the staging file `staging_name` while hashing
    /// it. More than `max_bytes` is [`FileFault::TooLarge`]; a length or digest other than
    /// expected is [`FileFault::Mismatch`]. The staged file is flushed, atomically renamed to
    /// the digest location, and every directory created or changed on the way is flushed. An
    /// existing digest file is verified before reuse; a corrupt one blocks publication with
    /// [`FileFault::Corrupt`]. On every failure the staging file is removed.
    fn publish(
        &self,
        staging_name: &str,
        source: &mut dyn Read,
        max_bytes: u64,
        expected_digest: &str,
        expected_bytes: u64,
    ) -> Result<PublishedBlob, FileFault>;
    /// The data-root-relative managed path of a digest (recorded on its blob row).
    fn managed_path(&self, digest: &str) -> Result<String, FileFault>;
    /// Whether the digest file exists with exactly `byte_count` bytes (metadata only).
    fn present(&self, digest: &str, byte_count: u64) -> Result<bool, FileFault>;
    /// The verified length of the digest file: `None` when it is missing,
    /// [`FileFault::Corrupt`] when its content does not hash to `digest`.
    fn inspect(&self, digest: &str) -> Result<Option<u64>, FileFault>;
    /// At most `max` bytes from `offset` of a blob whose length must be `byte_count`. The whole
    /// file's digest is verified before its first range is served and again whenever the file
    /// changed since; corrupt bytes are never returned.
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
    /// Removes the digest file; an already missing file is success. Only for a digest the store
    /// has claimed.
    fn unlink(&self, digest: &str) -> Result<(), FileFault>;
    /// Moves a corrupt digest file aside into quarantine (kept as evidence, never served).
    fn quarantine(&self, digest: &str) -> Result<(), FileFault>;
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
    /// directory. Without `overwrite` an existing destination is [`FileFault::Exists`]
    /// (no-clobber link); with it the destination is replaced atomically.
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

/// Host resolution of upload tickets, imported attachment handles and work-output file handles
/// (production adapter: `devworks_adapters::fs::sources::HostArtifactSources`).
pub trait ArtifactSources: Send + Sync {
    /// Resolves `source` for a save into `project` by the invocation `ctx`. A handle bound to
    /// a principal, project or work root resolves only for exactly that binding.
    fn resolve(
        &self,
        ctx: &InvocationContext,
        source: &ArtifactSource,
        project: &ProjectId,
    ) -> Result<ResolvedSource, SourceRefusal>;
}
