//! Verified native reads: bounded, no-follow regular-file reads beneath a registered root.
//!
//! The [`VerifiedReader`] adapter opens every path component relative to an already opened root
//! handle without following links (descriptor-relative `openat`/`O_NOFOLLOW` on Unix; held
//! no-delete-share handles with reparse-point checks on Windows), requires a regular file,
//! re-verifies after open that the path still names the opened object, and enforces a byte
//! bound. Authority publication happens afterwards: the gateway re-checks the request's fences
//! and grants on a fresh snapshot before publishing a query result, so a revocation that commits
//! before that point denies the output (bytes already published cannot be recalled).
//!
//! An open handle prevents pathname substitution from redirecting the read; it does not prevent
//! in-place mutation of the same file. Hard links are refused by default because a link inside
//! the root can name content stored outside it.
use super::destructive::FileIdentity;
use std::path::PathBuf;

/// A registered read root with the identity it had when registered (if the platform has one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadRoot {
    pub location: PathBuf,
    pub identity: Option<FileIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadRequest<'a> {
    pub root: &'a ReadRoot,
    /// Relative path of normal components only.
    pub relative: &'a str,
    pub max_bytes: u64,
    pub allow_hard_links: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedBytes {
    pub bytes: Vec<u8>,
    pub identity: Option<FileIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadRefusal {
    /// Absolute, empty, `.`/`..` or prefixed paths.
    Escape,
    /// A component is a symlink, junction or other reparse point.
    LinkRefused,
    NotRegularFile,
    HardLinked,
    TooLarge {
        limit: u64,
    },
    /// The root no longer has its registered identity (moved, replaced or a link).
    RootChanged,
    /// The path no longer names the opened file (swapped between open and verification).
    Changed,
    NotFound,
    Unsupported(String),
    Io(String),
}

/// A verified open file for streaming. It yields at most `max_bytes + 1` bytes, so a caller that
/// receives more than `max_bytes` knows the source exceeds the bound and refuses it.
pub struct VerifiedFile {
    pub reader: Box<dyn std::io::Read + Send>,
    pub identity: Option<FileIdentity>,
}

/// The native read adapter.
pub trait VerifiedReader: Send + Sync {
    fn read(&self, request: &ReadRequest<'_>) -> Result<VerifiedBytes, ReadRefusal>;
    /// Opens the file with exactly the checks of [`Self::read`] and returns it for bounded
    /// streaming instead of buffering it. The default buffers through [`Self::read`].
    fn open(&self, request: &ReadRequest<'_>) -> Result<VerifiedFile, ReadRefusal> {
        let v = self.read(request)?;
        Ok(VerifiedFile {
            reader: Box::new(std::io::Cursor::new(v.bytes)),
            identity: v.identity,
        })
    }
}
