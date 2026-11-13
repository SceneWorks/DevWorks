//! Owned browser sessions in the application lifecycle (DW-047): the `browser_effects` durable
//! consumer that executes journaled browser effects, the supervision loop and the composition
//! the runtime builds the one [`BrowserService`] from.
//!
//! * **Effects.** Create, Close, navigation, input and upload commit an operation and a
//!   delivery; an action names the session's previous one and is parked (claimed, without a
//!   worker permit) until that one settled. The dispatcher commits `Dispatching` before the
//!   effect runs, so an interruption leaves `OutcomeUnknown`, which is held for reconciliation
//!   and never re-executed; the effect itself runs on a blocking thread through
//!   [`BrowserService::execute`].
//! * **Supervision.** [`run`] pumps idle sessions' notifications, records popups and closed
//!   targets, and settles sessions whose browser exited or whose connection dropped, every
//!   [`crate::runtime::RuntimeConfig::browser_interval`]; it re-reads the browser assets of the
//!   target periodically.
use crate::dispatch::{
    Consumer, Delivery, DeliveryHandler, EffectExecutor, EffectFuture, EffectResult, Outcome,
    ReadyFuture, RetryPolicy,
};
use devworks_core::authority::destructive::FilesystemResolver;
use devworks_core::ports::browser::BrowserPlatform;
use devworks_core::ports::storage::{OperationRecord, StorageError, WriteTransaction};
use devworks_core::ports::web::Resolver;
use devworks_core::services::browser::{
    BrowserConfig, BrowserService, EFFECT_CONSUMER, EffectOutcome,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

/// What the runtime composes the browser service from: the native platform (discovery,
/// profiles, transport), the filesystem resolver of the launch preflight, the data root the
/// preflight protects and the name resolver of the egress policy.
#[derive(Clone)]
pub struct BrowserComposition {
    pub platform: Arc<dyn BrowserPlatform>,
    pub fs: Arc<dyn FilesystemResolver>,
    pub app_data_root: Option<String>,
    pub config: BrowserConfig,
    /// Resolves the names pages request so every address can be checked before it is sent.
    pub resolver: Arc<dyn Resolver>,
}

struct Effects(Arc<BrowserService>);

fn result(o: EffectOutcome) -> EffectResult {
    match o {
        EffectOutcome::Completed(v) => EffectResult::Completed(v),
        EffectOutcome::Failed(c) => EffectResult::Failed(c),
        EffectOutcome::NotApplied(c) => EffectResult::NotApplied(c),
        EffectOutcome::Unknown(c) => EffectResult::Unknown(c),
    }
}

impl EffectExecutor for Effects {
    fn execute(&self, operation: OperationRecord) -> EffectFuture {
        let svc = self.0.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || svc.execute(&operation)).await {
                Ok(o) => result(o),
                // A panic after dispatch: the effect may have happened.
                Err(_) => EffectResult::Unknown("the browser effect panicked".into()),
            }
        })
    }
    fn observe(&self, operation: OperationRecord) -> EffectFuture {
        let svc = self.0.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || svc.observe(&operation)).await {
                Ok(o) => result(o),
                Err(_) => EffectResult::Unknown("the browser observation panicked".into()),
            }
        })
    }
    /// An action ordered behind another waits (bounded by the order bound) until that one
    /// settled, parked without a worker permit; Create and Close are never ordered.
    fn ready(&self, operation: &OperationRecord) -> Option<ReadyFuture> {
        operation.payload.get("after")?;
        let (svc, op) = (self.0.clone(), operation.clone());
        Some(Box::pin(async move {
            let deadline = std::time::Instant::now() + svc.order_bound();
            loop {
                let (s, o) = (svc.clone(), op.clone());
                let pending = tokio::task::spawn_blocking(move || s.predecessor_pending(&o))
                    .await
                    .unwrap_or(false);
                if !pending || std::time::Instant::now() >= deadline {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }))
    }
}

/// The delivery itself has nothing to apply: the journaled effect is the work.
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

/// The `browser_effects` consumer over `service`.
pub fn consumer(service: Arc<BrowserService>) -> Consumer {
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

/// Stops the supervision loop.
#[derive(Default)]
pub struct Supervision {
    stopping: AtomicBool,
    wake: Notify,
}
impl Supervision {
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }
}

/// Supervision ticks every `every` until stopped; browser assets are re-read every 30 ticks.
pub async fn run(service: Arc<BrowserService>, control: Arc<Supervision>, every: Duration) {
    let mut n: u64 = 0;
    while !control.stopping.load(Ordering::SeqCst) {
        n += 1;
        let svc = service.clone();
        let refresh = n.is_multiple_of(30);
        let _ = tokio::task::spawn_blocking(move || {
            if refresh {
                svc.refresh_assets();
            }
            svc.tick();
        })
        .await;
        let _ = crate::dispatch::first(control.wake.notified(), tokio::time::sleep(every)).await;
    }
}
