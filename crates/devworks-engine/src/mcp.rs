//! Durable outgoing MCP connection effects. Dispatch owns the attempt and recovery state;
//! the core service rechecks the saved configuration and authority before native I/O.
use crate::dispatch::{
    Consumer, Delivery, DeliveryHandler, EffectExecutor, EffectFuture, EffectResult, Outcome,
    RetryPolicy,
};
use devworks_core::ports::storage::{OperationRecord, StorageError, WriteTransaction};
use devworks_core::services::mcp::{EFFECT_CONSUMER, McpClientService, McpEffectOutcome};
use std::sync::Arc;

struct Effects(Arc<McpClientService>);

fn result(outcome: McpEffectOutcome) -> EffectResult {
    match outcome {
        McpEffectOutcome::Completed(value) => EffectResult::Completed(value),
        McpEffectOutcome::Failed(cause) => EffectResult::Failed(cause),
        McpEffectOutcome::NotApplied(cause) => EffectResult::NotApplied(cause),
        McpEffectOutcome::Unknown(cause) => EffectResult::Unknown(cause),
    }
}

impl EffectExecutor for Effects {
    fn execute(&self, operation: OperationRecord) -> EffectFuture {
        let service = self.0.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || service.execute(&operation)).await {
                Ok(outcome) => result(outcome),
                Err(_) => EffectResult::Unknown("outgoing MCP effect panicked".into()),
            }
        })
    }

    fn observe(&self, operation: OperationRecord) -> EffectFuture {
        let service = self.0.clone();
        // Observation may close a native session, which blocks; keep it off the async workers.
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || service.observe(&operation)).await {
                Ok(outcome) => result(outcome),
                Err(_) => EffectResult::Unknown("outgoing MCP observation panicked".into()),
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

pub fn consumer(service: Arc<McpClientService>) -> Consumer {
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
        max_in_flight: None,
    }
}
