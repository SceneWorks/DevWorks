//! Git effects share the application's operation journal and owned process supervisor.
use crate::dispatch::{
    Consumer, Delivery, DeliveryHandler, EffectExecutor, EffectFuture, EffectResult, Outcome,
    RetryPolicy,
};
use devworks_core::ports::storage::{OperationRecord, StorageError, WriteTransaction};
use devworks_core::services::git::{EFFECT_CONSUMER, EffectOutcome, GitConfig, GitService};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct GitComposition {
    pub config: GitConfig,
}
struct Effects(Arc<GitService>);
fn result(outcome: EffectOutcome) -> EffectResult {
    match outcome {
        EffectOutcome::Completed(result) => EffectResult::Completed(result),
        EffectOutcome::Failed(reason) => EffectResult::Failed(reason),
        EffectOutcome::Unknown(reason) => EffectResult::Unknown(reason),
    }
}
impl EffectExecutor for Effects {
    fn execute(&self, operation: OperationRecord) -> EffectFuture {
        let service = self.0.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || service.execute(&operation)).await {
                Ok(outcome) => result(outcome),
                Err(_) => EffectResult::Unknown("Git effect panicked after dispatch".into()),
            }
        })
    }
    fn observe(&self, operation: OperationRecord) -> EffectFuture {
        let service = self.0.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || service.observe(&operation)).await {
                Ok(outcome) => result(outcome),
                Err(_) => EffectResult::Unknown("Git observation panicked".into()),
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
pub fn consumer(service: Arc<GitService>) -> Consumer {
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
/// Cancel-before-dispatch is already proved by the shared journal; release those local
/// reservations without repeating a Git command or deleting anything from the filesystem.
pub async fn run(service: Arc<GitService>) {
    while !service.is_stopped() {
        let current = service.clone();
        let _ = tokio::task::spawn_blocking(move || current.reconcile_cancelled()).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
