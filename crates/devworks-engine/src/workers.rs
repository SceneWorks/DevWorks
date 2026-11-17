//! The background workers [`crate::boot`] starts, registered in one list that
//! [`crate::Runtime`]'s stop, shutdown and drop iterate, so none of them can miss a worker.
//!
//! Each registration also says where shutdown signals the worker ([`StopAt`]) and where it waits
//! for the worker's in-flight work ([`DrainAt`]); within one point, workers are handled in
//! registration order. Every worker is signalled again when the remaining work stops, and every
//! loop is joined within what is left of the one shutdown bound (past it, the task is aborted).

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// A boxed future a worker hands to the registry.
pub(crate) type WorkerFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// One background worker: a loop the registry spawns once, a stop signal that ends it after the
/// pass under way, and a drain that waits for work it started outside the loop.
pub(crate) trait Worker: Send + Sync + 'static {
    /// The name diagnostics and test failures use.
    fn name(&self) -> &'static str;
    /// The loop, called once. It returns after [`Worker::stop`]; every wait between its passes
    /// also ends at the stop signal, so no loop holds shutdown for its interval.
    fn run(self: Arc<Self>) -> WorkerFuture<'static>;
    /// Signals the loop to end and wakes its wait. Never blocks; may be called more than once.
    fn stop(&self);
    /// Waits for work the worker started outside its loop, at most until `deadline` unless the
    /// worker's writes must be fenced before shutdown returns.
    fn drain(&self, _deadline: Instant) -> WorkerFuture<'_> {
        Box::pin(async {})
    }
}

/// Where [`crate::Runtime::shutdown`] signals a worker first (every worker is signalled again
/// when the remaining work stops, and by [`crate::Runtime`]'s drop).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopAt {
    /// Before anything else: schedule admission freezes, so no occurrence is claimed and no
    /// scheduled run started from here on.
    Schedule,
    /// As shutdown begins: workers that admit, readmit or settle work outside commands.
    Admission,
    /// Just before the admission fence closes: once signalled, the diagnostics monitor starts
    /// no further write.
    Fence,
    /// After live work is held for Continue, together with every other worker.
    Work,
}

/// Where [`crate::Runtime::shutdown`] waits for a worker's in-flight work, in shutdown order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DrainAt {
    /// First, before admission closes: preparation, admission and settlement of a saved
    /// operation are one drained unit, so no blocking readmission is orphaned.
    Readmission,
    /// Next, still before admission closes.
    BeforeFence,
    /// Once admission has closed.
    AfterFence,
    /// Once every loop is joined (in-flight deliveries).
    AfterJoin,
    /// Once package helpers had their cooperative stop (index runs, then scheduled runs).
    AfterPackages,
}

struct Entry {
    worker: Arc<dyn Worker>,
    stop_at: StopAt,
    drain_at: Option<DrainAt>,
    task: Mutex<Option<JoinHandle<()>>>,
}

/// The registered workers, in registration order.
#[derive(Default)]
pub(crate) struct Workers {
    entries: Vec<Entry>,
}

impl Workers {
    /// Registers `worker` and spawns its loop on the current Tokio runtime.
    pub(crate) fn start(
        &mut self,
        stop_at: StopAt,
        drain_at: Option<DrainAt>,
        worker: Arc<dyn Worker>,
    ) {
        let task = tokio::spawn(worker.clone().run());
        self.entries.push(Entry {
            worker,
            stop_at,
            drain_at,
            task: Mutex::new(Some(task)),
        });
    }

    /// Signals every worker registered to stop at `at`.
    pub(crate) fn stop_at(&self, at: StopAt) {
        for entry in self.entries.iter().filter(|e| e.stop_at == at) {
            entry.worker.stop();
        }
    }

    /// Signals every worker.
    pub(crate) fn stop_all(&self) {
        for entry in &self.entries {
            entry.worker.stop();
        }
    }

    /// Drains every worker registered to drain at `at`, passing each the one `deadline`. A drain
    /// is never cancelled here: one that closes a write fence must finish before shutdown
    /// returns, so it bounds its own wait.
    pub(crate) async fn drain(&self, at: DrainAt, deadline: Instant) {
        for entry in self.entries.iter().filter(|e| e.drain_at == Some(at)) {
            entry.worker.drain(deadline).await;
        }
    }

    /// Awaits every loop until `deadline` and aborts those still running then. Returns the names
    /// of the workers it aborted.
    pub(crate) async fn join(&self, deadline: Instant) -> Vec<&'static str> {
        let mut aborted = Vec::new();
        for entry in &self.entries {
            let task = entry.task.lock().unwrap_or_else(|p| p.into_inner()).take();
            let Some(mut task) = task else { continue };
            if tokio::time::timeout_at(deadline, &mut task).await.is_err() {
                task.abort();
                aborted.push(entry.worker.name());
            }
        }
        aborted
    }
}

/// The stop flag, wake and interval every engine supervisor repeats, with the write fence its
/// drain closes: one stop ends the passes, a wake runs the next pass now, and the wait between
/// passes ends at a wake, a stop or the interval, whichever comes first.
pub(crate) struct Ticker {
    stopping: Arc<AtomicBool>,
    fence: Arc<RwLock<bool>>,
    wake: Notify,
}

impl Default for Ticker {
    fn default() -> Self {
        Self::new()
    }
}

impl Ticker {
    pub(crate) fn new() -> Self {
        Self {
            stopping: Arc::new(AtomicBool::new(false)),
            fence: Arc::new(RwLock::new(false)),
            wake: Notify::new(),
        }
    }
    /// Runs the next pass now (a wake sent during a pass runs another right after it).
    pub(crate) fn notify(&self) {
        self.wake.notify_one();
    }
    /// Freezes the loop's work and wakes its wait. Never blocks; may be called more than once.
    pub(crate) fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }
    pub(crate) fn is_stopped(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }
    /// The stop flag, for blocking work that outlives the borrow of the supervisor.
    pub(crate) fn flag(&self) -> Arc<AtomicBool> {
        self.stopping.clone()
    }
    /// The write fence [`Self::drain`] and [`Self::close`] shut: blocking jobs check it inside
    /// their write and refuse once it is closed.
    pub(crate) fn fence(&self) -> Arc<RwLock<bool>> {
        self.fence.clone()
    }
    /// Stops, then waits for admitted blocking writes and closes the fence behind them, so no
    /// orphaned blocking job can persist after this returns.
    pub(crate) async fn drain(&self) {
        self.stop();
        let fence = self.fence.clone();
        let _ = tokio::task::spawn_blocking(move || {
            *fence.write().unwrap_or_else(|p| p.into_inner()) = true;
        })
        .await;
    }
    /// [`Self::drain`] for a synchronous caller (drop), also fencing jobs that have not begun.
    pub(crate) fn close(&self) {
        self.stop();
        *self.fence.write().unwrap_or_else(|p| p.into_inner()) = true;
    }
    /// Waits for a wake, a stop or `interval`.
    pub(crate) async fn wait(&self, interval: Duration) {
        let _ = crate::dispatch::first(
            self.wake.notified(),
            tokio::time::sleep(interval.max(Duration::from_millis(1))),
        )
        .await;
    }
    /// Passes until stopped: `pass`, then the wait.
    pub(crate) async fn run<F, Fut>(&self, interval: Duration, mut pass: F)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = ()>,
    {
        while !self.is_stopped() {
            pass().await;
            if self.is_stopped() {
                break;
            }
            self.wait(interval).await;
        }
    }
}

/// The stop signal of a [`Loop`]: checked between passes and awaited with each interval.
#[derive(Clone)]
pub(crate) struct Stopped(watch::Receiver<bool>);

impl Stopped {
    /// Whether the loop was asked to stop.
    pub(crate) fn is_set(&self) -> bool {
        *self.0.borrow()
    }
    /// Waits until the loop is asked to stop.
    pub(crate) async fn until_set(&mut self) {
        let _ = self.0.wait_for(|stopped| *stopped).await;
    }
    /// Waits `interval`, or less when the loop is asked to stop (a stop sent before the wait
    /// began ends it at once).
    pub(crate) async fn wait(&mut self, interval: Duration) {
        let _ = tokio::time::timeout(interval, self.0.wait_for(|stopped| *stopped)).await;
    }
}

/// What wakes a polling loop between store passes: a post-commit signal, else a fallback
/// interval that repairs a missed signal and covers work that becomes due by time alone. The
/// two signals are an admitted command that may have changed domain state and a finished
/// delivery (see [`crate::Runtime::state_changes`] and [`crate::Dispatcher::subscribe`]).
#[derive(Clone)]
pub(crate) struct Commits {
    admitted: watch::Receiver<u64>,
    delivered: watch::Receiver<u64>,
    fallback: Duration,
    passes: Arc<AtomicU64>,
}

impl Commits {
    pub(crate) fn new(
        admitted: watch::Receiver<u64>,
        delivered: watch::Receiver<u64>,
        fallback: Duration,
        passes: Arc<AtomicU64>,
    ) -> Self {
        Self {
            admitted,
            delivered,
            fallback,
            passes,
        }
    }
    /// Counts one store pass (the idle-runtime test reads the total).
    pub(crate) fn pass(&self) {
        self.passes.fetch_add(1, Ordering::SeqCst);
    }
    /// Runs `pass` until `live` is false or the loop is stopped:
    /// a pass that returns true runs again at once (more work is waiting), any other waits for a
    /// commit signal or the fallback interval. The first pass runs without waiting.
    pub(crate) async fn run<L, F, Fut>(mut self, mut stopped: Stopped, live: L, mut pass: F)
    where
        L: Fn() -> bool,
        F: FnMut() -> Fut,
        Fut: Future<Output = bool>,
    {
        while live() && !stopped.is_set() {
            self.pass();
            if !pass().await {
                self.wait(&mut stopped).await;
            }
        }
    }
    /// Waits for a commit seen since the last wait, the fallback interval, or the stop signal.
    pub(crate) async fn wait(&mut self, stopped: &mut Stopped) {
        async fn changed(rx: &mut watch::Receiver<u64>) {
            if rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
        tokio::select! {
            () = stopped.until_set() => {}
            () = changed(&mut self.admitted) => {}
            () = changed(&mut self.delivered) => {}
            () = tokio::time::sleep(self.fallback) => {}
        }
    }
}

/// A worker built from a loop and the stop of the service it drives.
pub(crate) struct Loop {
    name: &'static str,
    body: Mutex<Option<WorkerFuture<'static>>>,
    signal: watch::Sender<bool>,
    on_stop: Option<Box<dyn Fn() + Send + Sync>>,
    on_drain: Option<Box<dyn Fn(Instant) -> WorkerFuture<'static> + Send + Sync>>,
}

impl Loop {
    /// A worker running `body`, which gets the loop's stop signal.
    pub(crate) fn new<F, Fut>(name: &'static str, body: F) -> Self
    where
        F: FnOnce(Stopped) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (signal, stopped) = watch::channel(false);
        Self {
            name,
            body: Mutex::new(Some(Box::pin(body(Stopped(stopped))))),
            signal,
            on_stop: None,
            on_drain: None,
        }
    }
    /// Also calls `stop` (the driven service's own stop) before the loop's signal is sent.
    pub(crate) fn on_stop(mut self, stop: impl Fn() + Send + Sync + 'static) -> Self {
        self.on_stop = Some(Box::new(stop));
        self
    }
    /// Drains with `drain`.
    pub(crate) fn on_drain<Fut>(
        mut self,
        drain: impl Fn(Instant) -> Fut + Send + Sync + 'static,
    ) -> Self
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.on_drain = Some(Box::new(move |deadline| Box::pin(drain(deadline))));
        self
    }
}

impl Worker for Loop {
    fn name(&self) -> &'static str {
        self.name
    }
    fn run(self: Arc<Self>) -> WorkerFuture<'static> {
        self.body
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .unwrap_or_else(|| Box::pin(async {}))
    }
    fn stop(&self) {
        if let Some(stop) = &self.on_stop {
            stop();
        }
        self.signal.send_replace(true);
    }
    fn drain(&self, deadline: Instant) -> WorkerFuture<'_> {
        match &self.on_drain {
            Some(drain) => drain(deadline),
            None => Box::pin(async {}),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A worker whose loop never ends on its own: it passes, then waits an hour or until stopped.
    struct Endless {
        stopped: watch::Sender<bool>,
        passes: std::sync::atomic::AtomicUsize,
        ended: AtomicBool,
    }
    impl Worker for Endless {
        fn name(&self) -> &'static str {
            "endless"
        }
        fn run(self: Arc<Self>) -> WorkerFuture<'static> {
            Box::pin(async move {
                let mut stopped = Stopped(self.stopped.subscribe());
                while !stopped.is_set() {
                    self.passes.fetch_add(1, Ordering::SeqCst);
                    stopped.wait(Duration::from_secs(3600)).await;
                }
                self.ended.store(true, Ordering::SeqCst);
            })
        }
        fn stop(&self) {
            self.stopped.send_replace(true);
        }
    }

    /// A worker that ignores its stop signal.
    struct Stubborn;
    impl Worker for Stubborn {
        fn name(&self) -> &'static str {
            "stubborn"
        }
        fn run(self: Arc<Self>) -> WorkerFuture<'static> {
            Box::pin(std::future::pending())
        }
        fn stop(&self) {}
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    /// Shutdown signals every registered worker and joins it within the bound, though the loop
    /// would otherwise wait out an hour-long interval.
    #[test]
    fn shutdown_stops_and_joins_a_registered_worker_that_never_ends_on_its_own() {
        rt().block_on(async {
            let worker = Arc::new(Endless {
                stopped: watch::channel(false).0,
                passes: Default::default(),
                ended: AtomicBool::new(false),
            });
            let mut workers = Workers::default();
            workers.start(StopAt::Work, None, worker.clone());
            // The first pass runs once the worker task is polled; the next waits an hour.
            while worker.passes.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            assert_eq!(worker.passes.load(Ordering::SeqCst), 1);
            let bound = Duration::from_secs(30);
            let started = Instant::now();
            workers.stop_all();
            let aborted = workers.join(started + bound).await;
            assert!(aborted.is_empty(), "aborted {aborted:?}");
            assert!(worker.ended.load(Ordering::SeqCst));
            assert!(started.elapsed() < Duration::from_secs(1));
        });
    }

    /// A worker that never honours its stop is aborted at the deadline, so it cannot hold
    /// shutdown past the bound.
    #[test]
    fn a_worker_that_ignores_its_stop_is_aborted_at_the_deadline() {
        rt().block_on(async {
            let mut workers = Workers::default();
            workers.start(StopAt::Work, None, Arc::new(Stubborn));
            let started = Instant::now();
            workers.stop_all();
            let aborted = workers.join(started + Duration::from_millis(100)).await;
            assert_eq!(aborted, ["stubborn"]);
            assert!(started.elapsed() < Duration::from_secs(5));
        });
    }

    /// Only the workers registered at a point are signalled or drained there.
    #[test]
    fn stop_and_drain_points_select_their_workers() {
        rt().block_on(async {
            let drained = Arc::new(Mutex::new(Vec::new()));
            let mut workers = Workers::default();
            for (name, stop_at, drain_at) in [
                ("first", StopAt::Schedule, Some(DrainAt::AfterPackages)),
                ("second", StopAt::Work, Some(DrainAt::Readmission)),
                ("third", StopAt::Work, None),
            ] {
                let log = drained.clone();
                let worker = Loop::new(name, |mut stopped| async move {
                    while !stopped.is_set() {
                        stopped.wait(Duration::from_secs(3600)).await;
                    }
                })
                .on_drain(move |_| {
                    let log = log.clone();
                    async move { log.lock().unwrap().push(name) }
                });
                workers.start(stop_at, drain_at, Arc::new(worker));
            }
            workers.stop_at(StopAt::Schedule);
            let deadline = Instant::now() + Duration::from_secs(30);
            workers.drain(DrainAt::Readmission, deadline).await;
            assert_eq!(*drained.lock().unwrap(), ["second"]);
            workers.drain(DrainAt::AfterPackages, deadline).await;
            assert_eq!(*drained.lock().unwrap(), ["second", "first"]);
            // Only "first" was signalled: joining with a short deadline aborts the other two.
            let aborted = workers
                .join(Instant::now() + Duration::from_millis(100))
                .await;
            assert_eq!(aborted, ["second", "third"]);
        });
    }
}
