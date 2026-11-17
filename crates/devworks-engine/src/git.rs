//! Git effects share the application's operation journal and owned process supervisor.
use crate::dispatch::Consumer;
use crate::effect_consumer::{PanicReasons, STANDARD_RETRY, effect_consumer};
use devworks_core::services::git::{EFFECT_CONSUMER, GitConfig, GitService};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct GitComposition {
    pub config: GitConfig,
}
pub fn consumer(service: Arc<GitService>) -> Consumer {
    effect_consumer(
        EFFECT_CONSUMER,
        service,
        PanicReasons {
            execute: "Git effect panicked after dispatch",
            observe: "Git observation panicked",
        },
        STANDARD_RETRY,
        None,
    )
}
/// Cancel-before-dispatch is already proved by the shared journal; release those local
/// reservations without repeating a Git command or deleting anything from the filesystem.
pub(crate) async fn run(service: Arc<GitService>, mut stopped: crate::workers::Stopped) {
    while !service.is_stopped() && !stopped.is_set() {
        let current = service.clone();
        let _ = tokio::task::spawn_blocking(move || current.reconcile_cancelled()).await;
        // Waits out the interval, or ends at once when the runtime stops (a stop sent while a
        // pass is running ends the wait as soon as it begins).
        stopped.wait(Duration::from_secs(1)).await;
    }
}
