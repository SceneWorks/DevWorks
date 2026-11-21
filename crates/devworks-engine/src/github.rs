//! GitHub effect delivery uses the shared outbox and its recovery/settlement rules.
use crate::dispatch::Consumer;
use crate::effect_consumer::{PanicReasons, STANDARD_RETRY, effect_consumer};
use devworks_core::services::github::{EFFECT_CONSUMER, GitHubService};
use std::sync::Arc;

pub fn consumer(service: Arc<GitHubService>) -> Consumer {
    effect_consumer(
        EFFECT_CONSUMER,
        service,
        PanicReasons {
            execute: "GitHub effect interrupted after dispatch",
            observe: "GitHub recovery observation interrupted",
        },
        STANDARD_RETRY,
        Some(1),
    )
}

struct Notice;

impl crate::dispatch::DeliveryHandler for Notice {
    fn apply(
        &self,
        tx: &mut dyn devworks_core::ports::storage::WriteTransaction,
        delivery: &crate::dispatch::Delivery,
    ) -> Result<crate::dispatch::Outcome, devworks_core::ports::storage::StorageError> {
        use crate::dispatch::Outcome;
        match devworks_core::services::github::notices::deliver(tx, &delivery.event) {
            Ok(()) => Ok(Outcome::Completed),
            Err(devworks_core::ports::storage::StorageError::ScopeDenied) => {
                Ok(Outcome::Hold("GitHub source authorization changed".into()))
            }
            Err(error) => Err(error),
        }
    }
}

pub(crate) fn notice_consumer() -> Consumer {
    Consumer {
        name: devworks_core::services::github::references::NOTICE_CONSUMER.into(),
        handler: Arc::new(Notice),
        effect: None,
        async_handler: None,
        eligibility: None,
        stop_grace_ms: 5000,
        retry: STANDARD_RETRY,
        max_in_flight: Some(1),
    }
}

pub(crate) async fn run(
    service: Arc<GitHubService>,
    clock: Arc<dyn devworks_core::ports::Clock>,
    stopped: crate::workers::Stopped,
    commits: crate::workers::Commits,
) {
    use crate::workers::Next;
    use devworks_core::locks::LockExt;
    use std::sync::Mutex;
    use std::time::Duration;

    let live = service.clone();
    let cursor = Arc::new(Mutex::new((None, None)));
    commits
        .run(
            stopped,
            move || !live.is_stopped(),
            move || {
                let (service, cursor, clock) = (service.clone(), cursor.clone(), clock.clone());
                async move {
                    let (after, throttled) = cursor.locked().clone();
                    let now = clock.now_ms().get();
                    if let Some(until) = throttled
                        && until > now
                    {
                        return Next::Within(Duration::from_millis((until - now) as u64));
                    }
                    match tokio::task::spawn_blocking(move || service.poll_pass(after)).await {
                        Ok(Ok(pass)) => {
                            let more = pass.after.is_some();
                            *cursor.locked() = (pass.after, pass.defer_until_ms);
                            match pass.defer_until_ms {
                                Some(until) => Next::Within(Duration::from_millis(
                                    until.saturating_sub(clock.now_ms().get()).max(0) as u64,
                                )),
                                None => more.into(),
                            }
                        }
                        _ => Next::Idle,
                    }
                }
            },
        )
        .await;
}

struct IndexRefresh(Arc<devworks_core::services::indexing::IndexEnv>);

impl crate::dispatch::DeliveryHandler for IndexRefresh {
    fn apply(
        &self,
        tx: &mut dyn devworks_core::ports::storage::WriteTransaction,
        delivery: &crate::dispatch::Delivery,
    ) -> Result<crate::dispatch::Outcome, devworks_core::ports::storage::StorageError> {
        use crate::dispatch::Outcome;
        use devworks_core::ports::storage::StorageError;
        use devworks_core::services::github::indexing::{IndexDelivery, deliver};
        match deliver(tx, &self.0, &delivery.event) {
            Ok(IndexDelivery::Settled) => Ok(Outcome::Completed),
            Ok(IndexDelivery::Busy) => Ok(Outcome::Retry(
                "Repository indexing is already running".into(),
            )),
            Err(StorageError::ScopeDenied) => {
                Ok(Outcome::Hold("GitHub source authorization changed".into()))
            }
            Err(error) => Err(error),
        }
    }
}

pub(crate) fn index_consumer(index: Arc<devworks_core::services::indexing::IndexEnv>) -> Consumer {
    Consumer {
        name: devworks_core::services::github::references::INDEX_CONSUMER.into(),
        handler: Arc::new(IndexRefresh(index)),
        effect: None,
        async_handler: None,
        eligibility: None,
        stop_grace_ms: 5000,
        retry: crate::dispatch::RetryPolicy {
            max_attempts: 64,
            ..STANDARD_RETRY
        },
        max_in_flight: Some(1),
    }
}
