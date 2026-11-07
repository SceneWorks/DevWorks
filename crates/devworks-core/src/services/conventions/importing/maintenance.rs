use super::*;
use crate::{
    ports::Preparation,
    protocol::{OperationId, OperationState},
};

/// Progress for one bounded private-capture maintenance pass.
pub struct ImportCleanup {
    /// Exclusive keyset position; `None` starts a fresh cycle on the next pass.
    pub next: Option<OperationId>,
    /// Private blob references were released; the normal collector may now reclaim them.
    pub released: bool,
}

impl ImportService {
    /// Examine at most 32 imports and release at most 32 captures from each terminal owner.
    /// Active parser checkpoints, caller inputs and published/history artifacts are untouched.
    pub fn cleanup_terminal_page(
        &self,
        after: Option<OperationId>,
    ) -> Result<ImportCleanup, StorageError> {
        if self.is_stopped() {
            return Err(StorageError::Cancelled);
        }
        let rows = Preparation::new(self.store.as_ref(), None, None).read(|read| {
            read.conventions()
                .convention_imports_pending(after.as_ref(), 32)
        })?;
        let next = (rows.len() == 32)
            .then(|| rows.last().map(|row| row.operation_id.clone()))
            .flatten();
        let mut released = false;
        for row in rows {
            released |= self.write(move |tx| {
                let id = row.operation_id;
                let Some(mut draft) = tx.conventions().convention_import(&id)? else {
                    return Ok(false);
                };
                let Some(owner) = tx.operations().operation(&id)? else {
                    return Ok(false);
                };
                // A worker can mark its draft before its journal settles. Never release a
                // live owner's bytes, or turn a resumable parser into a new/restarted import.
                if !matches!(
                    owner.state,
                    OperationState::Failed
                        | OperationState::NeedsInput
                        | OperationState::CancelledBeforeDispatch
                        | OperationState::ObservedComplete
                ) {
                    return Ok(false);
                }
                if matches!(draft.phase.as_str(), "Queued" | "Parsing" | "Ready") {
                    let phase = match owner.state {
                        OperationState::Failed => Some("Failed"),
                        OperationState::CancelledBeforeDispatch => Some("Cancelled"),
                        _ => None,
                    };
                    if let Some(phase) = phase {
                        draft = tx.conventions().set_convention_import_phase(
                            &id,
                            draft.revision,
                            phase,
                            None,
                            Some("Owning import operation ended before publication"),
                            None,
                        )?;
                    }
                }
                if !matches!(draft.phase.as_str(), "Failed" | "Cancelled") {
                    return Ok(false);
                }
                let captures = tx
                    .conventions()
                    .convention_import_artifacts(&id, None, 32)?;
                if captures.is_empty() {
                    return Ok(false);
                }
                tx.conventions()
                    .discard_convention_import_artifacts(&id, 32)?;
                for digest in captures
                    .into_iter()
                    .map(|row| row.version.blob_digest)
                    .collect::<std::collections::BTreeSet<_>>()
                {
                    crate::services::artifacts::release_blob(tx, &digest)?;
                }
                Ok(true)
            })?;
        }
        Ok(ImportCleanup { next, released })
    }
}
