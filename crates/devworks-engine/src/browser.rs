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
use crate::dispatch::Consumer;
use crate::effect_consumer::{PanicReasons, STANDARD_RETRY, effect_consumer};
use crate::workers::Ticker;
use devworks_core::authority::destructive::FilesystemResolver;
use devworks_core::ports::browser::BrowserPlatform;
use devworks_core::ports::web::Resolver;
use devworks_core::services::browser::{BrowserConfig, BrowserService, EFFECT_CONSUMER};
#[cfg(doc)]
use devworks_core::services::effect::JournaledEffect;
use std::sync::Arc;
use std::time::Duration;

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

/// The `browser_effects` consumer over `service`. An action ordered behind another waits
/// (bounded by the order bound) until that one settled, parked without a worker permit; the
/// service states that through [`JournaledEffect::wait_bound`].
pub fn consumer(service: Arc<BrowserService>) -> Consumer {
    effect_consumer(
        EFFECT_CONSUMER,
        service,
        PanicReasons {
            // A panic after dispatch: the effect may have happened.
            execute: "the browser effect panicked",
            observe: "the browser observation panicked",
        },
        STANDARD_RETRY,
        None,
    )
}

/// Stops the supervision loop.
#[derive(Default)]
pub struct Supervision {
    ticker: Ticker,
}
impl Supervision {
    pub fn stop(&self) {
        self.ticker.stop();
    }
}

/// Supervision ticks every `every` until stopped; browser assets are re-read every 30 ticks.
pub async fn run(service: Arc<BrowserService>, control: Arc<Supervision>, every: Duration) {
    let mut n: u64 = 0;
    control
        .ticker
        .run(every, || {
            n += 1;
            let svc = service.clone();
            let refresh = n.is_multiple_of(30);
            async move {
                let _ = tokio::task::spawn_blocking(move || {
                    if refresh {
                        svc.refresh_assets();
                    }
                    svc.tick();
                })
                .await;
            }
        })
        .await;
}
