//! One durable qualification consumer; only the actual native executor can supply evidence.
use super::embedding::NativeEmbeddings;
use crate::dispatch::{
    Consumer, Delivery, DeliveryHandler, EffectExecutor, EffectFuture, EffectResult, Outcome,
    RetryPolicy,
};
use devworks_core::{
    ports::storage::{OperationRecord, StorageError, WriteTransaction},
    services::{
        conventions::indexing::embedding::EmbeddingFailure,
        router::embedding_spaces::{
            EFFECT_CONSUMER, EmbeddingSpaceService, ProbeOutcome, ProbePreparation,
        },
    },
};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};

pub struct ProbeRuntime {
    service: Arc<EmbeddingSpaceService>,
    executor: OnceLock<NativeEmbeddings>,
    stopped: AtomicBool,
}
impl ProbeRuntime {
    pub fn new(service: Arc<EmbeddingSpaceService>) -> Arc<Self> {
        Arc::new(Self {
            service,
            executor: OnceLock::new(),
            stopped: AtomicBool::new(false),
        })
    }
    /// Called once after the real provider/schema composition exists, before delivery starts.
    pub fn install(&self, executor: NativeEmbeddings) -> bool {
        if self.stopped.load(Ordering::Acquire) {
            return false;
        }
        self.executor.set(executor).is_ok()
    }
    pub fn stop(&self) {
        self.service.disable_native_qualification();
        self.stopped.store(true, Ordering::Release);
        if let Some(executor) = self.executor.get() {
            executor.stop();
        }
    }
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
    pub fn reconcile(&self) -> Result<(), StorageError> {
        let Some(executor) = self.executor.get() else {
            return Ok(());
        };
        let mut preparation_after = None;
        loop {
            if self.is_stopped() {
                return Err(StorageError::Cancelled);
            }
            preparation_after = executor.reconcile_preparations(preparation_after.as_ref())?;
            if preparation_after.is_none() {
                break;
            }
        }
        let mut after = None;
        loop {
            if self.is_stopped() {
                return Err(StorageError::Cancelled);
            }
            let page = executor.reconcile_probes(after.as_ref())?;
            after = page.next;
            if after.is_none() {
                return Ok(());
            }
        }
    }
    fn perform(&self, operation: OperationRecord, observe: bool) -> EffectResult {
        if self.is_stopped() {
            return EffectResult::Unknown("Embedding runtime is stopping".into());
        }
        let Some(executor) = self.executor.get() else {
            return EffectResult::Unknown("Native embedding runtime is unavailable".into());
        };
        match self.service.prepare_probe(&operation) {
            Ok(ProbePreparation::Ready(prepared)) => {
                match executor.execute_probe(*prepared, observe) {
                    Ok(()) => EffectResult::Completed(
                        serde_json::json!({"kind":"VerifiedNativeEmbeddingProbe"}),
                    ),
                    Err(EmbeddingFailure::NotStarted { .. }) => EffectResult::NotApplied(
                        "Original native request has not been admitted".into(),
                    ),
                    Err(EmbeddingFailure::Unavailable(reason)) => EffectResult::Failed(reason),
                    Err(EmbeddingFailure::Unknown { reason, .. }) => EffectResult::Unknown(reason),
                    Err(EmbeddingFailure::Cancelled) => {
                        EffectResult::Failed("Embedding probe cancelled".into())
                    }
                    Err(EmbeddingFailure::Storage(error)) => EffectResult::Unknown(format!(
                        "Native probe evidence unavailable: {error:?}"
                    )),
                }
            }
            Ok(ProbePreparation::Terminal(ProbeOutcome::Completed)) => EffectResult::Completed(
                serde_json::json!({"kind":"PreviouslyVerifiedNativeEmbeddingProbe"}),
            ),
            Ok(ProbePreparation::Terminal(
                ProbeOutcome::Failed(reason) | ProbeOutcome::Cancelled(reason),
            )) => EffectResult::Failed(reason),
            Ok(
                ProbePreparation::Terminal(ProbeOutcome::Unknown(reason))
                | ProbePreparation::Waiting(reason),
            ) => EffectResult::Unknown(reason),
            Err(error) => {
                EffectResult::Unknown(format!("Original embedding probe unavailable: {error:?}"))
            }
        }
    }
    pub fn consumer(self: &Arc<Self>) -> Consumer {
        Consumer {
            name: EFFECT_CONSUMER.into(),
            handler: self.clone(),
            effect: Some(Arc::new(Effects(self.clone()))),
            async_handler: None,
            eligibility: None,
            stop_grace_ms: 5_000,
            retry: RetryPolicy::default(),
            max_in_flight: Some(1),
        }
    }
}
struct Effects(Arc<ProbeRuntime>);
impl EffectExecutor for Effects {
    fn execute(&self, operation: OperationRecord) -> EffectFuture {
        self.run(operation, false)
    }
    fn observe(&self, operation: OperationRecord) -> EffectFuture {
        self.run(operation, true)
    }
}
impl Effects {
    fn run(&self, operation: OperationRecord, observe: bool) -> EffectFuture {
        let owner = self.0.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || owner.perform(operation, observe))
                .await
                .unwrap_or_else(|_| {
                    EffectResult::Unknown("Native embedding observer stopped unexpectedly".into())
                })
        })
    }
}
impl DeliveryHandler for ProbeRuntime {
    fn apply(
        &self,
        tx: &mut dyn WriteTransaction,
        delivery: &Delivery,
    ) -> Result<Outcome, StorageError> {
        if self.is_stopped() {
            return Err(StorageError::Cancelled);
        }
        let executor = self.executor.get().ok_or(StorageError::ScopeDenied)?;
        let operation = delivery
            .entry
            .operation_id
            .as_ref()
            .ok_or(StorageError::ScopeDenied)?;
        executor.complete_probe(tx, operation)?;
        Ok(Outcome::Completed)
    }
}
