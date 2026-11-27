//! The durable consumer that continues a Knowledge delete by source past its first transaction.
//!
//! A source too large for one short write transaction is deleted as a job of bounded batches
//! (`knowledge::DELETION_BATCH` claims each). The confirming command commits the first batch,
//! the job record (which fences the source read-only) and a queued delivery for this consumer;
//! every delivery runs one batch in its own transaction, together with the next delivery, so
//! each batch is a checkpoint. A delivery claimed when the process stopped is reclaimed by
//! [`crate::Dispatcher::recover`] at the next boot, so an interrupted job resumes there.
use std::sync::Arc;

use devworks_core::ports::storage::{Cancellation, StorageError, WriteTransaction};
use devworks_core::protocol::Id;
use devworks_core::services::knowledge::{
    KnowledgeService, SOURCE_DELETION_CONSUMER, VECTOR_CONSUMER,
};

use crate::dispatch::{
    AsyncDelivery, AsyncDeliveryHandler, AsyncFuture, AsyncOutcome, Consumer, Delivery,
    DeliveryHandler, Outcome, RetryPolicy,
};
use crate::effect_consumer::STANDARD_RETRY;

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

/// Embeds the claims of a written scope off the dispatcher runtime (provider calls never run on
/// it or inside a write) while the dispatcher renews the claim's lease.
struct ClaimVectors(Arc<KnowledgeService>);

impl AsyncDeliveryHandler for ClaimVectors {
    fn start(&self, job: AsyncDelivery) -> AsyncFuture {
        let service = self.0.clone();
        Box::pin(async move {
            let mut cancelled = job.cancelled;
            let cancellation = Cancellation::new();
            if *cancelled.borrow() {
                cancellation.cancel();
            }
            let token = cancellation.clone();
            let mut work = tokio::task::spawn_blocking(move || {
                service.embed_claims(&job.delivery.entry, &token)
            });
            let result = loop {
                tokio::select! {
                    result = &mut work => break result,
                    changed = cancelled.changed() => {
                        if changed.is_err() || *cancelled.borrow() {
                            cancellation.cancel();
                            break work.await;
                        }
                    }
                }
            };
            match result {
                Ok(Ok(())) => AsyncOutcome::Committed,
                Ok(Err(StorageError::InvalidRecord(reason))) => AsyncOutcome::Poison(reason),
                Ok(Err(_)) | Err(_) => {
                    AsyncOutcome::Retry("Embedding claims was interrupted".into())
                }
            }
        })
    }
}

impl DeliveryHandler for ClaimVectors {
    fn apply(&self, _: &mut dyn WriteTransaction, _: &Delivery) -> Result<Outcome, StorageError> {
        Ok(Outcome::Hold(
            "Embedding claims requires the asynchronous worker".into(),
        ))
    }
}

/// The consumer that embeds claims ([`VECTOR_CONSUMER`]), for a composition whose Knowledge
/// service has the shared embedding runtime.
pub fn claim_vector_consumer(service: Arc<KnowledgeService>) -> Consumer {
    let handler = Arc::new(ClaimVectors(service));
    Consumer {
        name: VECTOR_CONSUMER.into(),
        handler: handler.clone(),
        async_handler: Some(handler),
        effect: None,
        eligibility: None,
        stop_grace_ms: 5_000,
        retry: STANDARD_RETRY,
        // One delivery at a time: two writes of a scope never embed the same claim twice.
        max_in_flight: Some(1),
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
