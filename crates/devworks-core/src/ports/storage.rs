//! Persistence ports (STORE-09/10/11/12): the transaction owner, read snapshots and the
//! operational repositories every mutation shares.
//!
//! # Boundary rationale
//!
//! * **Driver-free.** Nothing here names SQLite, SQL, statements, rows, migrations, FTS or vector
//!   tables. Consumers see domain identities, revisions, committed sequences, typed records and
//!   [`StorageError`]. There is no generic execute-SQL entry point. The only implementation is
//!   `devworks-adapters::storage::sqlite`, constructed at the composition root and injected as
//!   `&dyn ApplicationStore`.
//! * **One unit of work per logical mutation.** [`ApplicationStore::submit_write`] hands an owned
//!   [`WriteJob`] to the single write owner. The job runs synchronously inside one write
//!   transaction with borrowed, transaction-scoped repositories ([`WriteTransaction`]); it commits
//!   only if the job returns `Ok`, and every error, panic or cancellation rolls the entire unit
//!   back. Repositories never commit independently and their borrows cannot escape the job.
//!   Owned values leave the job:
//!
//!   ```
//!   use devworks_core::ports::storage::*;
//!   fn owned(store: &dyn ApplicationStore) -> Result<i64, StorageError> {
//!       run_write(store, WriteOptions::host_state(), |tx| tx.history().page(&EventFilter::All, 0, 1).map(|p| p.watermark))
//!   }
//!   ```
//!
//!   but repository handles do not:
//!
//!   ```compile_fail
//!   use devworks_core::ports::storage::*;
//!   fn escape(store: &dyn ApplicationStore) {
//!       // A repository handle cannot be returned out of its transaction.
//!       let _ = run_write(store, WriteOptions::host_state(), |tx| Ok(tx.history()));
//!   }
//!   ```
//! * **Every write names its class.** [`WriteOptions`] has no `Default`: each `run_write` or
//!   `submit_write` site builds its options with a [`WriteClass`] constructor
//!   (`WriteOptions::recovery()`, `::housekeeping()`, ...), so the allowed kinds of write outside
//!   a gateway command are listed on [`WriteClass`] and greppable at every call. Authority
//!   records are separate: [`WriteTransaction::authority`] and [`WriteTransaction::grants`] only
//!   read, and their writes need the [`AuthorityCapability`] that only `crate::authority` can
//!   build, whatever the class.
//! * **No callbacks with effects inside a write.** Jobs must not await, call the network, run
//!   inference, invoke package/UI callbacks or traverse unboundedly; preparation and observation
//!   happen before or after the job (ARCH-03).
//! * **Cancellation and acknowledgment.** Cancellation (or an expired deadline) before admission
//!   prevents dispatch. After admission the owner either rolls back before commit or reports the
//!   commit; a caller that stops waiting never establishes rollback. An ambiguous commit is
//!   reported as [`StorageError::CommitUnknown`] and resolved by reading the durable record (for
//!   commands, the receipt).
//! * **Snapshots.** [`ApplicationStore::read`] runs a bounded closure against one database
//!   snapshot on a query-only connection; the snapshot ends when the closure returns, so no live
//!   snapshot survives think time. Each snapshot carries its [`SnapshotStamp`] freshness.
//!
//! Repositories owned by later items (work, knowledge, index/retrieval views and manual
//! backup/restore) extend this module with their own aggregate traits; this module holds the
//! operational subset every command already needs: receipts, history, the operation journal,
//! the durable queue/outbox (with ordered partitions and consumer progress), package event-handler
//! state and processing receipts, grants and actor/store identity, plus the scoped configuration
//! repository ([`ConfigurationRepository`]), the host authority ledger ([`AuthorityLedger`]), the
//! artifact store records ([`ArtifactRepository`]), the session, transcript and lineage
//! records ([`SessionRepository`]), the resource reservation and owned-process records
//! ([`ResourceRepository`]), the run root control records ([`RunRepository`]) and the retained
//! diagnostic log ([`DiagnosticRepository`]), the project and repository registration
//! records ([`ProjectRepository`]) and the repository indexing records ([`IndexRepository`]).
//! Owned browser sessions and observations use BrowserRepository.
use crate::authority::AuthorityCapability;
use crate::context::{PrincipalKind, ReceiptIdentity};
use crate::fingerprint::Fingerprint;
use crate::locks::LockExt;
use crate::ports::{GrantUse, IdKind, StoredReceipt};
use crate::protocol::{
    ActorId, AssignmentId, AttemptId, Counter, EventId, ExecutionId, Id, OperationId,
    OperationState, ProjectId, ReadStamp, Revision, RevisionDimension, RootId, StoreId,
};
use crate::registry::SchemaRevision;
use serde_json::Value;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};

mod agents;
mod artifacts;
mod authority;
mod browser;
mod configuration;
mod conventions;
mod decisions;
mod diagnostics;
mod embedding;
mod evals;
mod git;
mod github;
mod graph;
mod help;
mod indexing;
mod instructions;
mod jobs;
mod knowledge;
mod maintenance;
mod packages;
mod projects;
mod record_kinds;
mod resources;
mod router;
mod runs;
mod schemas;
mod sessions;
mod usage;
mod watchers;
mod work;
mod work_imports;
mod workspace_archive;
pub use agents::*;
pub use artifacts::*;
pub use authority::*;
pub use browser::*;
pub use configuration::*;
pub use conventions::*;
pub use decisions::*;
pub use diagnostics::*;
pub use embedding::*;
pub use evals::*;
pub use git::*;
pub use github::*;
pub use graph::*;
pub use help::*;
pub use indexing::*;
pub use instructions::*;
pub use jobs::*;
pub use knowledge::*;
pub use maintenance::*;
pub use packages::*;
pub use projects::*;
pub use record_kinds::ProjectRecordKind;
pub use resources::*;
pub use router::*;
pub use runs::*;
pub use schemas::*;
pub use sessions::*;
pub use usage::*;
pub use watchers::*;
pub use work::*;
pub use work_imports::*;
pub use workspace_archive::*;

// ---------------------------------------------------------------------------------------------
// Errors

/// A failed optimistic update: current identity and revision, with the whole command unchanged.
#[derive(Clone, Debug)]
pub struct RevisionConflict {
    pub dimension: RevisionDimension,
    pub target_kind: &'static str,
    pub target_id: Id,
    pub expected: Revision,
    pub current: Revision,
}

/// Why storage could not serve a request. Diagnostics are redacted adapter text, never secrets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    Io,
    /// The volume or the store's size bound is exhausted; previously committed data is preserved
    /// and no weaker durability is substituted.
    DiskFull,
    Corrupt,
    ReadOnly,
    /// Another process owns this data root.
    DataRootInUse,
    /// The write owner has stopped (shutdown or failure); nothing was dispatched.
    WriterStopped,
    /// The linked engine, its compile options or a bundled extension failed startup verification.
    UnsupportedRuntime,
    /// The job panicked; its transaction was rolled back.
    HandlerPanicked,
    Locked,
}

/// Shared typed storage error vocabulary. Services decide retry or escalation from these and the
/// known outcome, never from engine-specific numeric codes.
#[derive(Clone, Debug)]
pub enum StorageError {
    RevisionConflict(Box<RevisionConflict>),
    NotFound {
        kind: &'static str,
        id: String,
    },
    /// The current scope or authority no longer permits this access.
    ScopeDenied,
    /// Bounded admission is saturated; nothing was dispatched. Retry later.
    Busy,
    /// Cancelled before admission or rolled back before commit.
    Cancelled,
    ViewUnavailable,
    CursorExpired,
    /// Ownership or the fencing generation no longer matches; the caller lost its claim.
    LeaseLost {
        kind: &'static str,
        id: String,
    },
    /// A uniqueness/dedupe identity already exists.
    Duplicate {
        kind: &'static str,
        id: String,
    },
    /// The request violates a persistence invariant (illegal transition, bound, malformed field).
    InvalidRecord(String),
    /// The store's schema is unknown, newer than this build allows for the requested access, or
    /// its migration ledger does not match. Data is left unchanged.
    IncompatibleSchema(String),
    StorageUnavailable {
        cause: Unavailable,
        diagnostic: String,
    },
    /// The commit may have happened but its acknowledgment was lost; resolve by durable lookup.
    CommitUnknown,
}
impl StorageError {
    /// Diagnostics are adapter text: credential-shaped content is redacted before it can be
    /// logged or surfaced.
    pub fn unavailable(cause: Unavailable, diagnostic: impl Into<String>) -> Self {
        Self::StorageUnavailable {
            cause,
            diagnostic: crate::authority::redact::Redactor::new().redact_text(&diagnostic.into()),
        }
    }
    pub fn conflict(
        dimension: RevisionDimension,
        target_kind: &'static str,
        target_id: Id,
        expected: Revision,
        current: Revision,
    ) -> Self {
        Self::RevisionConflict(Box::new(RevisionConflict {
            dimension,
            target_kind,
            target_id,
            expected,
            current,
        }))
    }
}
impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RevisionConflict(c) => write!(
                f,
                "{} {} is at revision {}, expected {}",
                c.target_kind,
                c.target_id.as_str(),
                c.current.as_str(),
                c.expected.as_str()
            ),
            Self::NotFound { kind, id } => write!(f, "{kind} {id} not found"),
            Self::ScopeDenied => f.write_str("scope denied"),
            Self::Busy => f.write_str("storage busy"),
            Self::Cancelled => f.write_str("cancelled"),
            Self::ViewUnavailable => f.write_str("view unavailable"),
            Self::CursorExpired => f.write_str("cursor expired"),
            Self::LeaseLost { kind, id } => write!(f, "{kind} {id}: ownership lost"),
            Self::Duplicate { kind, id } => write!(f, "{kind} {id} already exists"),
            Self::InvalidRecord(e) => write!(f, "invalid record: {}", redacted(e)),
            Self::IncompatibleSchema(e) => write!(f, "incompatible schema: {}", redacted(e)),
            Self::StorageUnavailable { cause, diagnostic } => {
                write!(f, "storage unavailable ({cause:?}): {diagnostic}")
            }
            Self::CommitUnknown => f.write_str("commit acknowledgment lost"),
        }
    }
}
impl std::error::Error for StorageError {}
fn redacted(text: &str) -> String {
    crate::authority::redact::Redactor::new().redact_text(text)
}

// ---------------------------------------------------------------------------------------------
// Admission options and the transaction owner

/// Cooperative cancellation shared between a caller and the write/read owner. A waiter on a
/// [`Wake`] registers it, so cancelling ends the wait at once instead of at the next poll.
#[derive(Clone, Debug, Default)]
pub struct Cancellation(Arc<CancelState>);
#[derive(Debug, Default)]
struct CancelState {
    flag: AtomicBool,
    wakes: Mutex<Vec<Weak<Wake>>>,
}
impl Cancellation {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.flag.store(true, Ordering::SeqCst);
        let wakes: Vec<_> = {
            let mut wakes = self.0.wakes.locked();
            wakes.retain(|wake| wake.strong_count() > 0);
            wakes.iter().filter_map(Weak::upgrade).collect()
        };
        for wake in wakes {
            wake.notify();
        }
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.flag.load(Ordering::SeqCst)
    }
    /// Notifies `wake` when this token is cancelled. Registering after the cancellation has
    /// already happened leaves the caller to see [`Self::is_cancelled`] on its next check, so a
    /// waiter registers first and checks the flag second.
    pub fn wake_on_cancel(&self, wake: &Arc<Wake>) {
        let mut wakes = self.0.wakes.locked();
        wakes.retain(|other| other.strong_count() > 0);
        if !wakes
            .iter()
            .any(|other| std::ptr::eq(other.as_ptr(), Arc::as_ptr(wake)))
        {
            wakes.push(Arc::downgrade(wake));
        }
    }
}

/// A signal that something a waiter is waiting for may have changed. A waiter reads
/// [`Wake::generation`] before it checks its condition and then waits for a change after that
/// value ([`Wake::changed_after`], or [`Wake::wait_after`] for a caller that cannot await), so a
/// signal sent between the check and the wait is never lost. An awaiting waiter holds no thread:
/// the signal wakes its task. The signal depends on no async runtime.
#[derive(Debug, Default)]
pub struct Wake {
    state: Mutex<WakeState>,
}
#[derive(Debug, Default)]
struct WakeState {
    generation: u64,
    next_slot: u64,
    wakers: Vec<(u64, Waker)>,
}
impl Wake {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    /// Signals every waiter.
    pub fn notify(&self) {
        let wakers = {
            let mut state = self.state.locked();
            state.generation += 1;
            std::mem::take(&mut state.wakers)
        };
        for (_, waker) in wakers {
            waker.wake();
        }
    }
    /// The number of signals so far.
    pub fn generation(&self) -> u64 {
        self.state.locked().generation
    }
    /// Resolves once a signal arrives after `seen`. Dropping it ends the wait.
    pub fn changed_after(self: &Arc<Self>, seen: u64) -> Changed {
        Changed {
            wake: self.clone(),
            seen,
            slot: None,
        }
    }
    /// Parks the calling thread until a signal arrives after `seen` or `timeout` passes,
    /// whichever is first. For a caller that cannot await; an awaiting caller uses
    /// [`Wake::changed_after`] and holds no thread.
    pub fn wait_after(self: &Arc<Self>, seen: u64, timeout: std::time::Duration) {
        struct Unpark(std::thread::Thread);
        impl std::task::Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let deadline = std::time::Instant::now() + timeout;
        let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
        let mut cx = Context::from_waker(&waker);
        let mut changed = std::pin::pin!(self.changed_after(seen));
        loop {
            if changed.as_mut().poll(&mut cx).is_ready() {
                return;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return;
            }
            std::thread::park_timeout(deadline - now);
        }
    }
}

/// The future of [`Wake::changed_after`].
#[derive(Debug)]
pub struct Changed {
    wake: Arc<Wake>,
    seen: u64,
    slot: Option<u64>,
}
impl Future for Changed {
    type Output = ();
    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let wake = self.wake.clone();
        let mut state = wake.state.locked();
        if state.generation != self.seen {
            if let Some(slot) = self.slot.take() {
                state.wakers.retain(|(id, _)| *id != slot);
            }
            return Poll::Ready(());
        }
        let slot = match self.slot {
            Some(slot) => slot,
            None => {
                state.next_slot += 1;
                self.slot = Some(state.next_slot);
                state.next_slot
            }
        };
        match state.wakers.iter_mut().find(|(id, _)| *id == slot) {
            Some((_, waker)) => waker.clone_from(cx.waker()),
            None => state.wakers.push((slot, cx.waker().clone())),
        }
        Poll::Pending
    }
}
impl Drop for Changed {
    fn drop(&mut self) {
        if let Some(slot) = self.slot {
            self.wake
                .state
                .locked()
                .wakers
                .retain(|(id, _)| *id != slot);
        }
    }
}

/// A count of store commits that changed data, with listeners that run after each one. The write
/// owner bumps it once per committed unit of work that modified a row, whoever submitted that
/// work, so a background loop can wait on it instead of a timer. A unit that changed nothing
/// (an empty cleanup pass) does not bump it, which keeps a loop's own idle pass from waking it.
#[derive(Clone, Default)]
pub struct CommitSignal(Arc<CommitSignalState>);
type CommitListener = Box<dyn Fn(u64) + Send + Sync>;
#[derive(Default)]
struct CommitSignalState {
    generation: AtomicU64,
    listeners: Mutex<Vec<CommitListener>>,
}
impl CommitSignal {
    /// Records one data-changing commit and tells every listener the new generation. Runs on the
    /// write owner after the commit is durable; listeners must not block.
    pub fn bump(&self) {
        let generation = self
            .0
            .generation
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1);
        for listener in self.0.listeners.locked().iter() {
            listener(generation);
        }
    }
    /// Commits recorded so far.
    pub fn generation(&self) -> u64 {
        self.0.generation.load(Ordering::SeqCst)
    }
    /// Calls `listener` with the new generation after each later commit.
    pub fn subscribe(&self, listener: impl Fn(u64) + Send + Sync + 'static) {
        self.0.listeners.locked().push(Box::new(listener));
    }
}

/// Writer scheduling class. Authoritative work (controls, receipts, leases, history) takes
/// precedence; bulk batches keep a reserved progress share so they are never starved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum WritePriority {
    #[default]
    Authoritative,
    Bulk,
}

/// Why a write runs outside a use case's receipt. Every [`WriteOptions`] names exactly one class,
/// so each `run_write` and `submit_write` site states its class at the call (search for
/// `WriteOptions::` plus the constructor name). The store applies no different rules per class:
/// the class is a contract for reviewers and a hook for tests, and it never grants authority.
///
/// A class never excuses a write that changes authority records. Binding or revoking a
/// principal, or issuing, revoking or spending a host grant, needs the authority capability
/// ([`WriteTransaction::authority_writes`]) whichever class the write has, and the matching
/// history records (principal, grant, repository deletion) can only be appended through
/// [`WriteTransaction::authority_history`]: `history().append` refuses them in every class. So
/// the code outside `crate::authority` can neither change nor record authority.
///
/// Choosing a class. When two fit, the purpose decides. A write whose purpose is to repair or
/// expire stale state is [`Self::Recovery`] (after a restart or a lost owner) or
/// [`Self::Housekeeping`] (age or retention sweeps), whatever repository it touches.
/// [`Self::HostState`] is only for the live control ticks of a resource that is healthy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WriteClass {
    /// A user or host command executed by the gateway, which records its receipt and history in
    /// the same transaction. Constructor: [`WriteOptions::command`].
    Command,
    /// A write by the authority seams or the gateway's own bookkeeping of a refusal (denial
    /// history, the dedicated repository-deletion confirmation). Constructor:
    /// [`WriteOptions::authority`].
    Authority,
    /// A record of something the host observed or produced outside any command: diagnostics,
    /// transcript and context publication, observed artifacts, usage, notifications, watcher and
    /// package observations. Constructor: [`WriteOptions::observed`].
    HostObserved,
    /// A host-owned resource moving through its own lifecycle under its own fences and
    /// idempotency (queue claims and delivery, leases, staging, reservations, phase changes of
    /// watchers, packages, connections and indexes, run and job control ticks). Constructor:
    /// [`WriteOptions::host_state`].
    HostState,
    /// Recording the outcome of an external effect (process, container, git, MCP, tool call,
    /// approval resume) that already happened or was refused. Constructor:
    /// [`WriteOptions::settlement`].
    EffectSettlement,
    /// Boot or restart reconciliation of state a previous process left uncertain. Constructor:
    /// [`WriteOptions::recovery`].
    Recovery,
    /// Pruning, garbage collection and retention sweeps of derived or expired records.
    /// Constructor: [`WriteOptions::housekeeping`].
    Housekeeping,
    /// Seeding or inspecting a store in tests and in the contract suite. Production code does not
    /// use it. Constructor: [`WriteOptions::fixture`].
    Fixture,
}

#[derive(Clone, Debug)]
pub struct WriteOptions {
    /// Required: there is no default. Struct-update syntax copies it from a class constructor
    /// (`..WriteOptions::recovery()`).
    pub class: WriteClass,
    pub priority: WritePriority,
    pub cancellation: Option<Cancellation>,
    /// UTC milliseconds by the store clock. It bounds admission and commit only: past it, an
    /// undispatched job is not started and an admitted job is rolled back instead of committed.
    /// It does not bound how long the caller waits for the reply, and it never revokes a commit.
    pub deadline_ms: Option<i64>,
}
impl WriteOptions {
    /// Options of `class` with the default priority, no cancellation and no deadline. There is
    /// no `Default`: a write must name its class.
    pub fn new(class: WriteClass) -> Self {
        Self {
            class,
            priority: WritePriority::default(),
            cancellation: None,
            deadline_ms: None,
        }
    }
    pub fn command() -> Self {
        Self::new(WriteClass::Command)
    }
    pub fn authority() -> Self {
        Self::new(WriteClass::Authority)
    }
    pub fn observed() -> Self {
        Self::new(WriteClass::HostObserved)
    }
    pub fn host_state() -> Self {
        Self::new(WriteClass::HostState)
    }
    pub fn settlement() -> Self {
        Self::new(WriteClass::EffectSettlement)
    }
    pub fn recovery() -> Self {
        Self::new(WriteClass::Recovery)
    }
    pub fn housekeeping() -> Self {
        Self::new(WriteClass::Housekeeping)
    }
    pub fn fixture() -> Self {
        Self::new(WriteClass::Fixture)
    }
}
#[derive(Clone, Debug, Default)]
pub struct ReadOptions {
    pub cancellation: Option<Cancellation>,
    pub deadline_ms: Option<i64>,
}

/// Whether this store instance may write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccessMode {
    ReadWrite,
    /// Diagnostics/export only: every write is refused with [`StorageError::IncompatibleSchema`].
    ReadOnly {
        reason: String,
    },
}

/// Identity of the open store.
#[derive(Clone, Debug)]
pub struct StoreInfo {
    pub store_id: StoreId,
    pub restore_epoch: i64,
    /// This process's boot epoch record; absent in read-only mode.
    pub boot_id: Option<Id>,
    /// Host actor that attributes store-owned bookkeeping (queue/journal observations).
    pub host_actor: ActorId,
    pub access: AccessMode,
}

/// An owned unit of write work executed on the write owner.
pub trait WriteJob: Send {
    /// Runs inside the transaction. `Ok` requests commit; `Err` rolls the whole unit back.
    fn run(&mut self, tx: &mut dyn WriteTransaction) -> Result<(), StorageError>;
    /// Reports the final outcome once: `Ok` only after a confirmed commit.
    fn finish(self: Box<Self>, outcome: Result<(), StorageError>);
}

/// The store: one write owner and bounded read snapshots.
pub trait ApplicationStore: Send + Sync {
    fn info(&self) -> StoreInfo;
    /// The signal bumped after every commit that changed data, or `None` for a store that does
    /// not publish one (callers then rely on their own fallback interval).
    fn commit_signal(&self) -> Option<CommitSignal> {
        None
    }
    /// Admits `job` to the bounded mailbox for its priority. `Err` means it was not admitted
    /// (Busy, Cancelled, read-only, stopped) and `finish` will not be called.
    fn submit_write(
        &self,
        options: WriteOptions,
        job: Box<dyn WriteJob>,
    ) -> Result<(), StorageError>;
    /// Runs `work` against one coherent snapshot on a query-only connection of the calling thread.
    fn read(
        &self,
        options: &ReadOptions,
        work: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError>;
}

/// Builds a [`WriteJob`] from a closure and a responder that receives the committed value.
pub fn write_job<T, F, R>(work: F, respond: R) -> Box<dyn WriteJob>
where
    T: Send + 'static,
    F: FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
    R: FnOnce(Result<T, StorageError>) + Send + 'static,
{
    struct Job<T, F, R> {
        work: Option<F>,
        value: Option<T>,
        respond: R,
    }
    impl<T, F, R> WriteJob for Job<T, F, R>
    where
        T: Send,
        F: FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send,
        R: FnOnce(Result<T, StorageError>) + Send,
    {
        fn run(&mut self, tx: &mut dyn WriteTransaction) -> Result<(), StorageError> {
            let work = self
                .work
                .take()
                .ok_or_else(|| StorageError::InvalidRecord("write job ran twice".into()))?;
            self.value = Some(work(tx)?);
            Ok(())
        }
        fn finish(self: Box<Self>, outcome: Result<(), StorageError>) {
            let Job { value, respond, .. } = *self;
            respond(match (outcome, value) {
                (Ok(()), Some(v)) => Ok(v),
                (Ok(()), None) => Err(StorageError::CommitUnknown),
                (Err(e), _) => Err(e),
            });
        }
    }
    Box::new(Job {
        work: Some(work),
        value: None,
        respond,
    })
}

/// Submits `work` and blocks the calling (non-UI) thread until its outcome is known. A reply
/// that never arrives after admission is [`StorageError::CommitUnknown`], never a rollback.
pub fn run_write<T, F>(
    store: &dyn ApplicationStore,
    options: WriteOptions,
    work: F,
) -> Result<T, StorageError>
where
    T: Send + 'static,
    F: FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
{
    let (tx, rx) = mpsc::sync_channel(1);
    store.submit_write(
        options,
        write_job(work, move |r| {
            let _ = tx.send(r);
        }),
    )?;
    rx.recv().unwrap_or(Err(StorageError::CommitUnknown))
}

/// Runs `work` against one read snapshot and returns its value.
pub fn read_snapshot<T>(
    store: &dyn ApplicationStore,
    options: &ReadOptions,
    work: impl FnOnce(&dyn ReadSnapshot) -> Result<T, StorageError>,
) -> Result<T, StorageError> {
    let mut work = Some(work);
    let mut out = None;
    store.read(options, &mut |snapshot| {
        let f = work
            .take()
            .ok_or_else(|| StorageError::InvalidRecord("snapshot work ran twice".into()))?;
        out = Some(f(snapshot)?);
        Ok(())
    })?;
    out.ok_or(StorageError::CommitUnknown)
}

/// A savepoint inside the current write transaction. It is not a commit; only the job's outcome
/// commits. Not `Clone`, so it can be rolled back or released once, and its fields are private:
/// only the issuing transaction mints it and checks that it belongs to that transaction.
///
/// Semantics follow SQLite: rolling back to or releasing a savepoint also discards every savepoint
/// opened after it that is still open, so an outer owner can always unwind work an inner handler
/// left open.
#[derive(Debug, PartialEq, Eq)]
pub struct Savepoint {
    transaction: u64,
    depth: u32,
}
impl Savepoint {
    /// For [`WriteTransaction`] implementations: `transaction` is the implementation's private
    /// serial for the current unit of work and `depth` is 1 for the outermost savepoint.
    pub fn issue(transaction: u64, depth: u32) -> Self {
        Self { transaction, depth }
    }
    pub fn transaction(&self) -> u64 {
        self.transaction
    }
    pub fn depth(&self) -> u32 {
        self.depth
    }
}

/// The single write unit of work. All repositories borrow this transaction.
pub trait WriteTransaction {
    /// Transaction timestamp (UTC ms by the store clock), fixed when the transaction began.
    fn now_ms(&self) -> i64;
    /// Allocates an opaque identifier from the store's injected source.
    fn new_id(&mut self, kind: IdKind) -> Id;
    fn store(&self) -> &StoreInfo;
    /// Highest committed sequence visible inside this transaction (including its own appends).
    fn committed_sequence(&self) -> Result<i64, StorageError>;
    fn savepoint(&mut self) -> Result<Savepoint, StorageError>;
    fn rollback_to(&mut self, savepoint: Savepoint) -> Result<(), StorageError>;
    fn release(&mut self, savepoint: Savepoint) -> Result<(), StorageError>;

    fn actors(&mut self) -> &mut dyn ActorRegistry;
    fn receipts(&mut self) -> &mut dyn CommandLedger;
    fn history(&mut self) -> &mut dyn HistoryLog;
    fn operations(&mut self) -> &mut dyn OperationJournal;
    fn queue(&mut self) -> &mut dyn DurableQueue;
    /// Resume grants, read-only; their writes need [`Self::grant_writes`].
    fn grants(&mut self) -> &mut dyn GrantRead;
    fn packages(&mut self) -> &mut dyn PackageHandlerLedger;
    fn configuration(&mut self) -> &mut dyn ConfigurationRepository;
    /// Authority records, read-only. Writes are not reachable from here: a transaction cannot
    /// bind or revoke a principal, or issue, revoke or spend a host grant, without the
    /// [`AuthorityCapability`] that [`Self::authority_writes`] requires.
    fn authority(&mut self) -> &mut dyn AuthorityRead;
    /// The authority ledger with its writes, for the `crate::authority` seams, which alone can
    /// present the capability, on behalf of `actor`. On the store's own transaction (host code
    /// holding the store) this is the ledger. The gateway's handler view allows it only for the
    /// principal of a user- or host-ingress command, so a use case running for an agent, package,
    /// MCP client or schedule can never write authority, whatever context it constructs or wraps.
    fn authority_writes(
        &mut self,
        _: &AuthorityCapability,
        _actor: &ActorId,
    ) -> Result<&mut dyn AuthorityLedger, StorageError>;
    /// Resume-grant ledger writes for the `crate::authority` seams, under the same rule as
    /// [`Self::authority_writes`]; [`Self::grants`] is read-only.
    fn grant_writes(
        &mut self,
        _: &AuthorityCapability,
        _actor: &ActorId,
    ) -> Result<&mut dyn GrantLedger, StorageError>;
    /// History appends of the `crate::authority` seams (principal, grant and repository-deletion
    /// records), under the same rule as [`Self::authority_writes`]. This is the only history log
    /// that takes them: [`Self::history`] refuses authority-history records (see
    /// [`crate::authority::is_authority_history`]) on every implementation, so neither a raw
    /// write nor a use case can forge them. Implementations have no default for this method.
    fn authority_history(
        &mut self,
        _: &AuthorityCapability,
        _actor: &ActorId,
    ) -> Result<&mut dyn HistoryLog, StorageError>;
    /// The artifact repository. Inside a use case (the gateway's handler view) backup pins,
    /// physical-removal claims and receipt reply records are refused.
    fn artifacts(&mut self) -> &mut dyn ArtifactRepository;
    /// Narrow delegation from the artifact owner for transactionally admitted archive pins.
    fn archive_pin(
        &mut self,
        _authority: &crate::services::artifacts::ArchivePinAuthority,
        holder: &Id,
        digest: &str,
    ) -> Result<(), StorageError> {
        self.artifacts().pin(holder, digest)
    }
    fn archive_release_pins(
        &mut self,
        _authority: &crate::services::artifacts::ArchivePinAuthority,
        holder: &Id,
    ) -> Result<i64, StorageError> {
        self.artifacts().release_pins(holder)
    }

    fn sessions(&mut self) -> &mut dyn SessionRepository;
    fn resources(&mut self) -> &mut dyn ResourceRepository;
    fn runs(&mut self) -> &mut dyn RunRepository;
    fn diagnostics(&mut self) -> &mut dyn DiagnosticRepository;
    fn browser(&mut self) -> &mut dyn BrowserRepository;
    fn evals(&mut self) -> &mut dyn EvalRepository;
    fn watchers(&mut self) -> &mut dyn WatcherRepository;
    fn help(&mut self) -> &mut dyn HelpRepository;
    fn instructions(&mut self) -> &mut dyn InstructionRepository;
    fn github(&mut self) -> &mut dyn GithubRepository;
    fn conventions(&mut self) -> &mut dyn ConventionRepository;
    fn schemas(&mut self) -> &mut dyn SchemaRepository;
    fn decisions(&mut self) -> &mut dyn DecisionRepository;
    fn git(&mut self) -> &mut dyn GitRepository;
    fn router(&mut self) -> &mut dyn RouterRepository;
    fn projects(&mut self) -> &mut dyn ProjectRepository;
    fn indexing(&mut self) -> &mut dyn IndexRepository;
    fn graph(&mut self) -> &mut dyn GraphRepository;
    fn knowledge(&mut self) -> &mut dyn KnowledgeRepository;
    fn jobs(&mut self) -> &mut dyn JobRepository;
    fn work(&mut self) -> &mut dyn WorkRepository;
    fn retention(&mut self) -> Result<&mut dyn RetentionRepository, StorageError> {
        Err(StorageError::ViewUnavailable)
    }
    fn updates(&mut self) -> Result<&mut dyn super::update::UpdateRepository, StorageError> {
        Err(StorageError::ViewUnavailable)
    }
    fn workspace_archives(&mut self) -> Result<&mut dyn WorkspaceArchiveRepository, StorageError> {
        Err(StorageError::ViewUnavailable)
    }
    fn work_imports(&mut self) -> Result<&mut dyn WorkImportRepository, StorageError> {
        Err(StorageError::ViewUnavailable)
    }

    fn usage(&mut self) -> &mut dyn UsageRepository;
}

/// Freshness/provenance of one snapshot. Not an authorization token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotStamp {
    pub store_id: StoreId,
    pub restore_epoch: i64,
    /// Highest committed event sequence visible in this snapshot (0 for an empty history).
    pub committed_sequence: i64,
    /// The store clock (UTC ms) when the snapshot was taken; a lease is unexpired against it.
    pub now_ms: i64,
    /// This process's boot epoch; absent in read-only mode.
    pub boot_id: Option<Id>,
}
impl SnapshotStamp {
    /// The wire freshness stamp for this snapshot under host-resolved policy/scope revisions.
    pub fn read_stamp(&self, policy_revision: Revision, scope_revision: Revision) -> ReadStamp {
        ReadStamp {
            store_id: self.store_id.clone(),
            restore_epoch: Counter::new(self.restore_epoch.max(0)).expect("non-negative"),
            sequence: Counter::new(self.committed_sequence.max(0)).expect("non-negative"),
            policy_revision,
            scope_revision,
        }
    }
}

/// One coherent read snapshot. Rolled-back and later-committed changes are never visible.
pub trait ReadSnapshot {
    fn stamp(&self) -> &SnapshotStamp;
    fn actors(&self) -> &dyn ActorRead;
    fn receipts(&self) -> &dyn CommandLedgerRead;
    fn history(&self) -> &dyn HistoryRead;
    fn operations(&self) -> &dyn OperationRead;
    fn queue(&self) -> &dyn QueueRead;
    fn grants(&self) -> &dyn GrantRead;
    fn packages(&self) -> &dyn PackageHandlerRead;
    fn configuration(&self) -> &dyn ConfigurationRead;
    fn authority(&self) -> &dyn AuthorityRead;
    fn artifacts(&self) -> &dyn ArtifactRead;
    fn sessions(&self) -> &dyn SessionRead;
    fn resources(&self) -> &dyn ResourceRead;
    fn runs(&self) -> &dyn RunRead;
    fn diagnostics(&self) -> &dyn DiagnosticRead;
    fn browser(&self) -> &dyn BrowserRead;
    fn evals(&self) -> &dyn EvalRead;
    fn watchers(&self) -> &dyn WatcherRead;
    fn help(&self) -> &dyn HelpRead;
    fn instructions(&self) -> &dyn InstructionRead;
    fn github(&self) -> &dyn GithubRead;
    fn conventions(&self) -> &dyn ConventionRead;
    fn schemas(&self) -> &dyn SchemaRead;
    fn decisions(&self) -> &dyn DecisionRead;
    fn git(&self) -> &dyn GitRead;
    fn router(&self) -> &dyn RouterRead;
    fn projects(&self) -> &dyn ProjectRead;
    fn indexing(&self) -> &dyn IndexRead;
    fn graph(&self) -> &dyn GraphRead;
    fn knowledge(&self) -> &dyn KnowledgeRead;
    fn jobs(&self) -> &dyn JobRead;
    fn work(&self) -> &dyn WorkRead;
    fn retention(&self) -> Result<&dyn RetentionRead, StorageError> {
        Err(StorageError::ViewUnavailable)
    }
    fn updates(&self) -> Result<&dyn super::update::UpdateRead, StorageError> {
        Err(StorageError::ViewUnavailable)
    }
    fn workspace_archives(&self) -> Result<&dyn WorkspaceArchiveRead, StorageError> {
        Err(StorageError::ViewUnavailable)
    }
    fn work_imports(&self) -> Result<&dyn WorkImportRead, StorageError> {
        Err(StorageError::ViewUnavailable)
    }

    fn usage(&self) -> &dyn UsageRead;
}

// ---------------------------------------------------------------------------------------------
// Actors and store identity

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredActor {
    pub kind: PrincipalKind,
    pub display_name: String,
}

pub trait ActorRead {
    fn actor_kind(&self, actor: &ActorId) -> Result<Option<PrincipalKind>, StorageError>;
    fn actor(&self, actor: &ActorId) -> Result<Option<StoredActor>, StorageError>;
}
pub trait ActorRegistry: ActorRead {
    /// Records a host-resolved principal for attribution. Idempotent for the same kind; a
    /// different kind for an existing actor is [`StorageError::Duplicate`].
    fn register(
        &mut self,
        actor: &ActorId,
        kind: PrincipalKind,
        display_name: &str,
    ) -> Result<(), StorageError>;
}

// ---------------------------------------------------------------------------------------------
// Command receipts

/// A receipt row as found, decoded only when its contract and binding are current.
#[derive(Clone, Debug)]
pub enum LedgerEntry {
    Current(StoredReceipt),
    Purged(crate::ports::StoredPurgedReceipt),
    /// Written under a different contract revision or no longer decodable: a visible
    /// incompatible-record hold. It is never replayed or treated as a different intent.
    Incompatible {
        operation: String,
        contract: String,
        schema_revision: SchemaRevision,
        fingerprint: Option<Fingerprint>,
        reason: String,
    },
}
/// The published durable typed reply of one receipt: its immutable reply artifact version and
/// the schema reference the reply bytes conform to, recorded once after the receipt commits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiptReply {
    pub command_id: crate::protocol::CommandId,
    pub artifact_id: crate::protocol::Id,
    pub version_id: crate::protocol::Id,
    /// The `SchemaRef` (wire form) fixed at publication.
    pub schema: Value,
    pub published_at_ms: i64,
}
pub trait CommandLedgerRead {
    fn lookup(&self, identity: &ReceiptIdentity) -> Result<Option<LedgerEntry>, StorageError>;
    /// The published reply of the receipt at `identity`, if it was published.
    fn reply(&self, identity: &ReceiptIdentity) -> Result<Option<ReceiptReply>, StorageError>;
    /// Whether the receipt at `identity` can be `command`'s, revealing nothing else of it: a
    /// current receipt of that command, or an incompatible one (never disowned, since its command
    /// cannot be read). `false` when there is no receipt or it is another command's.
    fn may_be_receipt_of(
        &self,
        identity: &ReceiptIdentity,
        command: &crate::protocol::CommandId,
    ) -> Result<bool, StorageError> {
        Ok(match self.lookup(identity)? {
            None => false,
            Some(LedgerEntry::Incompatible { .. }) => true,
            Some(LedgerEntry::Current(stored)) => stored.receipt.command_id == *command,
            Some(LedgerEntry::Purged(stored)) => stored.outcome.command_id == *command,
        })
    }
}
pub trait CommandLedger: CommandLedgerRead {
    /// Inserts a decided receipt. Identity `(actor, scopeKey, requestKey)` is unique. Only the
    /// gateway records receipts: the transaction a use case receives refuses this call.
    fn record(
        &mut self,
        identity: &ReceiptIdentity,
        receipt: &StoredReceipt,
    ) -> Result<(), StorageError>;
    /// Records the published reply of an existing receipt, once: a second reply for the same
    /// receipt, command or artifact is [`StorageError::Duplicate`]. Only the receipt reply
    /// publisher records it: the transaction a use case receives refuses this call.
    fn record_reply(
        &mut self,
        identity: &ReceiptIdentity,
        reply: &ReceiptReply,
    ) -> Result<(), StorageError>;
}

// ---------------------------------------------------------------------------------------------
// History

/// An append-only history event. Its committed sequence is assigned by the store.
#[derive(Clone, Debug)]
pub struct NewEvent {
    pub id: EventId,
    pub actor: ActorId,
    pub project_id: Option<ProjectId>,
    pub root_id: Option<RootId>,
    pub operation_id: Option<OperationId>,
    pub aggregate_kind: String,
    pub aggregate_id: String,
    /// Positive revision of the aggregate after this change.
    pub aggregate_revision: i64,
    pub kind: String,
    /// Versioned JSON object payload.
    pub payload: Value,
}
#[derive(Clone, Debug, PartialEq)]
pub struct EventRecord {
    pub sequence: i64,
    pub id: EventId,
    pub actor: ActorId,
    pub project_id: Option<ProjectId>,
    pub root_id: Option<RootId>,
    pub operation_id: Option<OperationId>,
    pub aggregate_kind: String,
    pub aggregate_id: String,
    pub aggregate_revision: i64,
    pub kind: String,
    pub created_at_ms: i64,
    pub payload: Value,
}
#[derive(Clone, Debug)]
pub enum EventFilter {
    All,
    Aggregate { kind: String, id: String },
    Root(RootId),
}
/// One page in committed-sequence order. `watermark` is the snapshot's committed sequence;
/// `next_after` resumes after the last returned event (also at the tail, so polling never
/// replays). Gaps are normal: other aggregates and filters skip sequence numbers.
#[derive(Clone, Debug)]
pub struct EventPage {
    pub events: Vec<EventRecord>,
    pub next_after: i64,
    pub has_more: bool,
    pub watermark: i64,
}
/// Page size bound for history and queue listings.
pub const MAX_PAGE: u16 = 500;
pub trait HistoryRead {
    fn event(&self, id: &EventId) -> Result<Option<EventRecord>, StorageError>;
    /// Events with sequence greater than `after`, at most `limit` (1..=500).
    fn page(&self, filter: &EventFilter, after: i64, limit: u16)
    -> Result<EventPage, StorageError>;
}
pub trait HistoryLog: HistoryRead {
    /// Appends `event` and returns its committed sequence (global, strictly increasing in commit
    /// order because there is one writer).
    fn append(&mut self, event: NewEvent) -> Result<i64, StorageError>;
}

// ---------------------------------------------------------------------------------------------
// Operation journal

#[derive(Clone, Debug)]
pub struct NewOperation {
    pub id: OperationId,
    pub root_id: Option<RootId>,
    pub assignment_id: Option<AssignmentId>,
    pub capability: String,
    pub schema_version: i64,
    pub args_digest: String,
    pub payload: Value,
}
/// Owner/policy binding recorded on an attempt; later transitions must present the same fence.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttemptBinding {
    pub execution_id: Option<ExecutionId>,
    pub owner_generation: Option<i64>,
    pub control_revision: Option<i64>,
    pub policy_revision: Option<i64>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptFence {
    pub attempt_no: i64,
    pub owner_generation: Option<i64>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct AttemptRecord {
    pub id: AttemptId,
    pub attempt_no: i64,
    pub boot_id: Id,
    pub binding: AttemptBinding,
    pub state: OperationState,
    /// Attributed observations in arrival order, including those from superseded owners.
    pub observations: Vec<Value>,
}
impl AttemptRecord {
    pub fn fence(&self) -> AttemptFence {
        AttemptFence {
            attempt_no: self.attempt_no,
            owner_generation: self.binding.owner_generation,
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct OperationRecord {
    pub id: OperationId,
    pub root_id: Option<RootId>,
    pub assignment_id: Option<AssignmentId>,
    pub capability: String,
    pub schema_version: i64,
    pub args_digest: String,
    pub payload: Value,
    pub state: OperationState,
    pub revision: i64,
    pub created_at_ms: i64,
    pub attempts: Vec<AttemptRecord>,
}
impl OperationRecord {
    pub fn latest_attempt(&self) -> Option<&AttemptRecord> {
        self.attempts.last()
    }
}

/// Whether the lifecycle allows `from -> to` through [`OperationJournal::transition`].
/// `IntentRecorded`/`RetryPending` -> `Preflighted` happens only through `begin_attempt`.
/// An `OutcomeUnknown` operation can never start another attempt directly: a reconciler must
/// first establish `ObservedComplete`, `Failed`, `RetryPending` (safe retry) or `NeedsInput`.
pub fn transition_allowed(from: OperationState, to: OperationState) -> bool {
    use OperationState::*;
    matches!(
        (from, to),
        (
            IntentRecorded | RetryPending,
            CancelledBeforeDispatch | Failed
        ) | (
            Preflighted,
            Dispatching | CancelledBeforeDispatch | RetryPending | Failed
        ) | (
            Dispatching,
            ObservedComplete | Failed | OutcomeUnknown | NeedsInput | RetryPending
        ) | (
            OutcomeUnknown,
            ObservedComplete | Failed | RetryPending | NeedsInput
        ) | (NeedsInput, OutcomeUnknown | Failed)
    )
}
pub fn is_terminal(state: OperationState) -> bool {
    use OperationState::*;
    matches!(state, ObservedComplete | Failed | CancelledBeforeDispatch)
}
/// States in which an effect may already have happened and no automatic replay is allowed.
pub fn possibly_dispatched(state: OperationState) -> bool {
    use OperationState::*;
    matches!(state, Dispatching | OutcomeUnknown | NeedsInput)
}
/// States in which the effect has certainly not started, so the operation may still be cancelled
/// before dispatch.
pub fn cancellable_before_dispatch(state: OperationState) -> bool {
    use OperationState::*;
    matches!(state, IntentRecorded | RetryPending | Preflighted)
}
/// The one rule for whether a requested cancellation applies to `entry`: it has no operation,
/// or its operation has certainly not dispatched (or was already cancelled before dispatch, an
/// idempotent no-op). Otherwise the effect may have run or has an outcome, and it is reconciled
/// instead. The store, the dispatcher and the package commit path all decide with this.
pub fn cancellation_applies(
    operations: &dyn OperationRead,
    entry: &QueueEntry,
) -> Result<bool, StorageError> {
    let Some(op) = &entry.operation_id else {
        return Ok(true);
    };
    Ok(operations.operation(op)?.is_some_and(|o| {
        cancellable_before_dispatch(o.state) || o.state == OperationState::CancelledBeforeDispatch
    }))
}

pub trait OperationRead {
    fn operation(&self, id: &OperationId) -> Result<Option<OperationRecord>, StorageError>;
    /// Non-terminal operations in stable ID order, at most `limit` (1..=500).
    fn unresolved(&self, limit: u16) -> Result<Vec<OperationRecord>, StorageError>;
    /// Trusted recovery fact over the actual journal. Exact capability and current states are
    /// filtered before the 1..=500 keyset page. Empty states match nothing. This read grants
    /// no invocation authority; owners still recheck scope, lineage and effects before acting.
    fn operations_for_capability(
        &self,
        capability: &str,
        states: &[OperationState],
        after: Option<&OperationId>,
        limit: u16,
    ) -> Result<Vec<OperationRecord>, StorageError>;
}
pub trait OperationJournal: OperationRead {
    /// Records durable intent (`IntentRecorded`, revision 1) before any effect.
    fn record_intent(&mut self, operation: NewOperation) -> Result<OperationRecord, StorageError>;
    /// Starts the next attempt (`Preflighted`) from `IntentRecorded` or `RetryPending` under
    /// the current boot, recording its owner/policy binding.
    fn begin_attempt(
        &mut self,
        id: &OperationId,
        expected_revision: i64,
        binding: AttemptBinding,
    ) -> Result<AttemptRecord, StorageError>;
    /// Conditional lifecycle transition. From `Preflighted`/`Dispatching` the caller must
    /// present the latest attempt's fence; a stale owner gets [`StorageError::LeaseLost`].
    fn transition(
        &mut self,
        id: &OperationId,
        expected_revision: i64,
        fence: Option<&AttemptFence>,
        to: OperationState,
        observation: Option<Value>,
    ) -> Result<OperationRecord, StorageError>;
    /// Appends an attributed observation to an attempt without changing current state; a
    /// superseded owner's observation is retained as evidence.
    fn observe(
        &mut self,
        id: &OperationId,
        attempt_no: i64,
        observation: Value,
    ) -> Result<(), StorageError>;
    /// Classifies operations interrupted in earlier boots: an undispatched `Preflighted` attempt
    /// becomes `RetryPending` (reconsider under current policy); a `Dispatching` attempt becomes
    /// `OutcomeUnknown` (observe before any retry). Returns the reclassified operations.
    fn classify_interrupted(
        &mut self,
        limit: u16,
    ) -> Result<Vec<(OperationId, OperationState)>, StorageError>;
}

// ---------------------------------------------------------------------------------------------
// Durable queue / outbox

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum QueueState {
    Ready,
    Claimed,
    Completed,
    /// Terminal and visible (exhausted retries or a deterministic failure).
    Failed,
    Cancelled,
    /// Held: the linked operation may have dispatched; it needs reconciliation before redelivery.
    Reconciling,
    /// Terminal: an operator acknowledged a `Failed` (poison) delivery. Its cause stays visible in
    /// history; the partition may advance past it because a person decided so.
    Dismissed,
}
impl QueueState {
    /// Settled states no longer block their ordered partition or the consumer watermark.
    pub fn is_settled(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled | Self::Dismissed)
    }
}
#[derive(Clone, Debug)]
pub struct NewQueueEntry {
    pub id: Id,
    pub consumer: String,
    /// The history event this delivery is for; it must be committed in the same transaction or
    /// earlier.
    pub event_id: EventId,
    pub root_id: Option<RootId>,
    pub operation_id: Option<OperationId>,
    /// Ordered partition (for example an aggregate or root). Deliveries of one consumer in the
    /// same partition are delivered strictly in event-sequence order and a held or failed head
    /// blocks only that partition. `None` is unordered: independent of every other delivery.
    pub partition: Option<String>,
    pub priority: i64,
    pub ready_at_ms: i64,
    pub payload: Value,
}
#[derive(Clone, Debug, PartialEq)]
pub struct QueueEntry {
    pub id: Id,
    pub consumer: String,
    pub event_id: EventId,
    pub root_id: Option<RootId>,
    pub operation_id: Option<OperationId>,
    pub priority: i64,
    pub ready_at_ms: i64,
    pub state: QueueState,
    pub owner: Option<Id>,
    pub lease_until_ms: Option<i64>,
    /// Fencing generation, incremented on every claim and recovery.
    pub generation: i64,
    pub cancellation_epoch: i64,
    pub attempt_count: i64,
    pub payload: Value,
    pub partition: Option<String>,
    /// Committed sequence of the delivered event (ordering key within a partition).
    pub event_sequence: i64,
    /// Positive revision, incremented on every state, lease, retry or resolution change.
    pub revision: i64,
    /// Most recent failure cause; it stays visible through retries until the delivery completes.
    pub last_error: Option<String>,
    /// Boot epoch of the current claim; a claim is only current within the boot that made it.
    pub claim_boot_id: Option<Id>,
    /// A cancellation was requested while the delivery was claimed or held behind an operation
    /// whose outcome was not established. It is durable: a later hold, retry, requeue or claim
    /// recovery cancels the delivery instead of making it ready again, unless its operation has
    /// an established outcome that must still be recorded.
    pub cancel_requested: bool,
    /// When the delivery last failed or was held (store clock, UTC ms); `None` before that.
    pub failed_at_ms: Option<i64>,
    /// Root-scoped work recovered from an earlier boot: held until root eligibility is confirmed
    /// (by the root-control layer or an explicit operator retry); never re-claimed automatically.
    pub awaiting_eligibility: bool,
}
impl QueueEntry {
    /// This entry's keyset position in the inspection order (`None` once settled).
    pub fn inspection_key(&self) -> Option<DeliveryKey> {
        let (rank, a, b) = match self.state {
            QueueState::Claimed => (0, self.event_sequence, 0),
            QueueState::Failed | QueueState::Reconciling => (1, self.failed_at_ms.unwrap_or(0), 0),
            QueueState::Ready => (2, self.priority, self.ready_at_ms),
            _ => return None,
        };
        Some(DeliveryKey {
            rank,
            a,
            b,
            id: self.id.clone(),
        })
    }
    pub fn fence(&self) -> Option<QueueFence> {
        self.owner.clone().map(|owner| QueueFence {
            owner,
            generation: self.generation,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueFence {
    pub owner: Id,
    pub generation: i64,
}
#[derive(Clone, Debug)]
pub struct ClaimRequest {
    pub consumer: String,
    pub owner: Id,
    pub lease_ms: i64,
    /// 1..=500 entries.
    pub limit: u16,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settlement {
    Completed,
    /// The owner observed a cancellation request and stopped.
    Cancelled,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureDisposition {
    /// Transient: ready again after `delay_ms`, unless `max_attempts` claims were used.
    Retry { delay_ms: i64, max_attempts: i64 },
    /// Deterministic/poison failure: visible terminal `Failed`.
    Terminal,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimRecovery {
    /// Claims whose lease expired by the transaction clock.
    Expired,
    /// Claims made under an earlier boot epoch. Their owners belonged to a dead process; claims
    /// of the current boot are never touched, whatever their lease.
    PriorBoots,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryOutcome {
    pub requeued: Vec<Id>,
    /// Held in `Reconciling`: possibly dispatched, or root-scoped work awaiting eligibility.
    pub held: Vec<Id>,
    /// Settled `Cancelled` because a cancellation was requested while they were claimed.
    pub cancelled: Vec<Id>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueueStatus {
    pub ready: i64,
    pub claimed: i64,
    pub reconciling: i64,
    pub failed: i64,
    pub completed: i64,
    pub cancelled: i64,
    pub dismissed: i64,
    pub oldest_ready_at_ms: Option<i64>,
}
/// Which unsettled deliveries an inspection lists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeliveryFilter {
    pub consumer: Option<String>,
    pub operation_id: Option<OperationId>,
}
/// Keyset position in the inspection order: `rank` 0 = claimed (oldest event first), 1 =
/// failed or held (most recent failure first), 2 = ready in drain order (priority descending,
/// ready time, id). `a`/`b` are the rank's sort keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryKey {
    pub rank: u8,
    pub a: i64,
    pub b: i64,
    pub id: Id,
}
#[derive(Clone, Debug)]
pub struct DeliveryPage {
    pub entries: Vec<QueueEntry>,
    /// Present when more unsettled deliveries follow.
    pub next: Option<DeliveryKey>,
}
/// Durable delivery progress of one consumer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConsumerProgress {
    /// Contiguous applied prefix: every delivery of this consumer whose event sequence is at or
    /// below it is settled. It never passes an unsettled (ready, claimed, held, failed) delivery.
    pub watermark: i64,
    /// Highest settled event sequence (recorded at settlement; survives compaction).
    pub highest_settled: i64,
    /// Incremented on every settlement of this consumer (0 before the first).
    pub revision: i64,
    /// Earliest ready time of a scheduled retry (a ready delivery that already had an attempt).
    pub next_retry_at_ms: Option<i64>,
}

pub trait QueueRead {
    fn entry(&self, id: &Id) -> Result<Option<QueueEntry>, StorageError>;
    /// The delivery of `event` to `consumer` (unique per pair), unless it was compacted.
    fn delivery(&self, consumer: &str, event: &EventId)
    -> Result<Option<QueueEntry>, StorageError>;
    /// Oldest unsettled delivery in the ordered partition.
    fn partition_head(
        &self,
        _consumer: &str,
        _partition: &str,
    ) -> Result<Option<QueueEntry>, StorageError> {
        Ok(None)
    }
    fn status(&self, consumer: &str) -> Result<QueueStatus, StorageError>;
    /// Unsettled deliveries (claimed, failed/held, ready) in inspection order after `after`, at
    /// most `limit` (1..=500). Settled deliveries are history, not queue contents.
    fn deliveries(
        &self,
        filter: &DeliveryFilter,
        after: Option<&DeliveryKey>,
        limit: u16,
    ) -> Result<DeliveryPage, StorageError>;
    fn progress(&self, consumer: &str) -> Result<ConsumerProgress, StorageError>;
    /// Applied checkpoint of one ordered partition (0 before its first settlement), or the
    /// consumer watermark for `None`.
    fn checkpoint(&self, consumer: &str, partition: Option<&str>) -> Result<i64, StorageError>;
    /// Earliest saved time after `now_ms` at which a delivery becomes ready or a claim's lease
    /// expires: the durable timer a dispatcher sleeps until (bounded by its sweep interval).
    fn next_due_ms(&self, now_ms: i64) -> Result<Option<i64>, StorageError>;
    /// Held deliveries of `consumer` awaiting root eligibility, in id order after `after`, at
    /// most `limit` (1..=500).
    fn awaiting_eligibility(
        &self,
        consumer: &str,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<QueueEntry>, StorageError>;
}
/// Durable at-least-once delivery primitives. Delivery deduplication is per `(consumer, event)`;
/// it is not exactly-once execution of external effects.
pub trait DurableQueue: QueueRead {
    fn enqueue(&mut self, entry: NewQueueEntry) -> Result<QueueEntry, StorageError>;
    /// Claims a bounded ready batch in priority / ready-time / stable-ID order with conditional
    /// state+generation updates, recording a claim observation per entry. Only the oldest
    /// unsettled delivery of an ordered partition is eligible, and a delivery whose operation may
    /// already have dispatched (`Dispatching`/`OutcomeUnknown`/`NeedsInput`) is not: an
    /// uncertain operation pauses its dependents while unrelated deliveries continue.
    fn claim_ready(&mut self, request: &ClaimRequest) -> Result<Vec<QueueEntry>, StorageError>;
    fn renew(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        lease_ms: i64,
    ) -> Result<QueueEntry, StorageError>;
    /// Settles the owner's claim. `Cancelled` also cancels the delivery's undispatched operation
    /// (`IntentRecorded`, `RetryPending`, `Preflighted`) and is refused once the operation may
    /// have dispatched or has an outcome: a dispatched effect is reconciled, never cancelled.
    fn settle(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        settlement: Settlement,
    ) -> Result<QueueEntry, StorageError>;
    /// Records the cause durably and either schedules a retry or fails terminally. An entry whose
    /// operation may already have dispatched is always held in `Reconciling`, whatever its
    /// attempt budget, so a possibly dispatched effect is never reported as poison. A retry of an
    /// entry whose cancellation was requested settles it `Cancelled` instead.
    fn fail(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        cause: &str,
        disposition: FailureDisposition,
    ) -> Result<QueueEntry, StorageError>;
    /// Cancels a ready/held entry (with its undispatched operation), or raises the cancellation
    /// epoch of a claimed one and records the request durably so its owner stops cooperatively
    /// and settles it as cancelled. A held entry whose operation may have dispatched only records
    /// the request: what happened must be reconciled first. An entry whose operation already has
    /// an outcome (`ObservedComplete`, `Failed`) is refused; one already cancelled before
    /// dispatch is settled `Cancelled` without touching the operation ([`cancellation_applies`]).
    fn cancel(&mut self, id: &Id) -> Result<QueueEntry, StorageError>;
    /// Returns stale claims to `Ready` (new generation) when redelivery is safe, or holds them in
    /// `Reconciling` when their operation may already have dispatched. A claim whose cancellation
    /// was requested is settled `Cancelled` instead of becoming ready. Root-scoped claims of an
    /// earlier boot are held awaiting root eligibility rather than continued automatically.
    /// Claims are bound to the boot epoch that made them; renew/settle/fail also require the
    /// current boot.
    fn recover_claims(
        &mut self,
        mode: ClaimRecovery,
        limit: u16,
    ) -> Result<RecoveryOutcome, StorageError>;
    /// Makes a `Reconciling` or `Failed` entry ready again after explicit resolution (new
    /// generation), confirming root eligibility. Refused while its operation is still possibly
    /// dispatched. From `Failed` the retry budget restarts (`attempt_count` = 0; earlier failures
    /// stay in history); from `Reconciling` the attempt count is kept. An entry whose
    /// cancellation was requested is settled `Cancelled` instead, unless its operation has an
    /// established outcome that redelivery must record.
    fn requeue(&mut self, id: &Id, delay_ms: i64) -> Result<QueueEntry, StorageError>;
    /// Compacts a settled (`Completed`/`Cancelled`/`Dismissed`) delivery: its bookkeeping (payload, lease) is dropped
    /// and it is no longer readable, but a tombstone keeps its `(consumer, event)` dedupe identity
    /// so the same event can never be delivered to that consumer again. History is untouched.
    /// The owner holds its claimed entry in `Reconciling` because its effect outcome is unknown
    /// or its payload cannot be interpreted (unknown schema/upcaster failure), with the cause.
    fn hold(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        cause: &str,
    ) -> Result<QueueEntry, StorageError>;
    /// The owner holds its claimed root-scoped entry in `Reconciling` with the awaiting-eligibility
    /// marker because its root does not currently permit dispatch (paused, held or stopped): it
    /// is requeued once the root-control layer confirms eligibility again, exactly as root work
    /// recovered from an earlier boot. Only entries scoped to a root can be held this way.
    fn hold_for_eligibility(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        cause: &str,
    ) -> Result<QueueEntry, StorageError>;
    /// Acknowledges a `Failed` delivery as `Dismissed` (an operator decision with its reason).
    /// Refused while its operation is possibly dispatched.
    fn dismiss(&mut self, id: &Id, reason: &str) -> Result<QueueEntry, StorageError>;
    /// Sleep/wake repair: the still-running owner re-establishes its lease on a claim whose lease
    /// time passed while the machine slept, provided nobody recovered it (same owner and
    /// generation). It never transfers a claim.
    fn reassert(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        lease_ms: i64,
    ) -> Result<QueueEntry, StorageError>;
    fn compact(&mut self, id: &Id) -> Result<(), StorageError>;
}

// ---------------------------------------------------------------------------------------------
// Package event handler state and processing receipts

/// A package's durable handler state under one activation generation.
#[derive(Clone, Debug, PartialEq)]
pub struct PackageState {
    pub package_id: String,
    pub generation: i64,
    pub revision: i64,
    pub state: Value,
}
/// The host processing receipt of one `(package, subscription, event)` commit: its payload
/// digest and the exact accepted reply, so an identical replay returns the original identities.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessingRecord {
    pub package_id: String,
    pub subscription_id: Id,
    pub event_id: EventId,
    pub receipt_id: Id,
    pub payload_digest: String,
    pub state_generation: i64,
    pub state_revision: i64,
    pub checkpoint: i64,
    /// `[{"actionKey", "operationId"}]` in submission order.
    pub intents: Value,
}
/// Handler state and processing receipts plus the package registry ([`PackageRegistryRead`]).
pub trait PackageHandlerRead: PackageRegistryRead {
    fn package_state(&self, package_id: &str) -> Result<Option<PackageState>, StorageError>;
    fn processing(
        &self,
        package_id: &str,
        subscription_id: &Id,
        event_id: &EventId,
    ) -> Result<Option<ProcessingRecord>, StorageError>;
}
pub trait PackageHandlerLedger: PackageHandlerRead + PackageRegistry {
    /// Compare-and-set of the package state. `expected_revision` 0 creates it at revision 1. A
    /// different stored generation is [`StorageError::LeaseLost`]; a different revision is a
    /// [`StorageError::RevisionConflict`].
    fn compare_and_set_state(
        &mut self,
        package_id: &str,
        generation: i64,
        expected_revision: i64,
        state: Value,
    ) -> Result<PackageState, StorageError>;
    /// Inserts the processing receipt; the `(package, subscription, event)` identity and the
    /// receipt ID are unique ([`StorageError::Duplicate`]).
    fn record_processing(&mut self, record: &ProcessingRecord) -> Result<(), StorageError>;
}

// ---------------------------------------------------------------------------------------------
// Resume/authority grants

#[derive(Clone, Debug)]
pub struct NewGrant {
    pub id: Id,
    pub actor: ActorId,
    pub root_id: RootId,
    pub authority_source: String,
    pub binding: Value,
    pub expires_at_ms: Option<i64>,
    pub uses: Option<i64>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct GrantRecord {
    pub id: Id,
    pub actor: ActorId,
    pub root_id: RootId,
    pub revision: i64,
    pub active: bool,
    /// Revoked by the user or host (an inactive grant that was not merely spent or expired).
    pub revoked: bool,
    pub authority_source: String,
    pub binding: Value,
    pub expires_at_ms: Option<i64>,
    pub uses_remaining: Option<i64>,
}
pub trait GrantRead {
    fn grant(&self, id: &Id) -> Result<Option<GrantRecord>, StorageError>;
}
pub trait GrantLedger: GrantRead {
    /// Records a host-minted grant (revision 1).
    fn issue(&mut self, grant: NewGrant) -> Result<GrantRecord, StorageError>;
    /// Consumes one use for `actor` at the revision authority resolved. Inactive, expired,
    /// exhausted or foreign grants are [`StorageError::ScopeDenied`]; a changed revision is a
    /// [`StorageError::RevisionConflict`].
    fn consume(&mut self, grant: &GrantUse, actor: &ActorId) -> Result<GrantRecord, StorageError>;
    /// Revokes an active grant at `expected_revision` (advancing it). A changed revision is a
    /// [`StorageError::RevisionConflict`]; an unknown grant is [`StorageError::NotFound`].
    fn revoke(&mut self, id: &Id, expected_revision: i64) -> Result<GrantRecord, StorageError>;
}

// ---------------------------------------------------------------------------------------------
// Maintenance seam

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedMigration {
    pub version: i64,
    pub checksum: String,
    pub applied_at_ms: i64,
    pub app_build: String,
}
#[derive(Clone, Debug)]
pub struct CompatibilityReport {
    pub build_schema_version: i64,
    pub store_schema_version: i64,
    pub minimum_reader: i64,
    pub minimum_writer: i64,
    pub access: AccessMode,
    pub migrations: Vec<AppliedMigration>,
}
/// Linked engine and bundled extension identities verified at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeIdentity {
    pub engine: String,
    pub engine_version: String,
    pub compile_options: Vec<String>,
    pub extensions: Vec<(String, String)>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrityReport {
    pub problems: Vec<String>,
}
impl IntegrityReport {
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }
}
/// Adapter-owned lifecycle operations invoked through dedicated application services. Manual
/// backup, restore and export (STORE-05/07/13) extend this seam in their own item; there is no
/// automatic backup and no generic SQL escape hatch.
pub trait StorageMaintenance {
    fn compatibility(&self) -> Result<CompatibilityReport, StorageError>;
    fn runtime_identity(&self) -> RuntimeIdentity;
    fn integrity_check(&self) -> Result<IntegrityReport, StorageError>;
}

#[cfg(feature = "contract-suite")]
pub mod contract;

#[cfg(test)]
mod wake_tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn poll_once(future: &mut Changed) -> Poll<()> {
        let mut cx = Context::from_waker(Waker::noop());
        std::pin::Pin::new(future).poll(&mut cx)
    }

    #[test]
    fn a_signal_after_the_count_is_read_is_not_lost() {
        let wake = Wake::new();
        let seen = wake.generation();
        wake.notify();
        assert!(poll_once(&mut wake.changed_after(seen)).is_ready());
    }

    #[test]
    fn a_waiting_future_is_woken_once_and_dropping_it_removes_its_waker() {
        struct Count(std::sync::atomic::AtomicUsize);
        impl std::task::Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let wake = Wake::new();
        let count = Arc::new(Count(Default::default()));
        let waker = Waker::from(count.clone());
        let mut cx = Context::from_waker(&waker);
        let mut changed = wake.changed_after(wake.generation());
        assert!(std::pin::Pin::new(&mut changed).poll(&mut cx).is_pending());
        // Polling again replaces its waker rather than adding one.
        assert!(std::pin::Pin::new(&mut changed).poll(&mut cx).is_pending());
        wake.notify();
        assert_eq!(count.0.load(Ordering::SeqCst), 1);
        assert!(std::pin::Pin::new(&mut changed).poll(&mut cx).is_ready());
        let mut dropped = wake.changed_after(wake.generation());
        assert!(poll_once(&mut dropped).is_pending());
        drop(dropped);
        assert!(wake.state.locked().wakers.is_empty());
    }

    #[test]
    fn a_blocking_wait_ends_on_a_signal_or_its_timeout() {
        let wake = Wake::new();
        let started = Instant::now();
        wake.wait_after(wake.generation(), Duration::from_millis(50));
        assert!(started.elapsed() >= Duration::from_millis(50));
        let seen = wake.generation();
        let signal = wake.clone();
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            signal.notify();
        });
        let started = Instant::now();
        wake.wait_after(seen, Duration::from_secs(30));
        assert!(started.elapsed() < Duration::from_secs(10));
        sender.join().unwrap();
    }

    #[test]
    fn cancelling_wakes_a_registered_wait() {
        let wake = Wake::new();
        let token = Cancellation::new();
        token.wake_on_cancel(&wake);
        let seen = wake.generation();
        token.cancel();
        assert!(poll_once(&mut wake.changed_after(seen)).is_ready());
        assert!(token.is_cancelled());
    }
}
