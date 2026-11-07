//! Cancellation follows the accepted owner's durable queue identity, never a guessed key.
use crate::{
    ports::storage::*,
    protocol::{Id, OperationId},
};
pub(crate) fn request_cancel(
    tx: &mut dyn WriteTransaction,
    operation: &OperationId,
) -> Result<bool, StorageError> {
    let Some(record) = tx.operations().operation(operation)? else {
        return Ok(false);
    };
    if is_terminal(record.state)
        || record.capability != "conventions.index"
        || record.schema_version != 1
        || record.payload["kind"] != super::commands::JOB_KIND
    {
        return Ok(false);
    }
    let delivery: Id =
        serde_json::from_value(record.payload["deliveryId"].clone()).map_err(|_| {
            StorageError::InvalidRecord("invalid convention index delivery identity".into())
        })?;
    let entry = tx
        .queue()
        .entry(&delivery)?
        .ok_or_else(|| StorageError::InvalidRecord("convention index delivery missing".into()))?;
    if entry.consumer != super::commands::EFFECT_CONSUMER
        || entry.operation_id.as_ref() != Some(operation)
    {
        return Err(StorageError::ScopeDenied);
    }
    if entry.cancel_requested {
        return Ok(false);
    }
    tx.queue().cancel(&delivery)?;
    Ok(true)
}
