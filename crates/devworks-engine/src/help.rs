//! Offline Help publication uses the application operation journal and durable queue.
use crate::dispatch::{
    Consumer, Delivery, DeliveryHandler, EffectExecutor, EffectFuture, EffectResult, Outcome,
    RetryPolicy,
};
use devworks_core::ports::storage::{OperationRecord, StorageError, WriteTransaction};
use devworks_core::services::help::{EFFECT_CONSUMER, GenerationOutcome, HelpService};
use std::sync::Arc;

struct Effects(Arc<HelpService>);
fn result(outcome: GenerationOutcome) -> EffectResult {
    match outcome {
        GenerationOutcome::Completed(result) => EffectResult::Completed(result),
        GenerationOutcome::Failed(reason) => EffectResult::Failed(reason),
        GenerationOutcome::Unknown(reason) => EffectResult::Unknown(reason),
    }
}
impl EffectExecutor for Effects {
    fn execute(&self, operation: OperationRecord) -> EffectFuture {
        let service = self.0.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || service.execute(&operation)).await {
                Ok(outcome) => result(outcome),
                Err(_) => EffectResult::Unknown("Help publication panicked after dispatch".into()),
            }
        })
    }
    fn observe(&self, operation: OperationRecord) -> EffectFuture {
        let service = self.0.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || service.observe(&operation)).await {
                Ok(outcome) => result(outcome),
                Err(_) => EffectResult::Unknown("Help observation panicked".into()),
            }
        })
    }
}
struct Settle;
impl DeliveryHandler for Settle {
    fn apply(&self, _: &mut dyn WriteTransaction, _: &Delivery) -> Result<Outcome, StorageError> {
        Ok(Outcome::Completed)
    }
}
pub fn consumer(service: Arc<HelpService>) -> Consumer {
    Consumer {
        name: EFFECT_CONSUMER.into(),
        handler: Arc::new(Settle),
        effect: Some(Arc::new(Effects(service))),
        async_handler: None,
        eligibility: None,
        stop_grace_ms: 5_000,
        retry: RetryPolicy {
            base_delay_ms: 500,
            max_delay_ms: 30_000,
            max_attempts: 4,
        },
        max_in_flight: Some(1),
    }
}
