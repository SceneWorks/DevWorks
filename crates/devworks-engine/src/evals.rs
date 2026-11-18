//! Automatic recorded grading over host-authorized immutable suites. The durable session and
//! activation state is the queue: boot and periodic keyset sweeps repair lost wakeups. No
//! provider or inference port is available here. Suite selection/activation belongs to DW-72.
use crate::store;
use crate::workers::Ticker;
use devworks_core::ports::storage::{
    ApplicationStore, SessionPosition, SessionScope, StorageError,
};
use devworks_core::protocol::records::EvalSuiteSnapshot;
use devworks_core::services::evals::EvalService;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::watch;

const PAGE: u16 = 64;

/// Latest completed background catch-up pass, including an observable storage failure.
#[derive(Clone, Debug)]
pub struct EvalPass {
    /// Successful session/suite observations, including reports already recorded unchanged.
    pub result: Result<usize, StorageError>,
}

/// One owned grading loop. Memory and each read are bounded by a page; a sweep follows its
/// keyset through the whole durable population so older sessions cannot starve behind the head.
/// There is one grade in flight, and shutdown fences and drains it before returning.
pub struct EvalSupervisor {
    store: Arc<dyn ApplicationStore>,
    service: Arc<EvalService>,
    suites: Vec<EvalSuiteSnapshot>,
    ticker: Ticker,
    ticking: tokio::sync::Mutex<()>,
    status: watch::Sender<Option<EvalPass>>,
}
impl EvalSupervisor {
    /// The host MUST supply only already authorized immutable suites. An empty set disables
    /// automatic grading and performs no session or assignment scans.
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        service: Arc<EvalService>,
        suites: Vec<EvalSuiteSnapshot>,
    ) -> Arc<Self> {
        let (status, _) = watch::channel(None);
        Arc::new(Self {
            store,
            service,
            suites,
            ticker: Ticker::new(),
            ticking: tokio::sync::Mutex::new(()),
            status,
        })
    }
    pub fn watch(&self) -> watch::Receiver<Option<EvalPass>> {
        self.status.subscribe()
    }
    pub fn notify(&self) {
        self.ticker.notify();
    }
    pub fn stop(&self) {
        self.ticker.stop();
    }

    /// Stops admission and synchronously drains the one local grading transaction. Blocking
    /// jobs cannot be cancelled by dropping a Tokio handle, so the write fence is mandatory:
    /// after it closes no orphaned blocking job can persist a report.
    pub async fn drain(&self) {
        self.stop();
        self.ticker.drain().await;
    }
    /// Drop's synchronous equivalent, also fencing jobs that have not begun yet.
    pub(crate) fn close(&self) {
        self.stop();
        self.ticker.close();
    }
    pub async fn run(self: Arc<Self>, every: Duration) {
        self.ticker
            .run(every, || async {
                let result = self.tick().await;
                self.status.send_replace(Some(EvalPass { result }));
            })
            .await;
    }
    /// A complete catch-up sweep, serialized with the background loop. Failed sessions do not
    /// starve later ones: the first error is returned after the rest of the sweep is attempted.
    pub async fn tick(&self) -> Result<usize, StorageError> {
        let _one = self.ticking.lock().await;
        if self.suites.is_empty() || self.ticker.is_stopped() {
            return Ok(0);
        }
        let mut after = None;
        let mut graded = 0;
        let mut failure = None;
        loop {
            if self.ticker.is_stopped() {
                break;
            }
            let cursor = after.clone();
            let page = store::read(&self.store, move |s| {
                s.sessions()
                    .sessions(&SessionScope::All, None, cursor.as_ref(), PAGE)
            })
            .await?;
            let more = page.len() == usize::from(PAGE);
            after = page.last().map(|s| SessionPosition {
                created_at_ms: s.created_at_ms,
                id: s.id.clone(),
            });
            for session in page {
                if self.ticker.is_stopped() {
                    break;
                }
                let candidate = session.clone();
                if !store::read(&self.store, move |s| {
                    devworks_core::services::evals::automatic::automatically_eligible(
                        s.runs(),
                        &candidate,
                    )
                })
                .await?
                {
                    continue;
                }
                for suite in &self.suites {
                    if self.ticker.is_stopped() {
                        break;
                    }
                    let (store, service, suite, session_id, fence, stopping) = (
                        self.store.clone(),
                        self.service.clone(),
                        suite.clone(),
                        session.id.clone(),
                        self.ticker.fence(),
                        self.ticker.flag(),
                    );
                    let result = tokio::task::spawn_blocking(move || {
                        let closed = fence.read().unwrap_or_else(|p| p.into_inner());
                        if *closed || stopping.load(Ordering::SeqCst) {
                            return Ok(false);
                        }
                        service
                            .grade_automatically_cancellable(&*store, &session_id, &suite, &|| {
                                stopping.load(Ordering::SeqCst)
                            })
                            .map(|_| true)
                    })
                    .await
                    .map_err(|_| {
                        StorageError::InvalidRecord("automatic grading task panicked".into())
                    })?;
                    match result {
                        Ok(true) => graded += 1,
                        Ok(false) => {}
                        Err(StorageError::Cancelled) if self.ticker.is_stopped() => {}
                        Err(StorageError::InvalidRecord(reason))
                            if reason == "eval session is no longer terminal"
                                || reason == "automatic grading stopped" => {}
                        Err(e) => {
                            if failure.is_none() {
                                failure = Some(e);
                            }
                        }
                    }
                }
            }
            if !more {
                break;
            }
            tokio::task::yield_now().await;
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(graded),
        }
    }
}
