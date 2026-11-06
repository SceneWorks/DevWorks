//! Supervised body indexing over the one durable operation and native inference journals.
use crate::dispatch::{
    Consumer, Delivery, DeliveryHandler, EffectExecutor, EffectFuture, EffectResult, Outcome,
    ReadyFuture, RetryPolicy,
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
enum JobLink {
    Unrelated,
    Pending,
    Ready,
    Refused,
}
fn job_link(
    read: &dyn ReadSnapshot,
    info: &StoreInfo,
    operation: &OperationRecord,
) -> Result<JobLink, StorageError> {
    let Some(value) = operation
        .payload
        .pointer("/sourceContext/invocationContext/jobRun")
        .filter(|v| !v.is_null())
    else {
        return Ok(JobLink::Unrelated);
    };
    let run_id: devworks_core::protocol::Id =
        serde_json::from_value(value.clone()).map_err(|_| StorageError::ScopeDenied)?;
    let Some(run) = read.jobs().job_run(&run_id)? else {
        return Ok(JobLink::Refused);
    };
    let definition: serde_json::Value =
        serde_json::from_str(&run.definition).map_err(|_| StorageError::ScopeDenied)?;
    if definition["kind"] != "Native" {
        return Ok(JobLink::Unrelated);
    }
    if definition["capabilityId"]
        != serde_json::json!(devworks_core::services::jobs::capability_id(
            "conventions.index"
        ))
        || operation
            .payload
            .pointer("/sourceContext/invocationContext/principal")
            != Some(&serde_json::json!(info.host_actor))
        || operation
            .payload
            .pointer("/sourceContext/invocationContext/kind")
            .and_then(serde_json::Value::as_str)
            != Some("Host")
        || run.boot_id != info.boot_id
        || run.status != "Running"
        || run.cancel_requested
        || run.hold.is_some()
    {
        return Ok(JobLink::Refused);
    }
    Ok(match run.inner_operation_id {
        Some(id) if id == operation.id => JobLink::Ready,
        Some(_) => JobLink::Refused,
        None => JobLink::Pending,
    })
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
            let owner_store = store.clone();
            let worker = tokio::task::spawn_blocking(move || {
                let info = owner_store.info();
                match read_snapshot(owner_store.as_ref(), &ReadOptions::default(), |read| {
                    job_link(read, &info, &operation)
                })? {
                    JobLink::Pending => Ok(IndexOutcome::Unknown(
                        "Awaiting the actual native job's accepted child link".into(),
                    )),
                    // Loss of current dispatch eligibility cannot establish the outcome
                    // of an already-sent child. Keep the existing operation and native
                    // custody unknown; observation must not enter the publisher/send path.
                    JobLink::Refused
                        if operation.state == devworks_core::protocol::OperationState::OutcomeUnknown =>
                    {
                        Ok(IndexOutcome::Unknown(
                            "The native job is no longer eligible; original effect custody remains unknown".into(),
                        ))
                    }
                    JobLink::Refused => Err(StorageError::ScopeDenied),
                    JobLink::Unrelated | JobLink::Ready => {
                        service.run_index(&operation, &worker_token, None)
                    }
                }
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
    fn ready(&self, operation: &OperationRecord) -> Option<ReadyFuture> {
        operation
            .payload
            .pointer("/sourceContext/invocationContext/jobRun")
            .filter(|v| !v.is_null())?;
        let (store, service, operation) =
            (self.store.clone(), self.service.clone(), operation.clone());
        Some(Box::pin(async move {
            while !service.is_stopped() {
                let (current, op) = (store.clone(), operation.clone());
                let pending = tokio::task::spawn_blocking(move || {
                    let info = current.info();
                    read_snapshot(current.as_ref(), &ReadOptions::default(), |read| {
                        job_link(read, &info, &op)
                    })
                })
                .await;
                if !matches!(pending, Ok(Ok(JobLink::Pending))) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }))
    }
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
