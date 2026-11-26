//! Workspace backup pieces the gateway and the storage port share with the artifact and
//! workspace archive services.
use crate::ports::storage::{StorageError, WriteTransaction};

/// Capability held by the artifact owner for its backup-specific protected writes. Only
/// `services::artifacts` mints it; `tests/dependency_direction.rs` checks that.
pub struct ArchivePinAuthority(());

impl ArchivePinAuthority {
    pub(crate) fn artifact_owner() -> Self {
        Self(())
    }
}

/// Whether a restored workspace still holds back the effect of `root` or `capability`.
pub(crate) fn restore_effect_held(
    tx: &mut dyn WriteTransaction,
    root: Option<&crate::protocol::RootId>,
    capability: Option<&str>,
) -> Result<bool, StorageError> {
    if tx.store().restore_epoch == 0 {
        return Ok(false);
    }
    match tx.workspace_archives() {
        Ok(read) => read.restore_effect_held(root, capability),
        Err(StorageError::ViewUnavailable) => Ok(false),
        Err(error) => Err(error),
    }
}
