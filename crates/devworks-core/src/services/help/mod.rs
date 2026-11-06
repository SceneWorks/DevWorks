//! One offline Help owner for native UI, agents and the scheduled regeneration producer.
use crate::operations::help::GenerateCapabilityDocsInput;
use crate::ports::storage::{ReadSnapshot, StorageError, WriteTransaction};
use crate::protocol::Revision;

fn generation_args(registry: i64, help: i64) -> Result<GenerateCapabilityDocsInput, StorageError> {
    Ok(GenerateCapabilityDocsInput {
        registry_revision: Revision::new(registry).map_err(|_| {
            StorageError::InvalidRecord("invalid capability registry revision".into())
        })?,
        expected_help_revision: Revision::new(help)
            .map_err(|_| StorageError::InvalidRecord("invalid Help revision".into()))?,
    })
}

/// Captures the current source revisions for an explicitly current-generation host request.
/// This never replaces explicitly authored arguments or grants permission to run the command.
pub fn current_generation_args(
    read: &dyn ReadSnapshot,
) -> Result<GenerateCapabilityDocsInput, StorageError> {
    generation_args(
        read.packages().registry_revision()?,
        read.help().help_head()?.revision,
    )
}

/// The scheduled job producer captures these arguments in its existing run transaction before
/// dispatch, then replays the stored invocation rather than resolving a new scope on retry.
pub fn current_generation_args_in(
    tx: &mut dyn WriteTransaction,
) -> Result<GenerateCapabilityDocsInput, StorageError> {
    let registry = tx.packages().registry_revision()?;
    generation_args(registry, tx.help().help_head()?.revision)
}
