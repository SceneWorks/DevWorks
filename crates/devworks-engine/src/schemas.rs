//! One supervised metadata-only consumer. The shared core owns admission, current source
//! authority, immutable staging and publication; this layer owns execution and cancellation.
use crate::dispatch::{
    Consumer, Delivery, DeliveryHandler, EffectExecutor, EffectFuture, EffectResult, Outcome,
    RetryPolicy,
};
use devworks_core::{
    ports::{database::MetadataFailure, storage::*},
    services::schemas::{
        SchemaService,
        indexing::{EFFECT_CONSUMER, SchemaOutcome},
    },
};
use std::{sync::Arc, time::Duration};
struct Effects {
    store: Arc<dyn ApplicationStore>,
    service: Arc<SchemaService>,
}
struct CancelOnDrop(Cancellation);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
impl Effects {
    fn run(&self, operation: OperationRecord) -> EffectFuture {
        let (store, service) = (self.store.clone(), self.service.clone());
        Box::pin(async move {
            let token = Cancellation::new();
            let _cancel = CancelOnDrop(token.clone());
            let (worker_service, worker_token) = (service.clone(), token.clone());
            let delivery = serde_json::from_value::<devworks_core::protocol::Id>(
                operation.payload["deliveryId"].clone(),
            )
            .ok();
            let handle = tokio::runtime::Handle::current();
            let worker = tokio::task::spawn_blocking(move || {
                let result =
                    handle.block_on(worker_service.run_metadata(&operation, &worker_token));
                match result {
                    Ok(SchemaOutcome::Completed(value)) => EffectResult::Completed(value),
                    Ok(SchemaOutcome::Failed(reason)) => EffectResult::Failed(reason),
                    Err(
                        StorageError::CommitUnknown
                        | StorageError::Busy
                        | StorageError::StorageUnavailable { .. },
                    ) => EffectResult::Unknown(
                        "Metadata publication interrupted; observe the original operation".into(),
                    ),
                    Err(_) if worker_service.is_stopped() => EffectResult::Unknown(
                        "Metadata owner stopped; original operation remains recoverable".into(),
                    ),
                    Err(error) => {
                        let failure = if matches!(error, StorageError::Cancelled) {
                            MetadataFailure::Cancelled
                        } else {
                            MetadataFailure::AccessDenied
                        };
                        match worker_service.fail_metadata(&operation,failure){
                            Ok(())=>EffectResult::Failed(format!("Metadata{failure:?}")),
                            Err(_)=>EffectResult::Unknown("Metadata status acknowledgement interrupted; observe the original operation".into()),
                        }
                    }
                }
            });
            loop {
                if worker.is_finished() {
                    return worker.await.unwrap_or_else(|_| {
                        EffectResult::Unknown(
                            "Metadata worker stopped before acknowledgement".into(),
                        )
                    });
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                let (owner, id) = (store.clone(), delivery.clone());
                let cancelled = tokio::task::spawn_blocking(move || {
                    read_snapshot(owner.as_ref(), &ReadOptions::default(), |r| match id {
                        Some(id) => Ok(r.queue().entry(&id)?.is_none_or(|q| q.cancel_requested)),
                        None => Ok(true),
                    })
                })
                .await;
                if service.is_stopped() || !matches!(cancelled, Ok(Ok(false))) {
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
    fn observe(&self, operation: OperationRecord) -> EffectFuture {
        self.run(operation)
    }
}
struct Settle(Arc<SchemaService>);
impl DeliveryHandler for Settle {
    fn apply(&self, _: &mut dyn WriteTransaction, _: &Delivery) -> Result<Outcome, StorageError> {
        if self.0.is_stopped() {
            Err(StorageError::Cancelled)
        } else {
            Ok(Outcome::Completed)
        }
    }
}
pub fn consumer(store: Arc<dyn ApplicationStore>, service: Arc<SchemaService>) -> Consumer {
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

#[derive(Clone)]
pub struct SchemaComposition {
    pub reader: Arc<dyn devworks_core::ports::database::MetadataReader>,
    pub secrets: Arc<dyn devworks_core::ports::secrets::SecretStore>,
}
impl std::fmt::Debug for SchemaComposition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchemaComposition").finish_non_exhaustive()
    }
}
