//! Indexing adapters (DW-023): the native [`NativeSourceTree`] (read-only, no-follow, bounded
//! access to repository checkouts) and the Tree-sitter [`treesitter::TreeSitterParser`].
pub mod domain;
pub mod endpoints;
pub mod manifests;
pub mod treesitter;

use crate::fs::NativeFilesystem;
use devworks_core::authority::reads::{ReadRefusal, ReadRequest, ReadRoot, VerifiedReader};
use devworks_core::ports::indexing::{DirEntry, EntryKind, GitMarker, SourceFault, SourceTree};
use devworks_core::services::indexing::paths::native_location;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Native source tree access. File reads go through the verified reader (descriptor-relative,
/// no link at any component, a regular file only, re-verified after open, bounded), so a path
/// inside a repository can never read outside it.
#[derive(Default)]
pub struct NativeSourceTree {
    fs: NativeFilesystem,
}
impl NativeSourceTree {
    pub fn new() -> Self {
        Self::default()
    }
}

/// The identity form of a canonical path (see [`native_location`]): on Windows the verbatim
/// prefix `canonicalize` adds is dropped while the path stays within the classic length limit,
/// so registrations compare equal with the paths Git reports.
fn plain(p: PathBuf) -> PathBuf {
    match p.to_str() {
        Some(s) if cfg!(windows) => PathBuf::from(native_location(s, true)),
        _ => p,
    }
}

impl SourceTree for NativeSourceTree {
    fn canonical_dir(&self, path: &Path) -> Result<PathBuf, SourceFault> {
        let c = std::fs::canonicalize(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => SourceFault::Missing,
            _ => SourceFault::Io(e.kind().to_string()),
        })?;
        let meta = std::fs::metadata(&c).map_err(|e| SourceFault::Io(e.kind().to_string()))?;
        if !meta.is_dir() {
            return Err(SourceFault::Refused("not a directory".into()));
        }
        Ok(plain(c))
    }
    fn list_dir(&self, dir: &Path, limit: usize) -> Result<Vec<DirEntry>, SourceFault> {
        let rd = std::fs::read_dir(dir).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => SourceFault::Missing,
            _ => SourceFault::Io(e.kind().to_string()),
        })?;
        let mut out = Vec::new();
        for entry in rd.take(limit.saturating_mul(2).max(1)) {
            let Ok(entry) = entry else { continue };
            // `file_type` never follows a link (symlinks and Windows junctions report as links).
            let Ok(t) = entry.file_type() else { continue };
            let kind = if t.is_symlink() {
                EntryKind::Link
            } else if t.is_dir() {
                EntryKind::Directory
            } else if t.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            };
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            out.push(DirEntry { name, kind });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out.truncate(limit);
        Ok(out)
    }
    fn git_marker(&self, dir: &Path) -> Result<Option<GitMarker>, SourceFault> {
        let marker = dir.join(".git");
        match std::fs::symlink_metadata(&marker) {
            Ok(m) if m.file_type().is_symlink() => Ok(None),
            Ok(m) if m.is_dir() => Ok(Some(GitMarker::Directory)),
            Ok(m) if m.is_file() => {
                let mut text = String::new();
                let _ = std::fs::File::open(&marker)
                    .and_then(|f| f.take(4096).read_to_string(&mut text));
                Ok(Some(GitMarker::File(
                    text.trim()
                        .strip_prefix("gitdir:")
                        .unwrap_or("")
                        .trim()
                        .to_owned(),
                )))
            }
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(SourceFault::Io(e.kind().to_string())),
        }
    }
    fn read_file(
        &self,
        root: &Path,
        relative: &str,
        max_bytes: u64,
    ) -> Result<Vec<u8>, SourceFault> {
        // The root's identity as observed now: a root replaced during the read is refused. The
        // root itself must be a directory, never a link.
        let meta = std::fs::symlink_metadata(root).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => SourceFault::Missing,
            _ => SourceFault::Io(e.kind().to_string()),
        })?;
        if !meta.is_dir() {
            return Err(SourceFault::Refused("the root is not a directory".into()));
        }
        let root = ReadRoot {
            location: root.to_path_buf(),
            identity: crate::fs::identity_at(root, &meta),
        };
        let request = ReadRequest {
            root: &root,
            relative,
            max_bytes,
            allow_hard_links: false,
        };
        match self.fs.read(&request) {
            Ok(v) => Ok(v.bytes),
            Err(ReadRefusal::TooLarge { .. }) => {
                let size = std::fs::symlink_metadata(root.location.join(relative))
                    .map(|m| m.len())
                    .unwrap_or(max_bytes.saturating_add(1));
                Err(SourceFault::TooLarge { size })
            }
            Err(ReadRefusal::NotFound) => Err(SourceFault::Missing),
            Err(ReadRefusal::Io(e)) => Err(SourceFault::Io(e)),
            Err(other) => Err(SourceFault::Refused(format!("{other:?}"))),
        }
    }
    fn case_insensitive(&self, root: &Path) -> bool {
        if cfg!(windows) {
            return true;
        }
        // Probe the root's own name with its case swapped: the same directory means the file
        // system folds case (the macOS default).
        let Some(name) = root.file_name().and_then(|n| n.to_str()) else {
            return cfg!(target_os = "macos");
        };
        let swapped: String = name
            .chars()
            .map(|c| {
                if c.is_uppercase() {
                    c.to_lowercase().next().unwrap_or(c)
                } else {
                    c.to_uppercase().next().unwrap_or(c)
                }
            })
            .collect();
        if swapped == name {
            return cfg!(target_os = "macos");
        }
        let other = root.with_file_name(swapped);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            match (std::fs::metadata(root), std::fs::metadata(&other)) {
                (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
                _ => false,
            }
        }
        #[cfg(not(unix))]
        {
            other.exists()
        }
    }
    fn home(&self) -> Option<PathBuf> {
        crate::fs::home_dir()
    }
}
