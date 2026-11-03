//! The receipt reply consumer: runs core's
//! [`ReceiptReplies`](devworks_core::services::artifacts::replies::ReceiptReplies) publisher for
//! every claimed [`REPLY_CONSUMER`] delivery through the dispatcher's asynchronous path. The
//! claim's lease is renewed while the reply file is written outside any transaction; the
//! publisher's own write re-checks the claim fence and cancellation, records the reply and
//! settles the delivery in one transaction. A failure is retried with durable backoff, a claim
//! interrupted by a crash is redelivered after restart, and the publisher is idempotent per
//! receipt, so recovery never publishes twice.
use crate::dispatch::{
    AsyncDelivery, AsyncDeliveryHandler, AsyncFuture, AsyncOutcome, Consumer, Delivery,
    DeliveryHandler, Outcome, RetryPolicy,
};
use devworks_core::ports::storage::{ApplicationStore, StorageError, WriteTransaction};
use devworks_core::services::artifacts::replies::{REPLY_CONSUMER, ReceiptReplies, ReplyOutcome};
use std::sync::Arc;

/// Publishes receipt replies for the dispatcher.
pub struct ReplyPublisher {
    replies: Arc<ReceiptReplies>,
    store: Arc<dyn ApplicationStore>,
}

impl AsyncDeliveryHandler for ReplyPublisher {
    fn start(&self, delivery: AsyncDelivery) -> AsyncFuture {
        let (replies, store) = (self.replies.clone(), self.store.clone());
        let entry = delivery.delivery.entry;
        Box::pin(async move {
            // The publisher blocks on file and store I/O: keep it off the dispatcher's runtime.
            let published =
                tokio::task::spawn_blocking(move || replies.publish(&*store, &entry)).await;
            match published {
                Ok(ReplyOutcome::Published(_) | ReplyOutcome::Existing(_)) => {
                    AsyncOutcome::Committed
                }
                // The claim is no longer ours: the dispatcher finds it gone (or cancelled).
                Ok(ReplyOutcome::Stale) => AsyncOutcome::Retry("the claim was lost".into()),
                Ok(ReplyOutcome::Retry(cause)) => AsyncOutcome::Retry(cause),
                Ok(ReplyOutcome::Hold(cause)) => AsyncOutcome::Hold(cause),
                Ok(ReplyOutcome::Poison(cause)) => AsyncOutcome::Poison(cause),
                Err(_) => AsyncOutcome::Retry("the reply publisher task failed".into()),
            }
        })
    }
}

/// Deliveries of this consumer always run through [`ReplyPublisher`]; the synchronous path is
/// never taken for it.
struct AsyncOnly;
impl DeliveryHandler for AsyncOnly {
    fn apply(&self, _: &mut dyn WriteTransaction, _: &Delivery) -> Result<Outcome, StorageError> {
        Ok(Outcome::Hold(
            "receipt replies are published asynchronously".into(),
        ))
    }
}

/// The [`REPLY_CONSUMER`] registration for the dispatcher.
pub fn reply_consumer(
    replies: Arc<ReceiptReplies>,
    store: Arc<dyn ApplicationStore>,
    retry: RetryPolicy,
) -> Consumer {
    Consumer {
        name: REPLY_CONSUMER.into(),
        handler: Arc::new(AsyncOnly),
        effect: None,
        async_handler: Some(Arc::new(ReplyPublisher { replies, store })),
        eligibility: None,
        stop_grace_ms: 5_000,
        retry,
    }
}
