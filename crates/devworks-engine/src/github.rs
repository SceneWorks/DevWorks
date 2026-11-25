//! GitHub effect delivery uses the shared outbox and its recovery/settlement rules.
use crate::dispatch::{
    AsyncDelivery, AsyncDeliveryHandler, AsyncFuture, AsyncOutcome, Consumer, Delivery,
    DeliveryHandler, Outcome,
};
use crate::effect_consumer::{PanicReasons, STANDARD_RETRY, effect_consumer};
use devworks_core::ports::storage::{ApplicationStore, StorageError, WriteTransaction};
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::github::{EFFECT_CONSUMER, GitHubService, settle_effect};
use devworks_core::services::work::WorkService;
use std::sync::Arc;

struct SettleGithubEffect;

impl DeliveryHandler for SettleGithubEffect {
    fn apply(
        &self,
        tx: &mut dyn WriteTransaction,
        delivery: &Delivery,
    ) -> Result<Outcome, StorageError> {
        if let Some(op_id) = &delivery.entry.operation_id
            && let Some(op) = tx.operations().operation(op_id)?
        {
            settle_effect(tx, &op)?;
        }
        Ok(Outcome::Completed)
    }
}

pub fn consumer(service: Arc<GitHubService>) -> Consumer {
    let mut consumer = effect_consumer(
        EFFECT_CONSUMER,
        service,
        PanicReasons {
            execute: "GitHub effect interrupted after dispatch",
            observe: "GitHub recovery observation interrupted",
        },
        STANDARD_RETRY,
        Some(1),
    );
    consumer.handler = Arc::new(SettleGithubEffect);
    consumer
}

struct Notice;

impl DeliveryHandler for Notice {
    fn apply(
        &self,
        tx: &mut dyn WriteTransaction,
        delivery: &Delivery,
    ) -> Result<Outcome, StorageError> {
        match devworks_core::services::github::notices::deliver(tx, &delivery.event) {
            Ok(()) => Ok(Outcome::Completed),
            Err(StorageError::ScopeDenied) => {
                Ok(Outcome::Hold("GitHub source authorization changed".into()))
            }
            Err(error) => Err(error),
        }
    }
}

struct NoticeRunner {
    store: Arc<dyn ApplicationStore>,
    artifacts: Arc<ArtifactService>,
    work: Arc<WorkService>,
}

impl AsyncDeliveryHandler for NoticeRunner {
    fn start(&self, delivery: AsyncDelivery) -> AsyncFuture {
        let store = self.store.clone();
        let artifacts = self.artifacts.clone();
        let work = self.work.clone();
        let event = delivery.delivery.event;
        Box::pin(async move {
            let outcome = tokio::task::spawn_blocking(move || {
                devworks_core::services::github::notices::reconcile_and_deliver(
                    &*store, &artifacts, &work, &event,
                )
            })
            .await;
            match outcome {
                Ok(Ok(())) => AsyncOutcome::Committed,
                Ok(Err(StorageError::ScopeDenied)) => {
                    AsyncOutcome::Hold("GitHub source authorization changed".into())
                }
                Ok(Err(StorageError::InvalidRecord(reason))) => AsyncOutcome::Poison(reason),
                Ok(Err(error)) => AsyncOutcome::Retry(error.to_string()),
                Err(_) => AsyncOutcome::Retry("GitHub notice task panicked".into()),
            }
        })
    }
}

pub fn notice_consumer() -> Consumer {
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

pub fn notice_consumer_with_services(
    store: Arc<dyn ApplicationStore>,
    artifacts: Arc<ArtifactService>,
    work: Arc<WorkService>,
) -> Consumer {
    Consumer {
        name: devworks_core::services::github::references::NOTICE_CONSUMER.into(),
        handler: Arc::new(Notice),
        effect: None,
        async_handler: Some(Arc::new(NoticeRunner {
            store,
            artifacts,
            work,
        })),
        eligibility: None,
        stop_grace_ms: 5000,
        retry: STANDARD_RETRY,
        max_in_flight: Some(1),
    }
}

pub(crate) async fn run(
    service: Arc<GitHubService>,
    reviews: Option<Arc<devworks_core::services::reviews::ReviewService>>,
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
    let review_cursor = Arc::new(Mutex::new(None));
    commits
        .run(
            stopped,
            move || !live.is_stopped(),
            move || {
                let (service, cursor, clock) = (service.clone(), cursor.clone(), clock.clone());
                let (reviews, review_cursor) = (reviews.clone(), review_cursor.clone());
                async move {
                    let (after, throttled) = cursor.locked().clone();
                    let now = clock.now_ms().get();
                    if let Some(until) = throttled
                        && until > now
                    {
                        return Next::Within(Duration::from_millis((until - now) as u64));
                    }
                    let after_review = review_cursor.locked().clone();
                    match tokio::task::spawn_blocking(move || {
                        let pass = service.poll_pass(after)?;
                        let next = reviews.and_then(|reviews| {
                            reviews
                                .reconcile_late_page(after_review.as_ref())
                                .ok()
                                .flatten()
                        });
                        Ok::<_, StorageError>((pass, next))
                    })
                    .await
                    {
                        Ok(Ok((pass, next_review))) => {
                            let more = pass.after.is_some() || next_review.is_some();
                            *review_cursor.locked() = next_review;
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

impl DeliveryHandler for IndexRefresh {
    fn apply(
        &self,
        tx: &mut dyn WriteTransaction,
        delivery: &Delivery,
    ) -> Result<Outcome, StorageError> {
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
