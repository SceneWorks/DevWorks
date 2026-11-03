//! The shell's one boundary to the application (ARCH-02/03/08).
//!
//! [`Backend`] admits a framed query or command for a host-selected operation through the
//! runtime's gateway — the same validation, authority, receipts and use cases an agent tool
//! reaches. The shell has no store, no SQL and no second implementation of any rule.
//!
//! [`Client`] runs every request off the UI thread (the runtime blocks its caller on the store),
//! on a small worker pool, and hands replies back to the frame loop tagged with the [`Slot`]
//! that asked. A newer request for the same slot supersedes an older one, so a late reply never
//! overwrites fresher state. [`Client::wait_idle`] is the synchronization point tests use
//! (a condition variable, never a sleep).
use devworks_core::gateway::{CommandOutcome, QueryOutcome};
use devworks_core::protocol::Id;
use devworks_core::settings::view::Health;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Wakes the frame loop from another thread (egui `request_repaint`).
pub type Waker = Box<dyn Fn() + Send + Sync>;

/// The application operations the shell may use.
pub trait Backend: Send + Sync {
    /// Admits a query frame for `operation` with the host-bound user context.
    fn query(&self, operation: &str, frame: &[u8]) -> QueryOutcome;
    /// Admits a command frame for `operation` with the host-bound user context.
    fn command(&self, operation: &str, frame: &[u8]) -> CommandOutcome;
    /// Mints a workspace upload ticket for a file the user picked, dropped or pasted (user
    /// ingress only; the artifact service resolves and verifies it at `artifacts.save`).
    fn mint_workspace_upload(&self, file: &Path) -> Result<Id, String>;
    /// Keep-awake runtime health, distinct from the configured value (SET-02).
    fn keep_awake_health(&self) -> Health;
    /// "Queue state may have changed" hints.
    fn hints(&self) -> Option<watch::Receiver<u64>>;
    /// Registers the frame-loop waker hints call.
    fn set_waker(&self, waker: Waker);
    /// A fresh opaque token for request keys and query IDs.
    fn token(&self, prefix: &str) -> String;
    /// A directory for files the UI materializes before attaching them (pasted images).
    fn scratch_dir(&self) -> PathBuf;
}

/// Who is waiting for a reply. Equal slots supersede each other.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Slot {
    SettingsRead,
    SettingsValidate,
    SettingsHealth,
    SettingsSave,
    SettingsExplain(String),
    SessionsList,
    SessionStart,
    Transcript(String),
    Append(String),
    /// An attachment being stored for a draft (`draft`, local attachment ID).
    Attach(String, u64),
    /// A decoded preview for a pending attachment.
    Preview(String, u64),
    ArtifactsList,
    ResourcesList,
}

/// A finished request.
pub enum Reply {
    Query(QueryOutcome),
    Command(CommandOutcome),
    /// A request that ran several steps (attachment storage) or local work (preview decoding).
    Task(Result<TaskOutput, String>),
}

/// The output of a multi-step task.
pub enum TaskOutput {
    /// An attachment stored as a workspace artifact.
    Stored(CommandOutcome),
    /// A decoded preview image (RGBA, unmultiplied).
    Preview { size: [usize; 2], rgba: Vec<u8> },
}

type Job = Box<dyn FnOnce(&dyn Backend) -> Reply + Send>;

struct Pending {
    slot: Slot,
    ticket: u64,
    label: String,
    started: Instant,
}

struct Shared {
    in_flight: Mutex<usize>,
    idle: Condvar,
}

/// A long-running request as the status surface shows it (UI-06).
#[derive(Clone, Debug)]
pub struct Activity {
    pub label: String,
    pub elapsed: Duration,
}

/// Runs requests on worker threads and returns their replies to the frame loop.
pub struct Client {
    backend: Arc<dyn Backend>,
    jobs: Sender<(u64, Job)>,
    replies: Receiver<(u64, Reply)>,
    reply_tx: Sender<(u64, Reply)>,
    pending: Vec<Pending>,
    latest: HashMap<Slot, u64>,
    next: u64,
    shared: Arc<Shared>,
    waker: Arc<Mutex<Option<Waker>>>,
    open: bool,
    workers: Vec<std::thread::JoinHandle<()>>,
}

/// Worker threads; requests rarely contend, but an attachment upload must not hold up a
/// settings read.
const WORKERS: usize = 3;

impl Client {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        let (jobs, job_rx) = channel::<(u64, Job)>();
        let (reply_tx, replies) = channel::<(u64, Reply)>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let shared = Arc::new(Shared {
            in_flight: Mutex::new(0),
            idle: Condvar::new(),
        });
        let waker: Arc<Mutex<Option<Waker>>> = Arc::new(Mutex::new(None));
        let mut workers = Vec::with_capacity(WORKERS);
        for n in 0..WORKERS {
            let (rx, tx, be, sh, wk) = (
                job_rx.clone(),
                reply_tx.clone(),
                backend.clone(),
                shared.clone(),
                waker.clone(),
            );
            let spawned = std::thread::Builder::new()
                .name(format!("devworks-ui-request-{n}"))
                .spawn(move || {
                    loop {
                        let next = rx.lock().unwrap_or_else(|p| p.into_inner()).recv();
                        let Ok((ticket, job)) = next else { return };
                        let reply =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&*be)))
                                .unwrap_or_else(|_| {
                                    Reply::Task(Err("the request panicked".into()))
                                });
                        let _ = tx.send((ticket, reply));
                        {
                            let mut n = sh.in_flight.lock().unwrap_or_else(|p| p.into_inner());
                            *n = n.saturating_sub(1);
                            sh.idle.notify_all();
                        }
                        if let Some(w) = wk.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
                            w();
                        }
                    }
                });
            if let Ok(handle) = spawned {
                workers.push(handle);
            }
        }
        Self {
            backend,
            jobs,
            replies,
            reply_tx,
            pending: Vec::new(),
            latest: HashMap::new(),
            next: 0,
            shared,
            waker,
            open: true,
            workers,
        }
    }

    pub fn backend(&self) -> &Arc<dyn Backend> {
        &self.backend
    }

    /// Registers the frame-loop waker for replies and runtime hints.
    pub fn set_waker(&self, make: impl Fn() -> Waker) {
        *self.waker.lock().unwrap_or_else(|p| p.into_inner()) = Some(make());
        self.backend.set_waker(make());
    }

    /// Whether new requests are admitted (closed once shutdown begins).
    pub fn is_open(&self) -> bool {
        self.open
    }
    /// Stops admitting new requests (shutdown: Quiescing).
    pub fn close(&mut self) {
        self.open = false;
    }

    fn submit(&mut self, slot: Slot, label: &str, job: Job) -> Option<u64> {
        if !self.open {
            return None;
        }
        self.next += 1;
        let ticket = self.next;
        self.latest.insert(slot.clone(), ticket);
        self.pending.push(Pending {
            slot,
            ticket,
            label: label.to_owned(),
            started: Instant::now(),
        });
        *self
            .shared
            .in_flight
            .lock()
            .unwrap_or_else(|p| p.into_inner()) += 1;
        if self.jobs.send((ticket, job)).is_err() {
            // No worker could start: report the request as failed rather than leave it pending.
            let mut n = self
                .shared
                .in_flight
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            *n = n.saturating_sub(1);
            let _ = self.reply_tx.send((
                ticket,
                Reply::Task(Err("no request worker is running".into())),
            ));
        }
        Some(ticket)
    }

    pub fn query(&mut self, slot: Slot, label: &str, operation: &'static str, frame: Vec<u8>) {
        self.submit(
            slot,
            label,
            Box::new(move |b| Reply::Query(b.query(operation, &frame))),
        );
    }

    pub fn command(&mut self, slot: Slot, label: &str, operation: &'static str, frame: Vec<u8>) {
        self.submit(
            slot,
            label,
            Box::new(move |b| Reply::Command(b.command(operation, &frame))),
        );
    }

    /// Runs a multi-step task (for example: mint a ticket, hash the file, save the artifact).
    pub fn task(
        &mut self,
        slot: Slot,
        label: &str,
        task: impl FnOnce(&dyn Backend) -> Result<TaskOutput, String> + Send + 'static,
    ) {
        self.submit(slot, label, Box::new(move |b| Reply::Task(task(b))));
    }

    /// Replies that arrived since the last call, for the slots still waiting for them.
    pub fn poll(&mut self) -> Vec<(Slot, Reply)> {
        let mut out = Vec::new();
        while let Ok((ticket, reply)) = self.replies.try_recv() {
            let Some(at) = self.pending.iter().position(|p| p.ticket == ticket) else {
                continue;
            };
            let p = self.pending.remove(at);
            // Superseded by a newer request for the same slot: drop the stale reply.
            if self.latest.get(&p.slot) == Some(&ticket) {
                self.latest.remove(&p.slot);
                out.push((p.slot, reply));
            }
        }
        out
    }

    /// Whether a request for `slot` is waiting for its reply.
    pub fn is_pending(&self, slot: &Slot) -> bool {
        self.latest.contains_key(slot)
    }

    /// Requests in flight, oldest first (status surface).
    pub fn activities(&self) -> Vec<Activity> {
        self.pending
            .iter()
            .filter(|p| self.latest.get(&p.slot) == Some(&p.ticket))
            .map(|p| Activity {
                label: p.label.clone(),
                elapsed: p.started.elapsed(),
            })
            .collect()
    }

    /// Blocks until every submitted request has a reply waiting in [`Self::poll`], or `bound`
    /// passes. Returns whether it became idle.
    pub fn wait_idle(&self, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        let mut n = self
            .shared
            .in_flight
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        while *n > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            n = self
                .shared
                .idle
                .wait_timeout(n, left)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        true
    }
}

/// Stops the workers and waits for them, so the backend (and with it the runtime and its store)
/// is released before the caller continues: a closed window leaves nothing running.
impl Drop for Client {
    fn drop(&mut self) {
        let (closed, _) = channel();
        drop(std::mem::replace(&mut self.jobs, closed));
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}
