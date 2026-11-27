//! Git effects share the application's operation journal and owned process supervisor.
use crate::dispatch::Consumer;
use crate::effect_consumer::{PanicReasons, STANDARD_RETRY, effect_consumer};
use devworks_core::services::git::{EFFECT_CONSUMER, GitConfig, GitService};
use std::sync::Arc;

#[derive(Clone)]
pub struct GitComposition {
    pub config: GitConfig,
    /// The transport of GitHub API calls. When present, [`crate::boot`] sets the config's GitHub
    /// client over it and the provider connection service's secret resolver (which must be
    /// composed).
    pub github_http: Option<Arc<dyn devworks_core::ports::http::HttpTransport>>,
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
pub(crate) async fn run(
    service: Arc<GitService>,
    stopped: crate::workers::Stopped,
    commits: crate::workers::Commits,
) {
    let live = service.clone();
    commits
        .run(
            stopped,
            move || !live.is_stopped(),
            move || {
                let current = service.clone();
                async move {
                    tokio::task::spawn_blocking(move || {
                        let _ = current.reconcile_cancelled();
                        current.schedule_completed_cleanup().unwrap_or(false)
                    })
                    .await
                    .unwrap_or(false)
                }
            },
        )
        .await;
}
