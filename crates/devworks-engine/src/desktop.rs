//! Serial, journaled desktop input. An unknown result is observed as unknown: replaying a
//! possible click or keypress would act twice on the user's desktop.
use crate::dispatch::{
    Consumer, Delivery, DeliveryHandler, EffectExecutor, EffectFuture, EffectResult, Outcome,
    RetryPolicy,
};
use devworks_core::ports::storage::{OperationRecord, StorageError, WriteTransaction};
use devworks_core::services::desktop::{DesktopService, INPUT_CONSUMER, InputEffectOutcome};
use std::sync::Arc;

struct InputEffects(Arc<DesktopService>);
fn result(outcome: InputEffectOutcome) -> EffectResult {
    match outcome {
        InputEffectOutcome::Completed(value) => EffectResult::Completed(value),
        InputEffectOutcome::Failed(reason) => EffectResult::Failed(reason),
        InputEffectOutcome::Unknown(reason) => EffectResult::Unknown(reason),
    }
}
impl EffectExecutor for InputEffects {
    fn execute(&self, operation: OperationRecord) -> EffectFuture {
        let service = self.0.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || service.execute_input(&operation)).await {
                Ok(outcome) => result(outcome),
                Err(_) => EffectResult::Unknown("desktop input worker panicked".into()),
            }
        })
    }
    fn observe(&self, operation: OperationRecord) -> EffectFuture {
        let service = self.0.clone();
        Box::pin(async move { result(service.observe_input(&operation)) })
    }
}

struct Settle;
impl DeliveryHandler for Settle {
    fn apply(
        &self,
        _tx: &mut dyn WriteTransaction,
        _delivery: &Delivery,
    ) -> Result<Outcome, StorageError> {
        Ok(Outcome::Completed)
    }
}

pub fn consumer(service: Arc<DesktopService>) -> Consumer {
    Consumer {
        name: INPUT_CONSUMER.into(),
        handler: Arc::new(Settle),
        effect: Some(Arc::new(InputEffects(service))),
        async_handler: None,
        eligibility: None,
        stop_grace_ms: 5_000,
        retry: RetryPolicy {
            base_delay_ms: 500,
            max_delay_ms: 500,
            max_attempts: 1,
        },
        max_in_flight: Some(1),
    }
}
