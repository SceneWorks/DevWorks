//! Official price maintenance uses the existing durable operation journal and queue. The
//! cadence supervisor records one overdue refresh, while bounded public GETs run off the writer.
use crate::dispatch::{
    Consumer, Delivery, DeliveryHandler, EffectExecutor, EffectFuture, EffectResult, Outcome,
    RetryPolicy,
};
use devworks_core::ports::storage::{
    ApplicationStore, OperationRecord, StorageError, WriteTransaction,
};
use devworks_core::ports::usage::PriceSource;
use devworks_core::services::prices::{
    PriceService,
    refresh::{self, RefreshOutcome, RefreshWriteFence},
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::{Notify, watch};
/// Public-source HTTP transport is injected by the native host; no provider credentials or
/// inference port are involved. Missing composition refuses refresh rather than faking success.
#[derive(Clone)]
pub struct PriceComposition {
    pub source: Arc<dyn PriceSource>,
    pub interval: Duration,
}
impl PriceComposition {
    pub fn new(source: Arc<dyn PriceSource>) -> Self {
        Self {
            source,
            interval: Duration::from_secs(60),
        }
    }
}
impl std::fmt::Debug for PriceComposition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PriceComposition")
            .field("source", &"injected official public source")
            .field("interval", &self.interval)
            .finish()
    }
}
#[derive(Clone, Debug)]
pub struct PricePass {
    pub result: Result<usize, StorageError>,
}
pub struct PriceSupervisor {
    store: Arc<dyn ApplicationStore>,
    service: Arc<PriceService>,
    source: Arc<dyn PriceSource>,
    stopping: Arc<AtomicBool>,
    fence: Arc<RefreshWriteFence>,
    stop_signal: watch::Sender<bool>,
    active: Arc<AtomicUsize>,
    finished: Arc<Notify>,
    ticking: tokio::sync::Mutex<()>,
    wake: Notify,
    status: watch::Sender<Option<PricePass>>,
}
impl PriceSupervisor {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        service: Arc<PriceService>,
        source: Arc<dyn PriceSource>,
    ) -> Arc<Self> {
        let (status, _) = watch::channel(None);
        let (stop_signal, _) = watch::channel(false);
        Arc::new(Self {
            store,
            service,
            source,
            stopping: Arc::new(AtomicBool::new(false)),
            fence: Arc::new(RefreshWriteFence::default()),
            stop_signal,
            active: Arc::new(AtomicUsize::new(0)),
            finished: Arc::new(Notify::new()),
            ticking: tokio::sync::Mutex::new(()),
            wake: Notify::new(),
            status,
        })
    }
    pub fn notify(&self) {
        self.wake.notify_one();
    }
    pub fn watch(&self) -> watch::Receiver<Option<PricePass>> {
        self.status.subscribe()
    }
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.fence.stop();
        self.stop_signal.send_replace(true);
        self.notify();
    }
    pub(crate) fn close(&self) {
        self.stop();
        self.fence.close();
    }
    async fn close_writes(&self) {
        self.stop();
        let fence = self.fence.clone();
        let _ = tokio::task::spawn_blocking(move || fence.close()).await;
    }
    async fn idle(&self) {
        loop {
            let finished = self.finished.notified();
            if self.active.load(Ordering::SeqCst) == 0 {
                return;
            }
            finished.await;
        }
    }
    /// Graceful callers may wait for every source callback; application shutdown uses the
    /// shared deadline instead. A deadline never certifies that a callback physically ended.
    pub async fn drain(&self) {
        self.close_writes().await;
        self.idle().await;
    }
    pub async fn drain_until(&self, deadline: tokio::time::Instant) {
        self.close_writes().await;
        let _ = crate::dispatch::first(tokio::time::sleep_until(deadline), self.idle()).await;
    }
    pub async fn tick(&self) -> Result<usize, StorageError> {
        let _one = self.ticking.lock().await;
        if self.stopping.load(Ordering::SeqCst) {
            return Ok(0);
        }
        let (store, service, stopping, fence, source) = (
            self.store.clone(),
            self.service.clone(),
            self.stopping.clone(),
            self.fence.clone(),
            self.source.clone(),
        );
        let flight = Flight::new(self.active.clone(), self.finished.clone());
        source_work(flight, move || {
            if stopping.load(Ordering::SeqCst) {
                return Ok(0);
            }
            let recovered = service.recover_refreshes(
                &*store,
                &*source,
                &|| stopping.load(Ordering::SeqCst),
                &fence,
            )?;
            if stopping.load(Ordering::SeqCst) {
                return Ok(recovered);
            }
            fence.write(&*store, move |tx| {
                service.schedule_due(tx).map(|n| n + recovered)
            })
        })
        .await
        .map_err(|_| StorageError::InvalidRecord("price schedule task panicked".into()))?
    }
    pub async fn run(self: Arc<Self>, every: Duration, wakeup: crate::dispatch::Wakeup) {
        while !self.stopping.load(Ordering::SeqCst) {
            let result = self.tick().await;
            if result.as_ref().is_ok_and(|n| *n > 0) {
                wakeup.wake();
            }
            self.status.send_replace(Some(PricePass { result }));
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
            let _ = crate::dispatch::first(
                self.wake.notified(),
                tokio::time::sleep(every.max(Duration::from_millis(1))),
            )
            .await;
        }
    }
    pub fn consumer(self: &Arc<Self>) -> Consumer {
        Consumer {
            name: refresh::CONSUMER.into(),
            handler: Arc::new(Settle),
            effect: Some(Arc::new(Effects(self.clone()))),
            async_handler: None,
            eligibility: None,
            stop_grace_ms: 30_000,
            retry: RetryPolicy {
                base_delay_ms: 1000,
                max_delay_ms: 60_000,
                max_attempts: 4,
            },
            max_in_flight: Some(1),
        }
    }
}
/// A bounded public GET may outlive application shutdown. It must not retain a Tokio blocking
/// task (whose runtime destructor waits for it). Late completion owns no write authority.
/// `flight` ends only after `work` and everything it captured (the store among it) are dropped,
/// so a drained supervisor no longer holds the data root.
async fn source_work<T, F>(flight: Flight, work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (finished, result) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("devworks-price-source".into())
        .spawn(move || {
            let value = work();
            drop(flight);
            let _ = finished.send(value);
        })
        .map_err(|error| error.to_string())?;
    result
        .await
        .map_err(|_| "price source worker ended without a result".into())
}

struct Flight {
    active: Arc<AtomicUsize>,
    finished: Arc<Notify>,
}
impl Flight {
    fn new(active: Arc<AtomicUsize>, finished: Arc<Notify>) -> Self {
        active.fetch_add(1, Ordering::SeqCst);
        Self { active, finished }
    }
}
impl Drop for Flight {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        self.finished.notify_one();
    }
}

struct Settle;
impl DeliveryHandler for Settle {
    fn apply(&self, _: &mut dyn WriteTransaction, _: &Delivery) -> Result<Outcome, StorageError> {
        Ok(Outcome::Completed)
    }
}
struct Effects(Arc<PriceSupervisor>);
impl Effects {
    fn effect(&self, operation: OperationRecord) -> EffectFuture {
        let worker = self.0.clone();
        let mut stopped = worker.stop_signal.subscribe();
        Box::pin(async move {
            if *stopped.borrow() {
                return EffectResult::Unknown(
                    "price maintenance stopped; refresh needs reconciliation".into(),
                );
            }
            let flight = Flight::new(worker.active.clone(), worker.finished.clone());
            let fetching = source_work(flight, move || {
                if worker.stopping.load(Ordering::SeqCst) {
                    return EffectResult::NotApplied("price maintenance is stopping".into());
                }
                match worker.service.execute_refresh(
                    &*worker.store,
                    &*worker.source,
                    &operation,
                    &|| worker.stopping.load(Ordering::SeqCst),
                    &worker.fence,
                ) {
                    RefreshOutcome::Completed(v) => EffectResult::Completed(v),
                    RefreshOutcome::Failed(e) => EffectResult::Failed(e),
                    RefreshOutcome::NotApplied(e) => EffectResult::NotApplied(e),
                    RefreshOutcome::Unknown(e) => EffectResult::Unknown(e),
                }
            });
            match crate::dispatch::first(stopped.changed(), fetching).await {
                crate::dispatch::Either::Left(_) => EffectResult::Unknown(
                    "price maintenance stopped; refresh needs reconciliation".into(),
                ),
                crate::dispatch::Either::Right(result) => result.unwrap_or_else(|_| {
                    EffectResult::Unknown("price maintenance task panicked".into())
                }),
            }
        })
    }
}
impl EffectExecutor for Effects {
    fn execute(&self, operation: OperationRecord) -> EffectFuture {
        self.effect(operation)
    }
    fn observe(&self, operation: OperationRecord) -> EffectFuture {
        self.effect(operation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// Dropped slowly, like a store that joins its writer before releasing the data root.
    struct SlowDrop(Arc<AtomicBool>);
    impl Drop for SlowDrop {
        fn drop(&mut self) {
            std::thread::sleep(std::time::Duration::from_millis(100));
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn a_source_callback_reports_idle_only_after_dropping_what_it_captured() {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let active = Arc::new(AtomicUsize::new(0));
                let finished = Arc::new(Notify::new());
                let dropped = Arc::new(AtomicBool::new(false));
                let held = SlowDrop(dropped.clone());
                let flight = Flight::new(active.clone(), finished.clone());
                let work = tokio::spawn(source_work(flight, move || {
                    let _ = &held;
                }));
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    loop {
                        let notified = finished.notified();
                        if active.load(Ordering::SeqCst) == 0 {
                            return;
                        }
                        notified.await;
                    }
                })
                .await
                .expect("the callback ends");
                assert!(
                    dropped.load(Ordering::SeqCst),
                    "idle was reported while the callback still held its captures"
                );
                work.await.unwrap().unwrap();
            });
    }
}
