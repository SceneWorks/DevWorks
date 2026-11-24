//! One supervised metadata-only consumer. The shared core owns admission, current source
//! authority, immutable staging and publication; this layer owns execution and cancellation.
use crate::dispatch::{Consumer, Delivery, DeliveryHandler, Outcome, RetryPolicy};
use crate::effect_consumer::{PanicReasons, effect_consumer};
use devworks_core::{
    ports::{database::MetadataFailure, storage::*},
    services::effect::{EffectOutcome, JournaledEffect},
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
impl Effects {
    /// Runs the metadata publication on this blocking thread. Every 100 ms it checks the
    /// delivery's cancel request and the owner's stop, and cancels `token` (which the consumer
    /// also cancels when the dispatcher abandons the attempt). Execute and observe are the same
    /// operation: the publication is idempotent under its original journal.
    fn run(&self, operation: &OperationRecord, token: &Cancellation) -> EffectOutcome {
        let delivery = serde_json::from_value::<devworks_core::protocol::Id>(
            operation.payload["deliveryId"].clone(),
        )
        .ok();
        tokio::runtime::Handle::current().block_on(async {
            let work = self.service.run_metadata(operation, token);
            tokio::pin!(work);
            let result = loop {
                tokio::select! {
                    result = &mut work => break result,
                    () = tokio::time::sleep(Duration::from_millis(100)) => {
                        let id = delivery.clone();
                        let cancelled = read_snapshot(
                            self.store.as_ref(),
                            &ReadOptions::default(),
                            |r| match id {
                                Some(id) => Ok(r.queue().entry(&id)?.is_none_or(|q| q.cancel_requested)),
                                None => Ok(true),
                            },
                        );
                        if self.service.is_stopped() || !matches!(cancelled, Ok(false)) {
                            token.cancel();
                        }
                    }
                }
            };
            self.settle(operation, result)
        })
    }
    fn settle(
        &self,
        operation: &OperationRecord,
        result: Result<SchemaOutcome, StorageError>,
    ) -> EffectOutcome {
        match result {
            Ok(SchemaOutcome::Completed(value)) => EffectOutcome::Completed(value),
            Ok(SchemaOutcome::Failed(reason)) => EffectOutcome::Failed(reason),
            Err(
                StorageError::CommitUnknown
                | StorageError::Busy
                | StorageError::StorageUnavailable { .. },
            ) => EffectOutcome::Unknown(
                "Metadata publication interrupted; observe the original operation".into(),
            ),
            Err(_) if self.service.is_stopped() => EffectOutcome::Unknown(
                "Metadata owner stopped; original operation remains recoverable".into(),
            ),
            Err(error) => {
                let failure = if matches!(error, StorageError::Cancelled) {
                    MetadataFailure::Cancelled
                } else {
                    MetadataFailure::AccessDenied
                };
                match self.service.fail_metadata(operation, failure) {
                    Ok(()) => EffectOutcome::Failed(format!("Metadata{failure:?}")),
                    Err(_) => EffectOutcome::Unknown(
                        "Metadata status acknowledgement interrupted; observe the original operation".into(),
                    ),
                }
            }
        }
    }
}
impl JournaledEffect for Effects {
    fn execute(&self, operation: &OperationRecord) -> EffectOutcome {
        self.run(operation, &Cancellation::new())
    }
    fn observe(&self, operation: &OperationRecord) -> EffectOutcome {
        self.run(operation, &Cancellation::new())
    }
    fn execute_cancellable(
        &self,
        operation: &OperationRecord,
        token: &Cancellation,
    ) -> EffectOutcome {
        self.run(operation, token)
    }
    fn observe_cancellable(
        &self,
        operation: &OperationRecord,
        token: &Cancellation,
    ) -> EffectOutcome {
        self.run(operation, token)
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
    let mut consumer = effect_consumer(
        EFFECT_CONSUMER,
        Arc::new(Effects {
            store,
            service: service.clone(),
        }),
        PanicReasons {
            execute: "Metadata worker stopped before acknowledgement",
            observe: "Metadata worker stopped before acknowledgement",
        },
        RetryPolicy::default(),
        Some(1),
    );
    consumer.handler = Arc::new(Settle(service));
    consumer
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

/// The host supplies data access separately from the schema catalog reader, whose
/// fixed read-only behavior is preserved.
#[derive(Clone)]
pub struct DatabaseComposition {
    pub adapter: Arc<dyn devworks_core::ports::database::data::DataAdapter>,
    pub filesystem: Arc<dyn devworks_core::authority::destructive::FilesystemResolver>,
}

impl std::fmt::Debug for DatabaseComposition {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DatabaseComposition")
            .finish_non_exhaustive()
    }
}

pub(crate) struct DatabaseReadRunner(pub(crate) tokio::runtime::Handle);

impl devworks_core::services::database::DatabaseReadRunner for DatabaseReadRunner {
    fn wait(
        &self,
        action: devworks_core::ports::database::data::DataFuture,
    ) -> Result<
        devworks_core::ports::database::data::DataOutcome,
        devworks_core::ports::database::data::DataFailure,
    > {
        use devworks_core::ports::database::data::{DataFailure, FailureKind};
        use tokio::runtime::{Handle, RuntimeFlavor};
        match Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| self.0.block_on(action))
            }
            Err(_) => self.0.block_on(action),
            _ => Err(DataFailure::Refused(FailureKind::UnsupportedProfile)),
        }
    }
    fn wait_for_effect(
        &self,
        action: devworks_core::ports::database::data::DataFuture,
    ) -> Result<
        devworks_core::ports::database::data::DataOutcome,
        devworks_core::ports::database::data::DataFailure,
    > {
        self.0.block_on(action)
    }
}

pub(crate) fn database_consumer(
    service: Arc<devworks_core::services::database::DataExecutor>,
) -> Consumer {
    effect_consumer(
        devworks_core::services::database::EFFECT_CONSUMER,
        service,
        PanicReasons {
            execute: "Database dispatch stopped without a confirmed outcome",
            observe: "Database observation did not finish",
        },
        RetryPolicy {
            base_delay_ms: 500,
            max_delay_ms: 3_000,
            max_attempts: 1,
        },
        Some(3),
    )
}
