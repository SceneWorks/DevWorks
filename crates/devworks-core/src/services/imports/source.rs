use super::{MAX_EXPORT_BYTES, Snapshot, SnapshotError};
use crate::authority::destructive::{EntryKind, FileIdentity, FilesystemResolver};
use crate::authority::reads::{ReadRefusal, ReadRequest, ReadRoot, VerifiedReader};
use crate::ports::storage::StorageError;
use crate::protocol::ProjectId;
use crate::protocol::records::RegisteredPath;
use crate::services::process::tools::ToolBindings;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone)]
pub struct ImportSources {
    bindings: ToolBindings,
    filesystem: Arc<dyn FilesystemResolver>,
    reader: Arc<dyn VerifiedReader>,
}

impl std::fmt::Debug for ImportSources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportSources").finish_non_exhaustive()
    }
}

pub(super) struct Captured {
    pub snapshot: Snapshot,
    pub root_identity: Option<FileIdentity>,
}

impl ImportSources {
    pub fn new(
        bindings: ToolBindings,
        filesystem: Arc<dyn FilesystemResolver>,
        reader: Arc<dyn VerifiedReader>,
    ) -> Self {
        Self {
            bindings,
            filesystem,
            reader,
        }
    }

    pub(super) fn root_identity(
        &self,
        path: &RegisteredPath,
    ) -> Result<Option<FileIdentity>, StorageError> {
        let location = self
            .bindings
            .root_directory(&path.root_id, None)
            .map_err(|_| StorageError::ScopeDenied)?;
        self.resolve_root(&location)
    }

    fn resolve_root(&self, location: &Path) -> Result<Option<FileIdentity>, StorageError> {
        let resolved = self
            .filesystem
            .resolve(location.to_str().ok_or(StorageError::ScopeDenied)?, None)
            .map_err(|_| StorageError::ScopeDenied)?;
        if resolved.parent_traversal
            || resolved.entry_kind != EntryKind::Directory
            || resolved.entry.identity.is_none()
        {
            return Err(StorageError::ScopeDenied);
        }
        Ok(resolved.entry.identity)
    }

    pub(super) fn capture(
        &self,
        path: &RegisteredPath,
        project: Option<&ProjectId>,
        stopped: &dyn Fn() -> bool,
    ) -> Result<Captured, StorageError> {
        let location = self
            .bindings
            .root_directory(&path.root_id, project)
            .map_err(|_| StorageError::ScopeDenied)?;
        let identity = self.resolve_root(&location)?;
        let root = ReadRoot {
            location: location.clone(),
            identity,
        };
        let mut file = self
            .reader
            .open(&ReadRequest {
                root: &root,
                relative: &path.relative.0,
                max_bytes: MAX_EXPORT_BYTES as u64,
                allow_hard_links: false,
            })
            .map_err(read_refused)?;
        let mut bytes = Vec::new();
        let mut block = [0; 64 * 1024];
        loop {
            if stopped() {
                return Err(StorageError::Cancelled);
            }
            let read = file
                .reader
                .read(&mut block)
                .map_err(|_| StorageError::ViewUnavailable)?;
            if read == 0 {
                break;
            }
            if bytes.len().saturating_add(read) > MAX_EXPORT_BYTES {
                return Err(StorageError::InvalidRecord(
                    "import snapshot too large".into(),
                ));
            }
            bytes.extend_from_slice(&block[..read]);
        }
        if identity != self.resolve_root(&location)? {
            return Err(StorageError::ScopeDenied);
        }
        let snapshot = Snapshot::parse(&bytes).map_err(|error| match error {
            SnapshotError::UnsupportedFormat => {
                StorageError::IncompatibleSchema("unsupported CodeGraph source snapshot".into())
            }
            error => StorageError::InvalidRecord(format!("import snapshot {error:?}")),
        })?;
        Ok(Captured {
            snapshot,
            root_identity: identity,
        })
    }
}

fn read_refused(error: ReadRefusal) -> StorageError {
    match error {
        ReadRefusal::TooLarge { .. } => {
            StorageError::InvalidRecord("import snapshot too large".into())
        }
        ReadRefusal::NotFound => StorageError::ViewUnavailable,
        ReadRefusal::Io(_) => StorageError::ViewUnavailable,
        _ => StorageError::ScopeDenied,
    }
}
