//! The durable consumer that continues a Knowledge delete by source past its first transaction.
//!
//! A source too large for one short write transaction is deleted as a job of bounded batches
//! (`knowledge::DELETION_BATCH` claims each). The confirming command commits the first batch,
//! the job record (which fences the source read-only) and a queued delivery for this consumer;
//! every delivery runs one batch in its own transaction, together with the next delivery, so
//! each batch is a checkpoint. A delivery claimed when the process stopped is reclaimed by
//! [`crate::Dispatcher::recover`] at the next boot, so an interrupted job resumes there.
use std::sync::Arc;

use devworks_core::ports::storage::{StorageError, WriteTransaction};
use devworks_core::protocol::Id;
use devworks_core::services::knowledge::{KnowledgeService, SOURCE_DELETION_CONSUMER};

use crate::dispatch::{Consumer, Delivery, DeliveryHandler, Outcome, RetryPolicy};

struct SourceDeletion(Arc<KnowledgeService>);

impl DeliveryHandler for SourceDeletion {
    fn apply(
        &self,
        tx: &mut dyn WriteTransaction,
        delivery: &Delivery,
    ) -> Result<Outcome, StorageError> {
        let Some(write_id) = delivery.entry.payload["writeId"]
            .as_str()
            .and_then(|s| Id::new(s).ok())
        else {
            return Ok(Outcome::Poison(
                "the delivery names no deletion write".into(),
            ));
        };
        match self.0.continue_source_deletion(tx, &write_id) {
            Ok(_) => Ok(Outcome::Completed),
            // A defect is deterministic: retrying the same batch cannot succeed.
            Err(StorageError::InvalidRecord(reason)) => Ok(Outcome::Poison(reason)),
            Err(e) => Err(e),
        }
    }
}

/// The consumer to register with the dispatcher of a composition that serves Knowledge.
pub fn source_deletion_consumer(service: Arc<KnowledgeService>) -> Consumer {
    Consumer {
        name: SOURCE_DELETION_CONSUMER.into(),
        handler: Arc::new(SourceDeletion(service)),
        effect: None,
        async_handler: None,
        eligibility: None,
        stop_grace_ms: 5_000,
        retry: RetryPolicy::default(),
        // One batch at a time: the deletion takes the writer in short turns, never all of it.
        max_in_flight: Some(1),
    }
}
