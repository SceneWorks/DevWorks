//! Index run supervision (DW-023): executes recorded index runs on the blocking pool with a
//! bounded number in flight, runs discovery and index housekeeping on an interval, signals a
//! committed cancellation to the run it names, and stops every run within the runtime's one
//! shutdown deadline.
//!
//! Each run executes on its own blocking task (parsing and Git are blocking work; Git trees are
//! owned by the process supervisor, so a stop kills them). At most
//! [`IndexingConfig::max_runs`] execute at once, and the worker never runs two runs of one
//! repository together. A stop signal is cooperative within one parse-progress or Git-poll
//! interval; past the deadline a run that has not finished is abandoned (its lease expires and
//! the next boot resumes it from its checkpoint).
use crate::workers::Ticker;
use devworks_core::locks::LockExt;
use devworks_core::ports::storage::StorageError;
use devworks_core::protocol::Id;
use devworks_core::services::indexing::IndexEnv;
use devworks_core::services::indexing::discover::{self, DiscoveryReport};
use devworks_core::services::indexing::worker::{Housekeeping, IndexWorker};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Bounds of index supervision.
#[derive(Clone, Debug)]
pub struct IndexingConfig {
    /// Runs executing at once (CODE-08 bounded concurrency).
    pub max_runs: usize,
    /// Time between claim passes when nothing wakes supervision.
    pub tick: Duration,
    /// Time between housekeeping passes (overlay expiry and staleness, payload reclamation).
    pub housekeeping_interval: Duration,
    /// Time between discovery scans of the configured base directories.
    pub discovery_interval: Duration,
    /// Scan once at boot (the configured base directories may have changed while closed).
    pub discover_at_boot: bool,
}
impl Default for IndexingConfig {
    fn default() -> Self {
        Self {
            max_runs: 2,
            tick: Duration::from_secs(2),
            housekeeping_interval: Duration::from_secs(60),
            discovery_interval: Duration::from_secs(15 * 60),
            discover_at_boot: true,
        }
    }
}

/// The latest discovery and housekeeping outcomes (for diagnostics and the UI).
#[derive(Clone, Debug, Default)]
pub struct IndexingStatus {
    pub discovery: Option<Result<DiscoveryReport, String>>,
    pub housekeeping: Option<Result<Housekeeping, String>>,
}

pub struct IndexSupervisor {
    analysis: std::sync::OnceLock<
        Arc<devworks_core::services::graph::repository_analysis::AnalysisService>,
    >,
    worker: Arc<IndexWorker>,
    config: IndexingConfig,
    ticker: Ticker,
    discover_now: AtomicBool,
    in_flight: AtomicUsize,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    status: watch::Sender<IndexingStatus>,
}

impl IndexSupervisor {
    pub fn new(worker: Arc<IndexWorker>, config: IndexingConfig) -> Arc<Self> {
        let discover_at_boot = config.discover_at_boot;
        let (status, _) = watch::channel(IndexingStatus::default());
        Arc::new(Self {
            analysis: std::sync::OnceLock::new(),
            worker,
            config,
            ticker: Ticker::new(),
            discover_now: AtomicBool::new(discover_at_boot),
            in_flight: AtomicUsize::new(0),
            tasks: Mutex::new(Vec::new()),
            status,
        })
    }
    pub(crate) fn install_analysis(
        &self,
        service: Arc<devworks_core::services::graph::repository_analysis::AnalysisService>,
    ) -> bool {
        self.analysis.set(service).is_ok()
    }
    pub fn worker(&self) -> &Arc<IndexWorker> {
        &self.worker
    }
    pub fn env(&self) -> &Arc<IndexEnv> {
        self.worker.env()
    }
    pub fn status(&self) -> watch::Receiver<IndexingStatus> {
        self.status.subscribe()
    }
    /// Wakes supervision (a run was recorded, a lease may have expired).
    pub fn notify(&self) {
        self.ticker.notify();
    }
    /// Asks for a discovery scan at the next pass (a section's base directory or exclusions
    /// changed).
    pub fn request_discovery(&self) {
        self.discover_now.store(true, Ordering::SeqCst);
        self.ticker.notify();
    }
    /// Signals the in-memory stop of a run whose cancellation committed (bound to that run).
    pub fn cancelled(&self, run: &Id) {
        self.worker.cancel_local(run);
        self.ticker.notify();
    }
    pub fn executing(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }

    /// Boot recovery: runs another boot left `Running` resume from their checkpoints.
    pub async fn recover(&self) -> Result<usize, StorageError> {
        let w = self.worker.clone();
        tokio::task::spawn_blocking(move || w.recover())
            .await
            .unwrap_or_else(|_| {
                Err(StorageError::unavailable(
                    devworks_core::ports::storage::Unavailable::HandlerPanicked,
                    "index recovery panicked",
                ))
            })
    }

    /// The supervision loop (until [`Self::stop`]).
    pub async fn run(self: Arc<Self>) {
        let mut last_housekeeping = tokio::time::Instant::now();
        let mut last_discovery = tokio::time::Instant::now();
        while !self.ticker.is_stopped() {
            if self.discover_now.swap(false, Ordering::SeqCst)
                || last_discovery.elapsed() >= self.config.discovery_interval
            {
                last_discovery = tokio::time::Instant::now();
                let _ = self.discover(None).await;
            }
            self.claim_pass().await;
            if last_housekeeping.elapsed() >= self.config.housekeeping_interval {
                last_housekeeping = tokio::time::Instant::now();
                let w = self.worker.clone();
                let me = self.clone();
                let report = tokio::task::spawn_blocking(move || {
                    let stop = || me.ticker.is_stopped();
                    let result = w.housekeeping(true, &stop)?;
                    if let Some(analysis) = me.analysis.get() {
                        analysis.maintain(&stop)?;
                    }
                    Ok::<_, StorageError>(result)
                })
                .await;
                let report = match report {
                    Ok(Ok(r)) => Ok(r),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(_) => Err("housekeeping panicked".into()),
                };
                self.status.send_modify(|s| s.housekeeping = Some(report));
            }
            self.ticker.wait(self.config.tick).await;
        }
    }

    /// One scan of the visible sections (`only`: just these section keys) for a host ingress.
    pub async fn discover(
        self: &Arc<Self>,
        only: Option<Vec<String>>,
    ) -> Result<DiscoveryReport, String> {
        let env = self.worker.env().clone();
        let me = self.clone();
        let report = tokio::task::spawn_blocking(move || {
            let stop = || me.ticker.is_stopped();
            let ctx = env.host_context(None);
            discover::scan(&env, &ctx, only.as_deref(), &stop)
        })
        .await;
        let report = match report {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("discovery panicked".into()),
        };
        self.status
            .send_modify(|s| s.discovery = Some(report.clone()));
        report
    }

    async fn claim_pass(self: &Arc<Self>) {
        while self.in_flight.load(Ordering::SeqCst) < self.config.max_runs.max(1)
            && !self.ticker.is_stopped()
        {
            let w = self.worker.clone();
            let claimed = tokio::task::spawn_blocking(move || w.claim()).await;
            let Ok(Ok(Some(run))) = claimed else { return };
            self.in_flight.fetch_add(1, Ordering::SeqCst);
            let (w, me) = (self.worker.clone(), self.clone());
            let task = tokio::task::spawn_blocking(move || {
                w.execute(run);
                me.in_flight.fetch_sub(1, Ordering::SeqCst);
                me.ticker.notify();
            });
            let mut tasks = self.tasks.locked();
            tasks.retain(|t| !t.is_finished());
            tasks.push(task);
        }
    }

    /// Stops claiming, ends the supervision loop and signals every executing run, without
    /// waiting for them.
    pub fn signal_stop(&self) {
        self.ticker.stop();
        self.worker.stop_all();
    }

    /// Stops claiming, signals every executing run and waits for them until `deadline` (the
    /// runtime's one shutdown deadline); runs still executing then are abandoned to recovery.
    pub async fn stop(&self, deadline: tokio::time::Instant) -> usize {
        self.signal_stop();
        let tasks = std::mem::take(&mut *self.tasks.locked());
        let mut abandoned = 0;
        for mut t in tasks {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match crate::dispatch::first(tokio::time::sleep(left), &mut t).await {
                crate::dispatch::Either::Left(()) => abandoned += 1,
                crate::dispatch::Either::Right(_) => {}
            }
        }
        abandoned
    }
}
