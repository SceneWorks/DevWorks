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

/// What a polling pass wants next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Next {
    /// More work is waiting: run again at once.
    Now,
    /// Nothing is waiting: sleep until a commit or the fallback interval.
    Idle,
    /// Work becomes due by time alone (a retry window, an expiry): wake then at the latest.
    Within(Duration),
}

impl From<bool> for Next {
    fn from(more: bool) -> Self {
        if more { Self::Now } else { Self::Idle }
    }
}

/// The least time between two passes started by commit wakes, so a burst of commits is one pass.
const MIN_WAKE_GAP: Duration = Duration::from_millis(50);
/// The most a loop whose own passes keep committing waits between commit-woken passes.
const MAX_WAKE_GAP: Duration = Duration::from_secs(5);

/// What wakes a polling loop between store passes: any store commit that changed data (the
/// write owner's [`devworks_core::ports::storage::CommitSignal`], so a command, a supervisor
/// write and a host-observed write all count), a finished delivery, the earliest deadline the
/// last pass reported, else a fallback interval that repairs a missed signal.
///
/// A loop does not wake itself in a storm: a pass that changed nothing commits nothing, wakes
/// are at least [`MIN_WAKE_GAP`] apart, and a pass during which commits happened (its own or
/// others') doubles that gap up to [`MAX_WAKE_GAP`] until a quiet pass resets it.
#[derive(Clone)]
pub(crate) struct Commits {
    stored: watch::Receiver<u64>,
    delivered: watch::Receiver<u64>,
    fallback: Duration,
    passes: Arc<AtomicU64>,
    state: Arc<LoopState>,
}

/// What one commit-woken loop has seen, so the runtime can say whether any loop still has a wake
/// or a pass pending. A loop is busy from creation until its first pass settles into a wait.
pub(crate) struct LoopState {
    busy: AtomicBool,
    stored: AtomicU64,
    delivered: AtomicU64,
}

impl LoopState {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            busy: AtomicBool::new(true),
            stored: AtomicU64::new(0),
            delivered: AtomicU64::new(0),
        })
    }
    /// True when the loop waits and has seen every commit and delivery up to the given counts.
    pub(crate) fn caught_up(&self, stored: u64, delivered: u64) -> bool {
        !self.busy.load(Ordering::SeqCst)
            && self.stored.load(Ordering::SeqCst) == stored
            && self.delivered.load(Ordering::SeqCst) == delivered
    }
}

impl Commits {
    pub(crate) fn new(
        stored: watch::Receiver<u64>,
        delivered: watch::Receiver<u64>,
        fallback: Duration,
        passes: Arc<AtomicU64>,
        state: Arc<LoopState>,
    ) -> Self {
        Self {
            stored,
            delivered,
            fallback,
            passes,
            state,
        }
    }
    /// Counts one store pass (the idle-runtime test reads the total).
    pub(crate) fn pass(&self) {
        self.passes.fetch_add(1, Ordering::SeqCst);
    }
    /// Marks every commit so far as seen and returns the commit count.
    fn observe(&mut self) -> u64 {
        let delivered = *self.delivered.borrow_and_update();
        let stored = *self.stored.borrow_and_update();
        self.state.delivered.store(delivered, Ordering::SeqCst);
        self.state.stored.store(stored, Ordering::SeqCst);
        stored
    }
    /// Runs `pass` until `live` is false or the loop is stopped. The first pass runs without
    /// waiting; after each pass the loop runs again at once ([`Next::Now`]) or waits.
    pub(crate) async fn run<L, F, Fut, N>(mut self, mut stopped: Stopped, live: L, mut pass: F)
    where
        L: Fn() -> bool,
        F: FnMut() -> Fut,
        Fut: Future<Output = N>,
        N: Into<Next>,
    {
        let mut gap = MIN_WAKE_GAP;
        while live() && !stopped.is_set() {
            self.state.busy.store(true, Ordering::SeqCst);
            self.pass();
            let started = tokio::time::Instant::now();
            let before = self.observe();
            let next = pass().await.into();
            if next == Next::Now {
                continue;
            }
            self.state.busy.store(false, Ordering::SeqCst);
            // Commits during the pass are newer than what it read: they wake the wait below,
            // and the longer gap keeps a pass that commits on every run from spinning.
            let busy = *self.stored.borrow() != before;
            gap = if busy {
                (gap * 2).min(MAX_WAKE_GAP)
            } else {
                MIN_WAKE_GAP
            };
            let due = match next {
                Next::Within(at) => at.min(self.fallback),
                _ => self.fallback,
            };
            if self.wait(&mut stopped, due).await {
                // Woken by a commit: coalesce a burst, and keep the gap since the last start.
                let resume = started + gap;
                tokio::select! {
                    () = stopped.until_set() => {}
                    () = tokio::time::sleep_until(resume) => {}
                }
            }
        }
    }
    /// Waits for a commit, `due` or the stop signal; true when a commit ended it.
    async fn wait(&mut self, stopped: &mut Stopped, due: Duration) -> bool {
        async fn changed(rx: &mut watch::Receiver<u64>) {
            if rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
        tokio::select! {
            () = stopped.until_set() => false,
            () = changed(&mut self.stored) => true,
            () = changed(&mut self.delivered) => true,
            () = tokio::time::sleep(due) => false,
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

    /// Runs a `Commits` loop whose passes are `pass`, with an hour-long fallback, and returns its
    /// commit sender, pass counter and stop sender.
    fn spawn_commits<F>(
        pass: F,
    ) -> (
        watch::Sender<u64>,
        Arc<std::sync::atomic::AtomicUsize>,
        watch::Sender<bool>,
        tokio::task::JoinHandle<()>,
    )
    where
        F: Fn(&watch::Sender<u64>, usize) -> Next + Send + Sync + 'static,
    {
        let (stored_tx, stored) = watch::channel(0u64);
        let delivered = watch::channel(0u64).1;
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (stop, stopped) = watch::channel(false);
        let commits = Commits::new(
            stored,
            delivered,
            Duration::from_secs(3600),
            Arc::new(AtomicU64::new(0)),
            LoopState::new(),
        );
        let (count, sender) = (runs.clone(), stored_tx.clone());
        let task = tokio::spawn(commits.run(
            Stopped(stopped),
            || true,
            move || {
                let n = count.fetch_add(1, Ordering::SeqCst) + 1;
                let next = pass(&sender, n);
                async move { next }
            },
        ));
        (stored_tx, runs, stop, task)
    }

    /// A commit made during a pass leaves the loop not caught up until a later pass has seen it.
    #[test]
    fn a_commit_during_a_pass_keeps_the_loop_unsettled_until_the_next_pass() {
        rt().block_on(async {
            let (stored_tx, stored) = watch::channel(0u64);
            let delivered = watch::channel(0u64).1;
            let state = LoopState::new();
            let (stop, stopped) = watch::channel(false);
            let commits = Commits::new(
                stored,
                delivered,
                Duration::from_secs(3600),
                Arc::new(AtomicU64::new(0)),
                state.clone(),
            );
            let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let (count, sender) = (runs.clone(), stored_tx.clone());
            let (release_tx, release) = watch::channel(false);
            let task = tokio::spawn(commits.run(
                Stopped(stopped),
                || true,
                move || {
                    let n = count.fetch_add(1, Ordering::SeqCst) + 1;
                    let sender = sender.clone();
                    let mut release = release.clone();
                    async move {
                        if n == 1 {
                            // A commit lands after the loop recorded what it had seen.
                            sender.send(1).unwrap();
                            while !*release.borrow_and_update() {
                                release.changed().await.unwrap();
                            }
                        }
                        Next::Idle
                    }
                },
            ));
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(
                !state.caught_up(1, 0),
                "a loop mid-pass is reported settled"
            );
            release_tx.send(true).unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                while !state.caught_up(1, 0) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the loop never caught up with the commit");
            assert_eq!(runs.load(Ordering::SeqCst), 2);
            stop.send(true).unwrap();
            task.await.unwrap();
        });
    }

    /// A pass that reports a deadline is run again then, not after the hour-long interval.
    #[test]
    fn a_pass_deadline_wakes_the_loop_before_the_fallback_interval() {
        rt().block_on(async {
            let (_tx, runs, stop, task) =
                spawn_commits(|_, _| Next::Within(Duration::from_millis(100)));
            tokio::time::sleep(Duration::from_millis(550)).await;
            let seen = runs.load(Ordering::SeqCst);
            assert!(
                (4..=7).contains(&seen),
                "{seen} passes in 550 ms at a 100 ms deadline"
            );
            stop.send_replace(true);
            task.await.unwrap();
        });
    }

    /// One commit runs exactly one more pass; a burst of commits is coalesced.
    #[test]
    fn a_commit_wakes_an_idle_loop_once_and_a_burst_is_one_pass() {
        rt().block_on(async {
            let (tx, runs, stop, task) = spawn_commits(|_, _| Next::Idle);
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(runs.load(Ordering::SeqCst), 1);
            for _ in 0..20 {
                tx.send_modify(|n| *n += 1);
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
            assert_eq!(runs.load(Ordering::SeqCst), 2);
            stop.send_replace(true);
            task.await.unwrap();
        });
    }

    /// A pass that commits on every run does not wake itself in a storm: the gap between its
    /// commit-woken passes doubles.
    #[test]
    fn a_pass_that_always_commits_does_not_spin() {
        rt().block_on(async {
            let (_tx, runs, stop, task) = spawn_commits(|tx, _| {
                tx.send_modify(|n| *n += 1);
                Next::Idle
            });
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let seen = runs.load(Ordering::SeqCst);
            // Gaps of 100, 200, 400, 800 ms: boot pass plus about four more.
            assert!((3..=7).contains(&seen), "{seen} passes in 1.5 s");
            stop.send_replace(true);
            task.await.unwrap();
        });
    }
}
