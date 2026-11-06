//! Supervised body indexing over the one durable operation and native inference journals.
use crate::dispatch::{
    Consumer, Delivery, DeliveryHandler, EffectExecutor, EffectFuture, EffectResult, Outcome,
    RetryPolicy,
};
use devworks_core::{
    ports::storage::*,
    services::conventions::indexing::{
        IndexService,
        commands::{EFFECT_CONSUMER, JOB_KIND},
        publisher::IndexOutcome,
    },
};
use std::sync::Arc;
use std::time::Duration;

struct Effects {
    store: Arc<dyn ApplicationStore>,
    service: Arc<IndexService>,
}
fn outcome(value: Result<IndexOutcome, StorageError>) -> EffectResult {
    match value {
        Ok(IndexOutcome::Completed(value)) => EffectResult::Completed(value),
        Ok(IndexOutcome::Failed(reason)) => EffectResult::Failed(reason),
        Ok(IndexOutcome::Unknown(reason)) => EffectResult::Unknown(reason),
        Err(
            StorageError::Cancelled
            | StorageError::Busy
            | StorageError::CommitUnknown
            | StorageError::StorageUnavailable { .. },
        ) => EffectResult::Unknown(
            "Index publication interrupted; observe the original operation".into(),
        ),
        Err(error) => {
            EffectResult::Failed(format!("Index source is no longer eligible: {error:?}"))
        }
    }
}
impl Effects {
    fn run(&self, operation: OperationRecord) -> EffectFuture {
        let service = self.service.clone();
        let store = self.store.clone();
        Box::pin(async move {
            // Only a captured lexical-only body job can prove no native request could
            // have started. Paid/unknown tails keep their actual inference custody.
            let lexical_only = operation.payload["kind"] == JOB_KIND
                && operation.payload["embeddingSpace"].is_null();
            let token = Cancellation::new();
            let worker_token = token.clone();
            let delivery = serde_json::from_value::<devworks_core::protocol::Id>(
                operation.payload["deliveryId"].clone(),
            );
            let worker = tokio::task::spawn_blocking(move || {
                service.run_index(&operation, &worker_token, None)
            });
            loop {
                if worker.is_finished() {
                    let result = worker.await;
                    if lexical_only && matches!(&result, Ok(Err(StorageError::Cancelled))) {
                        let store = store.clone();
                        let delivery = delivery.as_ref().ok().cloned();
                        let explicit = tokio::task::spawn_blocking(move || {
                            read_snapshot(store.as_ref(), &ReadOptions::default(), |read| {
                                match delivery {
                                    Some(id) => Ok(read
                                        .queue()
                                        .entry(&id)?
                                        .is_some_and(|e| e.cancel_requested)),
                                    None => Ok(false),
                                }
                            })
                        })
                        .await;
                        if matches!(explicit, Ok(Ok(true))) {
                            return EffectResult::Failed("The lexical-only index was cancelled; no native request was selected.".into());
                        }
                    }
                    return result.map(outcome).unwrap_or_else(|_| {
                        EffectResult::Unknown("Index worker stopped before acknowledgement".into())
                    });
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                let store = store.clone();
                let delivery = delivery.as_ref().ok().cloned();
                let cancelled = tokio::task::spawn_blocking(move || {
                    read_snapshot(
                        store.as_ref(),
                        &ReadOptions::default(),
                        |read| match delivery {
                            Some(id) => {
                                Ok(read.queue().entry(&id)?.is_none_or(|e| e.cancel_requested))
                            }
                            None => Ok(true),
                        },
                    )
                })
                .await;
                if !matches!(cancelled, Ok(Ok(false))) {
                    token.cancel();
                }
            }
        })
    }
}
impl EffectExecutor for Effects {
    fn execute(&self, operation: OperationRecord) -> EffectFuture {
        self.run(operation)
    }
    // The core reads exact captured chunks and native receipts. Only proven NotStarted can
    // begin the original identity; an admitted unknown request is never sent again.
    fn observe(&self, operation: OperationRecord) -> EffectFuture {
        self.run(operation)
    }
}
struct Settle(Arc<IndexService>);
impl DeliveryHandler for Settle {
    fn apply(&self, _: &mut dyn WriteTransaction, _: &Delivery) -> Result<Outcome, StorageError> {
        if self.0.is_stopped() {
            return Err(StorageError::Cancelled);
        }
        Ok(Outcome::Completed)
    }
}
pub fn consumer(store: Arc<dyn ApplicationStore>, service: Arc<IndexService>) -> Consumer {
    Consumer {
        name: EFFECT_CONSUMER.into(),
        handler: Arc::new(Settle(service.clone())),
        effect: Some(Arc::new(Effects { store, service })),
        async_handler: None,
        eligibility: None,
        stop_grace_ms: 5000,
        retry: RetryPolicy::default(),
        max_in_flight: Some(1),
    }
}
