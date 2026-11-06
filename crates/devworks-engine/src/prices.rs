//! Official price maintenance uses the existing durable operation journal and queue. The
//! cadence supervisor records one overdue refresh, while bounded public GETs run off the writer.
use crate::dispatch::{
    Consumer, Delivery, DeliveryHandler, EffectExecutor, EffectFuture, EffectResult, Outcome,
    RetryPolicy,
};
use devworks_core::ports::storage::{
    ApplicationStore, OperationRecord, StorageError, WriteOptions, WriteTransaction, run_write,
};
use devworks_core::ports::usage::PriceSource;
use devworks_core::services::prices::{
    PriceService,
    refresh::{self, RefreshOutcome},
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
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
    fence: Arc<RwLock<bool>>,
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
        Arc::new(Self {
            store,
            service,
            source,
            stopping: Arc::new(AtomicBool::new(false)),
            fence: Arc::new(RwLock::new(false)),
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
        self.notify();
    }
    pub(crate) fn close(&self) {
        self.stop();
        *self.fence.write().unwrap_or_else(|e| e.into_inner()) = true;
    }
    pub async fn drain(&self) {
        self.stop();
        let fence = self.fence.clone();
        let _ = tokio::task::spawn_blocking(move || {
            *fence.write().unwrap_or_else(|e| e.into_inner()) = true
        })
        .await;
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
        tokio::task::spawn_blocking(move || {
            let closed = fence.read().unwrap_or_else(|e| e.into_inner());
            if *closed || stopping.load(Ordering::SeqCst) {
                return Ok(0);
            }
            let recovered = service
                .recover_refreshes(&*store, &*source, &|| stopping.load(Ordering::SeqCst))?;
            if stopping.load(Ordering::SeqCst) {
                return Ok(recovered);
            }
            run_write(&*store, WriteOptions::default(), move |tx| {
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
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let closed = worker.fence.read().unwrap_or_else(|e| e.into_inner());
                if *closed || worker.stopping.load(Ordering::SeqCst) {
                    return EffectResult::NotApplied("price maintenance is stopping".into());
                }
                match worker.service.execute_refresh(
                    &*worker.store,
                    &*worker.source,
                    &operation,
                    &|| worker.stopping.load(Ordering::SeqCst),
                ) {
                    RefreshOutcome::Completed(v) => EffectResult::Completed(v),
                    RefreshOutcome::Failed(e) => EffectResult::Failed(e),
                    RefreshOutcome::NotApplied(e) => EffectResult::NotApplied(e),
                    RefreshOutcome::Unknown(e) => EffectResult::Unknown(e),
                }
            })
            .await
            .unwrap_or_else(|_| EffectResult::Unknown("price maintenance task panicked".into()))
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
