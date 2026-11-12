//! Durable process-tool effects share the existing owned-process supervisor. Dispatch records
//! the attempt before launch; observation never launches a replacement after an unknown result.
use crate::dispatch::{
    Consumer, Delivery, DeliveryHandler, EffectExecutor, EffectFuture, EffectResult, Outcome,
    RetryPolicy,
};
use devworks_core::ports::storage::{OperationRecord, StorageError, WriteTransaction};
use devworks_core::services::process::tools::{
    CANCEL_CONSUMER, EFFECT_CONSUMER, EffectOutcome, ProcessToolService, WATCH_CONSUMER,
};
use std::sync::Arc;

struct Effects(Arc<ProcessToolService>);
fn outcome(value: EffectOutcome) -> EffectResult {
    match value {
        EffectOutcome::Completed(value) => EffectResult::Completed(value),
        EffectOutcome::Failed(reason) => EffectResult::Failed(reason),
        EffectOutcome::Unknown(reason) => EffectResult::Unknown(reason),
    }
}
impl EffectExecutor for Effects {
    fn execute(&self, operation: OperationRecord) -> EffectFuture {
        let service = self.0.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || service.execute_detached(&operation)).await {
                Ok(value) => outcome(value),
                Err(_) => EffectResult::Unknown("process effect panicked".into()),
            }
        })
    }
    fn observe(&self, operation: OperationRecord) -> EffectFuture {
        let service = self.0.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || service.observe(&operation)).await {
                Ok(value) => outcome(value),
                Err(_) => EffectResult::Unknown("process observation panicked".into()),
            }
        })
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

pub fn consumer(service: Arc<ProcessToolService>) -> Consumer {
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
        max_in_flight: Some(2),
    }
}

/// Cancellation is isolated from long-running execute/install slots, so a full worker pool
/// cannot hold a stop request behind the processes it needs to stop.
pub fn cancel_consumer(service: Arc<ProcessToolService>) -> Consumer {
    let mut consumer = consumer(service);
    consumer.name = CANCEL_CONSUMER.into();
    consumer
}

/// Watched processes run long by design (a dev server, a build). Their effect ends once the
/// process is launched (a monitor thread records the outcome), so they hold a dispatcher worker
/// only for the launch and never starve cancellation or other consumers.
pub fn watch_consumer(service: Arc<ProcessToolService>) -> Consumer {
    let mut consumer = consumer(service);
    consumer.name = WATCH_CONSUMER.into();
    consumer
}
