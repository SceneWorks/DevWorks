//! The existing dispatcher supervises native local document work and renews its queue lease.
use crate::{
    dispatch::{
        AsyncDelivery, AsyncDeliveryHandler, AsyncFuture, AsyncOutcome, Consumer, Delivery,
        DeliveryHandler, Outcome, RetryPolicy,
    },
    effect_consumer::STANDARD_RETRY,
};
use devworks_core::{
    ports::storage::{StorageError, WriteTransaction},
    services::documents::indexing::{CONSUMER, DocumentIndexer, DocumentProcessor},
};
use std::sync::Arc;

#[derive(Clone)]
pub struct DocumentComposition {
    pub processor: Arc<dyn DocumentProcessor>,
}

impl std::fmt::Debug for DocumentComposition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentComposition")
            .finish_non_exhaustive()
    }
}

struct IndexWorker(Arc<DocumentIndexer>);

impl AsyncDeliveryHandler for IndexWorker {
    fn start(&self, job: AsyncDelivery) -> AsyncFuture {
        let indexer = self.0.clone();
        Box::pin(async move {
            let result = tokio::task::spawn_blocking(move || {
                indexer.run(&job.delivery.entry, &|| *job.cancelled.borrow())
            })
            .await;
            match result {
                Ok(Ok(())) => AsyncOutcome::Committed,
                Ok(Err(StorageError::InvalidRecord(_))) => {
                    AsyncOutcome::Poison("Invalid document index checkpoint".into())
                }
                Ok(Err(_)) | Err(_) => {
                    AsyncOutcome::Retry("Document indexing was interrupted".into())
                }
            }
        })
    }
}

impl DeliveryHandler for IndexWorker {
    fn apply(&self, _: &mut dyn WriteTransaction, _: &Delivery) -> Result<Outcome, StorageError> {
        Ok(Outcome::Hold(
            "Document indexing requires the asynchronous worker".into(),
        ))
    }
}

pub fn consumer(indexer: Arc<DocumentIndexer>) -> Consumer {
    let handler = Arc::new(IndexWorker(indexer));
    Consumer {
        name: CONSUMER.into(),
        handler: handler.clone(),
        async_handler: Some(handler),
        effect: None,
        eligibility: None,
        stop_grace_ms: 5_000,
        retry: RetryPolicy {
            max_attempts: i64::MAX,
            ..STANDARD_RETRY
        },
        max_in_flight: Some(2),
    }
}
