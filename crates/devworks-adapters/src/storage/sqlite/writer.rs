//! The dedicated write owner: one thread owning the only write connection.
//!
//! Admission uses one bounded Tokio `mpsc` mailbox per [`WritePriority`]; a full mailbox is a
//! visible [`StorageError::Busy`] (backpressure), never unbounded buffering. The thread waits on a
//! doorbell and picks authoritative work first, but after `bulk_share` consecutive authoritative
//! jobs while bulk work waits it runs one bulk job, so continuous control traffic cannot starve
//! indexing. Each job runs in its own `BEGIN IMMEDIATE` transaction.
use super::conn::db;
use super::repos::Unit;
use devworks_core::ports::storage::{
    Cancellation, StorageError, StoreInfo, Unavailable, WriteJob, WriteOptions, WritePriority,
};
use devworks_core::ports::{Clock, IdSource};
use rusqlite::Connection;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use tokio::sync::mpsc::{self, Receiver, Sender, error::TrySendError};

pub(crate) type RawWork = Box<dyn FnOnce(&Connection) + Send>;

pub(crate) enum Envelope {
    Job {
        job: Box<dyn WriteJob>,
        options: WriteOptions,
    },
    /// Adapter-internal work on the raw connection outside any transaction (diagnostics, tests).
    Raw(RawWork),
}

#[derive(Default)]
struct Doorbell {
    rung: Mutex<u64>,
    cv: Condvar,
}
impl Doorbell {
    fn ring(&self) {
        let mut n = self.rung.lock().unwrap_or_else(|p| p.into_inner());
        *n = n.wrapping_add(1);
        self.cv.notify_one();
    }
}

/// Test-only fault injection: report a lost acknowledgment after the next successful commit.
#[derive(Default)]
pub(crate) struct Faults {
    pub lose_next_ack: AtomicBool,
}

pub(crate) struct WriterHandle {
    authoritative: Sender<Envelope>,
    bulk: Sender<Envelope>,
    doorbell: Arc<Doorbell>,
    stopping: Arc<AtomicBool>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub faults: Arc<Faults>,
    thread: Option<JoinHandle<()>>,
}

pub(crate) struct WriterConfig {
    pub capacity: usize,
    pub bulk_share: u32,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdSource>,
    pub info: StoreInfo,
}

impl WriterHandle {
    pub fn start(conn: Connection, cfg: WriterConfig) -> Result<Self, StorageError> {
        let (a_tx, a_rx) = mpsc::channel(cfg.capacity.max(1));
        let (b_tx, b_rx) = mpsc::channel(cfg.capacity.max(1));
        let doorbell = Arc::new(Doorbell::default());
        let stopping = Arc::new(AtomicBool::new(false));
        let faults = Arc::new(Faults::default());
        let worker = Worker {
            conn,
            authoritative: a_rx,
            bulk: b_rx,
            doorbell: doorbell.clone(),
            stopping: stopping.clone(),
            faults: faults.clone(),
            streak: 0,
            serial: 0,
            cfg,
        };
        let thread = std::thread::Builder::new()
            .name("devworks-store-writer".into())
            .spawn(move || worker.run())
            .map_err(|e| StorageError::unavailable(Unavailable::WriterStopped, e.to_string()))?;
        Ok(Self {
            authoritative: a_tx,
            bulk: b_tx,
            doorbell,
            stopping,
            faults,
            thread: Some(thread),
        })
    }

    pub fn submit(
        &self,
        options: WriteOptions,
        job: Box<dyn WriteJob>,
    ) -> Result<(), StorageError> {
        if options
            .cancellation
            .as_ref()
            .is_some_and(Cancellation::is_cancelled)
        {
            return Err(StorageError::Cancelled);
        }
        let sender = match options.priority {
            WritePriority::Authoritative => &self.authoritative,
            WritePriority::Bulk => &self.bulk,
        };
        self.send(sender, Envelope::Job { job, options })
    }

    pub fn submit_raw(&self, work: RawWork) -> Result<(), StorageError> {
        self.send(&self.authoritative, Envelope::Raw(work))
    }

    fn send(&self, sender: &Sender<Envelope>, envelope: Envelope) -> Result<(), StorageError> {
        match sender.try_send(envelope) {
            Ok(()) => {
                self.doorbell.ring();
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(StorageError::Busy),
            Err(TrySendError::Closed(_)) => Err(StorageError::unavailable(
                Unavailable::WriterStopped,
                "write owner stopped",
            )),
        }
    }

    /// Stops admission, finishes queued jobs as not dispatched, records clean shutdown and joins.
    pub fn stop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.doorbell.ring();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
impl Drop for WriterHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Worker {
    conn: Connection,
    authoritative: Receiver<Envelope>,
    bulk: Receiver<Envelope>,
    doorbell: Arc<Doorbell>,
    stopping: Arc<AtomicBool>,
    faults: Arc<Faults>,
    streak: u32,
    /// Serial of the current unit of work (savepoint ownership).
    serial: u64,
    cfg: WriterConfig,
}

/// Rolls back on drop unless committed: a write unit can never be left open or half-applied.
pub(crate) struct TxGuard<'c> {
    conn: &'c Connection,
    open: bool,
}
impl<'c> TxGuard<'c> {
    pub fn begin(conn: &'c Connection) -> Result<Self, StorageError> {
        db(conn.execute_batch("BEGIN IMMEDIATE"))?;
        Ok(Self { conn, open: true })
    }
    /// Commits. Any failure here is ambiguous to the caller and reported as `CommitUnknown`.
    pub fn commit(mut self) -> Result<(), StorageError> {
        self.open = false;
        match self.conn.execute_batch("COMMIT") {
            Ok(()) => Ok(()),
            Err(_) => {
                if !self.conn.is_autocommit() {
                    let _ = self.conn.execute_batch("ROLLBACK");
                }
                Err(StorageError::CommitUnknown)
            }
        }
    }
    pub fn rollback(mut self) {
        self.open = false;
        let _ = self.conn.execute_batch("ROLLBACK");
    }
}
impl Drop for TxGuard<'_> {
    fn drop(&mut self) {
        if self.open && !self.conn.is_autocommit() {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
    }
}

impl Worker {
    fn next(&mut self) -> Option<Envelope> {
        let bulk_due = self.streak >= self.cfg.bulk_share.max(1);
        if bulk_due && let Ok(e) = self.bulk.try_recv() {
            self.streak = 0;
            return Some(e);
        }
        if let Ok(e) = self.authoritative.try_recv() {
            self.streak = self.streak.saturating_add(1);
            return Some(e);
        }
        let e = self.bulk.try_recv().ok();
        if e.is_some() {
            self.streak = 0;
        }
        e
    }

    fn run(mut self) {
        loop {
            let seen = *self.doorbell.rung.lock().unwrap_or_else(|p| p.into_inner());
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
            if let Some(envelope) = self.next() {
                self.execute(envelope);
                continue;
            }
            let guard = self.doorbell.rung.lock().unwrap_or_else(|p| p.into_inner());
            let _unused = self
                .doorbell
                .cv
                .wait_while(guard, |n| {
                    *n == seen && !self.stopping.load(Ordering::SeqCst)
                })
                .unwrap_or_else(|p| p.into_inner());
        }
        // Stop admission, then report everything still queued as never dispatched.
        self.authoritative.close();
        self.bulk.close();
        while let Some(e) = self.next() {
            if let Envelope::Job { job, .. } = e {
                finish(
                    job,
                    Err(StorageError::unavailable(
                        Unavailable::WriterStopped,
                        "write owner stopped before dispatch",
                    )),
                );
            }
        }
        if let Some(boot) = &self.cfg.info.boot_id {
            let now = self.cfg.clock.now_ms().get();
            let recorded = self.conn.execute(
                "UPDATE app_boots SET stopped_at=?1,shutdown_state='Clean' WHERE id=?2",
                rusqlite::params![now, boot.as_str()],
            );
            // Refresh planner statistics only on a store that just committed its clean shutdown:
            // a failed store (full disk, I/O error, lost lock) is left as it is. A read-only store
            // has no writer, so it never gets here.
            if matches!(recorded, Ok(1)) && self.conn.is_autocommit() {
                let _ = self.conn.execute_batch("PRAGMA optimize");
            }
        }
    }

    fn expired(&self, options: &WriteOptions, now: i64) -> bool {
        options
            .cancellation
            .as_ref()
            .is_some_and(Cancellation::is_cancelled)
            || options.deadline_ms.is_some_and(|d| now > d)
    }

    fn execute(&mut self, envelope: Envelope) {
        let (mut job, options) = match envelope {
            Envelope::Job { job, options } => (job, options),
            Envelope::Raw(work) => {
                let _ = catch_unwind(AssertUnwindSafe(|| work(&self.conn)));
                if !self.conn.is_autocommit() {
                    let _ = self.conn.execute_batch("ROLLBACK");
                }
                return;
            }
        };
        let now = self.cfg.clock.now_ms().get();
        if self.expired(&options, now) {
            // Cancellation before admission prevents dispatch entirely.
            finish(job, Err(StorageError::Cancelled));
            return;
        }
        let tx = match TxGuard::begin(&self.conn) {
            Ok(tx) => tx,
            Err(e) => {
                finish(job, Err(e));
                return;
            }
        };
        self.serial += 1;
        let (ran, open_savepoints) = {
            let mut unit = Unit {
                conn: &self.conn,
                now,
                ids: &*self.cfg.ids,
                info: &self.cfg.info,
                savepoints: 0,
                serial: self.serial,
            };
            let ran = catch_unwind(AssertUnwindSafe(|| job.run(&mut unit)));
            (ran, unit.savepoints)
        };
        let outcome = match ran {
            // SQLite rolled the transaction back on an error the job swallowed; any statements
            // after that ran in autocommit mode, so what is durable is unknown.
            Ok(Ok(())) if self.conn.is_autocommit() => Err(StorageError::CommitUnknown),
            Ok(Ok(())) if open_savepoints != 0 => {
                tx.rollback();
                Err(StorageError::InvalidRecord(format!(
                    "write job left {open_savepoints} savepoint(s) open; rolled back"
                )))
            }
            Ok(Ok(())) if self.expired(&options, self.cfg.clock.now_ms().get()) => {
                // Admitted but cancelled before commit: roll back and say so.
                tx.rollback();
                Err(StorageError::Cancelled)
            }
            Ok(Ok(())) => match tx.commit() {
                Ok(()) if cfg!(test) && self.faults.lose_next_ack.swap(false, Ordering::SeqCst) => {
                    Err(StorageError::CommitUnknown)
                }
                other => other,
            },
            Ok(Err(e)) => {
                tx.rollback();
                Err(e)
            }
            Err(_) => {
                tx.rollback();
                Err(StorageError::unavailable(
                    Unavailable::HandlerPanicked,
                    "write job panicked; rolled back",
                ))
            }
        };
        finish(job, outcome);
    }
}

/// Reports an outcome; a panicking responder cannot take the write owner down with it.
fn finish(job: Box<dyn WriteJob>, outcome: Result<(), StorageError>) {
    let _ = catch_unwind(AssertUnwindSafe(move || job.finish(outcome)));
}
