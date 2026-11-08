//! Durable container effects share the process supervisor and exact current authority.
use crate::dispatch::{
    Consumer, Delivery, DeliveryHandler, EffectExecutor, EffectFuture, EffectResult, Outcome,
    RetryPolicy,
};
use devworks_core::ports::AuthorityPort;
use devworks_core::ports::storage::{OperationRecord, StorageError, WriteTransaction};
use devworks_core::services::containers::{
    EFFECT_CONSUMER, EffectOutcome, STOP_CONSUMER, service::ContainerService,
};
use std::sync::Arc;

struct Effects {
    service: Arc<ContainerService>,
    authority: Arc<dyn AuthorityPort>,
}
fn outcome(value: EffectOutcome) -> EffectResult {
    match value {
        EffectOutcome::Completed(value) => EffectResult::Completed(value),
        EffectOutcome::Failed(reason) => EffectResult::Failed(reason),
        EffectOutcome::Unknown(reason) => EffectResult::Unknown(reason),
    }
}
impl EffectExecutor for Effects {
    fn execute(&self, operation: OperationRecord) -> EffectFuture {
        let service = self.service.clone();
        let authority = self.authority.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || {
                service.execute_effect(&operation, &*authority)
            })
            .await
            {
                Ok(result) => outcome(result),
                Err(_) => EffectResult::Unknown("container effect panicked".into()),
            }
        })
    }
    fn observe(&self, operation: OperationRecord) -> EffectFuture {
        let service = self.service.clone();
        let authority = self.authority.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || {
                service.observe_effect(&operation, &*authority)
            })
            .await
            {
                Ok(result) => outcome(result),
                Err(_) => EffectResult::Unknown("container observation panicked".into()),
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

pub fn consumer(service: Arc<ContainerService>, authority: Arc<dyn AuthorityPort>) -> Consumer {
    Consumer {
        name: EFFECT_CONSUMER.into(),
        handler: Arc::new(Settle),
        effect: Some(Arc::new(Effects { service, authority })),
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

/// Stop dispatch stays available while long builds, pulls or execs occupy every regular slot.
pub fn stop_consumer(
    service: Arc<ContainerService>,
    authority: Arc<dyn AuthorityPort>,
) -> Consumer {
    let mut consumer = consumer(service, authority);
    consumer.name = STOP_CONSUMER.into();
    consumer.max_in_flight = Some(2);
    consumer
}
