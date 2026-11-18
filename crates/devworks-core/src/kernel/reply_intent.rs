//! The reply-publication intent the gateway records with every receipt, and the names the reply
//! publisher in `services::artifacts::replies` consumes it by.
use crate::context::ReceiptIdentity;
use crate::ports::IdKind;
use crate::ports::storage::{NewQueueEntry, StorageError, WriteTransaction};
use crate::protocol::{CommandId, EventId};
use serde_json::json;

/// Durable delivery consumer of receipt reply publication.
pub const REPLY_CONSUMER: &str = "receipt_replies";
/// History aggregate of a receipt's reply publication (`aggregate_id` = command ID).
pub const REPLY_AGGREGATE: &str = "receipt_reply";

/// Records the publication intent of a receipt inside the command's unit of work: one unordered
/// delivery for the receipt's `CommandDecided` event. Called only by the gateway, right after it
/// recorded the receipt.
pub fn record_intent(
    tx: &mut dyn WriteTransaction,
    identity: &ReceiptIdentity,
    command: &CommandId,
    event: &EventId,
) -> Result<(), StorageError> {
    let id = tx.new_id(IdKind::QueueEntry);
    let ready_at_ms = tx.now_ms();
    tx.queue().enqueue(NewQueueEntry {
        id,
        consumer: REPLY_CONSUMER.into(),
        event_id: event.clone(),
        // Not root-scoped: a reply is bookkeeping of a decision already made, so its delivery is
        // never held for root eligibility, and it carries no operation (nothing external runs).
        root_id: None,
        operation_id: None,
        partition: None,
        priority: 0,
        ready_at_ms,
        payload: json!({
            "actorId": identity.actor_id().0.as_str(),
            "scopeKey": identity.scope_key().as_str(),
            "requestKey": identity.request_key().0.as_str(),
            "commandId": command.0.as_str(),
        }),
    })?;
    Ok(())
}
