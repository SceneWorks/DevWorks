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
    /// The generation whose file now holds the bytes: the requested new generation, or the
    /// reused one.
    pub generation: String,
    /// Data-root-relative managed path of that generation.
    pub relative_path: String,
    /// The existing generation was verified and reused; the staged copy was discarded.
    pub reused: bool,
}

/// Managed artifact files (adapter: `devworks_adapters::fs::artifacts`).
///
/// Every managed blob file is one immutable **generation** of a digest:
/// `<digest>.<generation>`. A generation is written once (by the staging operation whose ID it
/// is) and never rewritten, so every read, open, removal and quarantine names an exact
/// `(digest, generation)` and can never affect another incarnation of the same bytes.
pub trait ArtifactFiles: Send + Sync {
    /// Streams `source` in bounded chunks into the staging file `staging_name` while hashing
    /// it, reading at most `expected_bytes + 1` bytes; any other length or digest is
    /// [`FileFault::Mismatch`]. Then either reuses `reuse` (an existing generation, verified
    /// first: a corrupt one is [`FileFault::Corrupt`] and nothing is published) or publishes the
    /// staged file as the new `generation`: flushed, atomically renamed, and every directory
    /// created or changed on the way flushed. On every failure the staging file is removed.
    fn publish(
        &self,
        staging_name: &str,
        source: &mut dyn Read,
        expected_digest: &str,
        expected_bytes: u64,
        reuse: Option<&str>,
        generation: &str,
    ) -> Result<PublishedBlob, FileFault>;
    /// The data-root-relative managed path of one generation (recorded on its blob row).
    fn managed_path(&self, digest: &str, generation: &str) -> Result<String, FileFault>;
    /// Whether the generation's file exists with exactly `byte_count` bytes (metadata only).
    fn present(&self, digest: &str, generation: &str, byte_count: u64) -> Result<bool, FileFault>;
    /// The verified length of a generation's file: `None` when it is missing,
    /// [`FileFault::Corrupt`] when its content does not hash to `digest`.
    fn inspect(&self, digest: &str, generation: &str) -> Result<Option<u64>, FileFault>;
    /// At most `max` bytes from `offset` of a generation whose length must be `byte_count`.
    /// The file's digest is verified (on the same open handle) before its first range is served
    /// and again whenever the file's identity or timestamps changed; corrupt bytes are never
    /// returned.
    fn read_range(
        &self,
        digest: &str,
        generation: &str,
        byte_count: u64,
        offset: u64,
        max: usize,
    ) -> Result<Vec<u8>, FileFault>;
    /// Opens a generation, verifies its digest on that handle and returns the same handle,
    /// rewound, for streaming.
    fn open(
        &self,
        digest: &str,
        generation: &str,
        byte_count: u64,
    ) -> Result<Box<dyn Read + Send>, FileFault>;
    /// Opens and verifies on the same handle, checking `stopped` before every bounded
    /// verification chunk. An adapter that cannot honor this control refuses before I/O.
    fn open_controlled(
        &self,
        _digest: &str,
        _generation: &str,
        _byte_count: u64,
        _stopped: &mut dyn FnMut() -> bool,
    ) -> Result<Box<dyn Read + Send>, FileFault> {
        Err(FileFault::Unsupported(
            "controlled blob verification is unavailable".into(),
        ))
    }
    /// Verifies and reads a bounded range from that same handle, checking control during
    /// the first full-file verification too. Unsupported adapters refuse before I/O.
    fn read_range_controlled(
        &self,
        _digest: &str,
        _generation: &str,
        _byte_count: u64,
        _offset: u64,
        _max: usize,
        _stopped: &mut dyn FnMut() -> bool,
    ) -> Result<Vec<u8>, FileFault> {
        Err(FileFault::Unsupported(
            "controlled blob ranges are unavailable".into(),
        ))
    }
    /// Removes exactly one generation's file; an already missing file is success.
    fn unlink(&self, digest: &str, generation: &str) -> Result<(), FileFault>;
    /// Moves a corrupt generation aside into quarantine (kept as evidence, never served).
    fn quarantine(&self, digest: &str, generation: &str) -> Result<(), FileFault>;
    /// Names of files currently in staging.
    fn staged(&self) -> Result<Vec<String>, FileFault>;
    fn remove_staged(&self, name: &str) -> Result<(), FileFault>;
    /// Every blob file on disk as `(digest, generation)`, with or without metadata.
    fn stored_files(&self) -> Result<Vec<(String, String)>, FileFault>;
    /// Stores a derived, rebuildable payload (previews) in the disposable cache class.
    fn write_derived(&self, key: &str, bytes: &[u8]) -> Result<(), FileFault>;
    /// Project previews cannot create private cache bytes after a reviewed purge.
    /// Unsupported adapters leave the preview uncached.
    fn write_project_derived(
        &self,
        _project: &ProjectId,
        _key: &str,
        _bytes: &[u8],
    ) -> Result<(), FileFault> {
        Err(FileFault::Unsupported(
            "project cache custody unavailable".into(),
        ))
    }
    /// Normal cache-owner recovery, never a purge-triggered global cache sweep.
    fn recover_project_derived(&self) -> Result<(), FileFault> {
        Ok(())
    }

    /// A derived payload of at most `max` bytes, if still cached.
    fn read_derived(&self, key: &str, max: usize) -> Result<Option<Vec<u8>>, FileFault>;
    /// Removes every derived payload (they are rebuilt on demand); returns how many.
    fn collect_derived(&self) -> Result<u64, FileFault>;
    /// Copies a generation to `destination` through a flushed temporary file in the same
    /// directory, hashing the bytes as they are copied from one open handle and refusing a
    /// mismatch before the destination is touched. Without `overwrite` an existing destination
    /// is [`FileFault::Exists`] (no-clobber link); with it the destination is replaced
    /// atomically.
    fn export(
        &self,
        digest: &str,
        generation: &str,
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
    /// Resolves `source` for a save into `project` (`None`: the workspace scope) by the
    /// invocation `ctx`. A handle bound to a principal, project or work root resolves only for
    /// exactly that binding; a project-bound handle never resolves for the workspace.
    fn resolve(
        &self,
        ctx: &InvocationContext,
        source: &ArtifactSource,
        project: Option<&ProjectId>,
    ) -> Result<ResolvedSource, SourceRefusal>;
}
