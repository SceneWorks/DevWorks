//! Deterministic port substitutes for gateway tests. They only store and count; every admission,
//! replay and conflict decision under test is made by the production gateway. The in-memory
//! store serializes writes under one lock (a sole write owner) and keeps receipts, history,
//! actors and the queue intents units of work enqueue; the journal, queue delivery and grant
//! repositories are exercised against real SQLite in `devworks-adapters`.
#![allow(dead_code, clippy::result_large_err)]

use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind, ReceiptIdentity};
use devworks_core::gateway::{AdmittedCommand, AdmittedQuery, Gateway};
use devworks_core::ports::storage::*;
use devworks_core::ports::*;
use devworks_core::protocol::results::OperationResult;
use devworks_core::protocol::*;
use devworks_core::registry::{EffectClass, SchemaRevision};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub fn id(s: &str) -> Id {
    Id::new(s).expect("valid fixture id")
}
pub fn revision(n: i64) -> Revision {
    Revision::new(n).expect("positive")
}

pub fn context(actor: &str) -> InvocationContext {
    context_in(actor, "ui-main", 0)
}
pub fn context_in(actor: &str, namespace: &str, epoch: i64) -> InvocationContext {
    HostIngress::new(&MemoryStore::new(&[])).context(
        ActorId(id(actor)),
        PrincipalKind::Human,
        id("invocation-1"),
        id(namespace),
        Counter::new(epoch).expect("epoch"),
        revision(1),
    )
}

/// Thread-safe counter with the `.get()` reading the tests use.
#[derive(Default)]
pub struct Count(AtomicUsize);
impl Count {
    pub fn get(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
    pub fn bump(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
/// Thread-safe settable value.
#[derive(Default)]
pub struct Slot<T>(Mutex<T>);
impl<T: Clone> Slot<T> {
    pub fn get(&self) -> T {
        self.0.lock().unwrap().clone()
    }
    pub fn set(&self, v: T) {
        *self.0.lock().unwrap() = v;
    }
}

pub use devworks_test_support::{FixedClock, TestIds as SeqIds};

#[derive(Default)]
pub struct Authority {
    pub deny: Slot<Option<AuthorityDenial>>,
    pub calls: Count,
    pub last_effect: Slot<Option<(EffectClass, Option<&'static str>)>>,
}
impl AuthorityPort for Authority {
    fn authorize(
        &self,
        _: &InvocationContext,
        request: &AuthorityRequest<'_>,
    ) -> Result<Authorization, AuthorityDenial> {
        self.calls.bump();
        self.last_effect.set(Some((request.effect, request.action)));
        self.deny.get().map_or(Ok(Authorization::direct()), Err)
    }
}

#[derive(Clone, Default)]
struct MemState {
    actors: HashMap<String, PrincipalKind>,
    receipts: HashMap<ReceiptIdentity, LedgerEntry>,
    events: Vec<EventRecord>,
    queue: MemQueue,
}

/// Enqueued intents only (delivery itself is exercised against real SQLite).
#[derive(Clone, Default)]
struct MemQueue(Vec<NewQueueEntry>);

/// In-memory [`ApplicationStore`] substitute with failure injection.
pub struct MemoryStore {
    state: Mutex<MemState>,
    info: StoreInfo,
    ids: SeqIds,
    pub submits: Count,
    pub reads: Count,
    pub recorded: Count,
    /// The next submission is not admitted with this error.
    pub refuse_next: Slot<Option<StorageError>>,
}
impl MemoryStore {
    pub fn new(actors: &[&str]) -> Self {
        let mut state = MemState::default();
        for a in actors {
            state.actors.insert((*a).to_owned(), PrincipalKind::Human);
        }
        Self {
            state: Mutex::new(state),
            info: StoreInfo {
                store_id: StoreId(id("store-a")),
                restore_epoch: 0,
                boot_id: Some(id("boot-1")),
                host_actor: ActorId(id("host")),
                access: AccessMode::ReadWrite,
            },
            ids: SeqIds::default(),
            submits: Count::default(),
            reads: Count::default(),
            recorded: Count::default(),
            refuse_next: Slot::default(),
        }
    }
    pub fn touched(&self) -> usize {
        self.submits.get() + self.reads.get()
    }
    pub fn receipt_count(&self) -> usize {
        self.state.lock().unwrap().receipts.len()
    }
    /// Consumers of the committed queue intents, in commit order.
    pub fn intents(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .queue
            .0
            .iter()
            .map(|e| e.consumer.clone())
            .collect()
    }
    pub fn event_kinds(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .events
            .iter()
            .map(|e| e.kind.clone())
            .collect()
    }
    /// Rewrites every stored receipt as if written under another contract revision.
    pub fn age_receipts(&self, contract: &str) {
        for entry in self.state.lock().unwrap().receipts.values_mut() {
            if let LedgerEntry::Current(r) = entry {
                *entry = LedgerEntry::Incompatible {
                    operation: r.operation.clone(),
                    contract: contract.to_owned(),
                    schema_revision: r.schema_revision,
                    fingerprint: Some(r.fingerprint.clone()),
                    reason: "older contract".into(),
                };
            }
        }
    }
    /// Rewrites every stored receipt's binding schema revision.
    pub fn rebind_receipts(&self, schema_revision: u32) {
        for entry in self.state.lock().unwrap().receipts.values_mut() {
            if let LedgerEntry::Current(r) = entry {
                r.schema_revision = SchemaRevision(schema_revision);
            }
        }
    }
}

struct MemTx<'a> {
    state: MemState,
    saved: Vec<MemState>,
    info: &'a StoreInfo,
    ids: &'a SeqIds,
    recorded: &'a Count,
    unsupported: Unsupported,
}
impl ApplicationStore for MemoryStore {
    fn info(&self) -> StoreInfo {
        self.info.clone()
    }
    fn submit_write(
        &self,
        _: WriteOptions,
        mut job: Box<dyn WriteJob>,
    ) -> Result<(), StorageError> {
        self.submits.bump();
        if let Some(e) = self.refuse_next.get() {
            self.refuse_next.set(None);
            return Err(e);
        }
        // Holding the lock for the whole unit serializes writes like the sole write owner.
        let mut guard = self.state.lock().unwrap();
        let mut tx = MemTx {
            state: guard.clone(),
            saved: Vec::new(),
            info: &self.info,
            ids: &self.ids,
            recorded: &self.recorded,
            unsupported: Unsupported,
        };
        match job.run(&mut tx) {
            Ok(()) => {
                *guard = tx.state;
                drop(guard);
                job.finish(Ok(()));
            }
            Err(e) => {
                drop(guard);
                job.finish(Err(e));
            }
        }
        Ok(())
    }
    fn read(
        &self,
        _: &ReadOptions,
        work: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.reads.bump();
        let state = self.state.lock().unwrap().clone();
        let snapshot = MemSnapshot {
            stamp: SnapshotStamp {
                store_id: self.info.store_id.clone(),
                restore_epoch: self.info.restore_epoch,
                committed_sequence: state.events.len() as i64,
                now_ms: FixedClock.now_ms().get(),
                boot_id: self.info.boot_id.clone(),
            },
            state,
            unsupported: Unsupported,
        };
        work(&snapshot)
    }
}

struct MemSnapshot {
    stamp: SnapshotStamp,
    state: MemState,
    unsupported: Unsupported,
}

fn mem_page(events: &[EventRecord], filter: &EventFilter, after: i64, limit: u16) -> EventPage {
    let matching: Vec<EventRecord> = events
        .iter()
        .filter(|e| e.sequence > after)
        .filter(|e| match filter {
            EventFilter::All => true,
            EventFilter::Aggregate { kind, id } => {
                &e.aggregate_kind == kind && &e.aggregate_id == id
            }
            EventFilter::Root(r) => e.root_id.as_ref() == Some(r),
        })
        .cloned()
        .collect();
    let watermark = events.len() as i64;
    let has_more = matching.len() > limit as usize;
    let events: Vec<_> = matching.into_iter().take(limit as usize).collect();
    EventPage {
        next_after: if has_more {
            events.last().map_or(after, |e| e.sequence)
        } else {
            watermark
        },
        events,
        has_more,
        watermark,
    }
}

macro_rules! mem_reads {
    ($t:ty) => {
        impl ActorRead for $t {
            fn actor_kind(&self, a: &ActorId) -> Result<Option<PrincipalKind>, StorageError> {
                Ok(self.state.actors.get(a.0.as_str()).copied())
            }
            fn actor(&self, a: &ActorId) -> Result<Option<StoredActor>, StorageError> {
                Ok(self.state.actors.get(a.0.as_str()).map(|kind| StoredActor {
                    kind: *kind,
                    display_name: a.0.as_str().to_owned(),
                }))
            }
        }
        impl CommandLedgerRead for $t {
            fn lookup(&self, i: &ReceiptIdentity) -> Result<Option<LedgerEntry>, StorageError> {
                Ok(self.state.receipts.get(i).cloned())
            }
            fn reply(&self, _: &ReceiptIdentity) -> Result<Option<ReceiptReply>, StorageError> {
                Ok(None)
            }
        }
        impl HistoryRead for $t {
            fn event(&self, id: &EventId) -> Result<Option<EventRecord>, StorageError> {
                Ok(self.state.events.iter().find(|e| &e.id == id).cloned())
            }
            fn page(
                &self,
                f: &EventFilter,
                after: i64,
                limit: u16,
            ) -> Result<EventPage, StorageError> {
                Ok(mem_page(&self.state.events, f, after, limit))
            }
        }
    };
}
mem_reads!(MemTx<'_>);
mem_reads!(MemSnapshot);

impl ReadSnapshot for MemSnapshot {
    fn stamp(&self) -> &SnapshotStamp {
        &self.stamp
    }
    fn actors(&self) -> &dyn ActorRead {
        self
    }
    fn receipts(&self) -> &dyn CommandLedgerRead {
        self
    }
    fn history(&self) -> &dyn HistoryRead {
        self
    }
    fn operations(&self) -> &dyn OperationRead {
        &self.unsupported
    }
    fn queue(&self) -> &dyn QueueRead {
        &self.unsupported
    }
    fn grants(&self) -> &dyn GrantRead {
        &self.unsupported
    }
    fn packages(&self) -> &dyn PackageHandlerRead {
        &self.unsupported
    }
    fn configuration(&self) -> &dyn ConfigurationRead {
        &self.unsupported
    }
    fn authority(&self) -> &dyn AuthorityRead {
        &self.unsupported
    }
    fn artifacts(&self) -> &dyn ArtifactRead {
        &self.unsupported
    }
    fn sessions(&self) -> &dyn SessionRead {
        &self.unsupported
    }
    fn resources(&self) -> &dyn ResourceRead {
        &self.unsupported
    }
    fn runs(&self) -> &dyn RunRead {
        &self.unsupported
    }
    fn diagnostics(&self) -> &dyn DiagnosticRead {
        &self.unsupported
    }
    fn git(&self) -> &dyn GitRead {
        &self.unsupported
    }
    fn browser(&self) -> &dyn BrowserRead {
        &self.unsupported
    }
    fn usage(&self) -> &dyn UsageRead {
        &self.unsupported
    }
    fn decisions(&self) -> &dyn DecisionRead {
        &self.unsupported
    }
    fn evals(&self) -> &dyn EvalRead {
        &self.unsupported
    }
    fn watchers(&self) -> &dyn WatcherRead {
        &self.unsupported
    }
    fn conventions(&self) -> &dyn ConventionRead {
        &self.unsupported
    }
    fn schemas(&self) -> &dyn SchemaRead {
        &self.unsupported
    }
    fn documents(&self) -> &dyn DocumentRead {
        &self.unsupported
    }
    fn help(&self) -> &dyn HelpRead {
        &self.unsupported
    }
    fn instructions(&self) -> &dyn InstructionRead {
        &self.unsupported
    }
    fn github(&self) -> &dyn GithubRead {
        &self.unsupported
    }
    fn router(&self) -> &dyn RouterRead {
        &self.unsupported
    }
    fn projects(&self) -> &dyn ProjectRead {
        &self.unsupported
    }
    fn indexing(&self) -> &dyn IndexRead {
        &self.unsupported
    }
    fn graph(&self) -> &dyn GraphRead {
        &self.unsupported
    }
    fn knowledge(&self) -> &dyn KnowledgeRead {
        &self.unsupported
    }
    fn jobs(&self) -> &dyn JobRead {
        &self.unsupported
    }
    fn work(&self) -> &dyn WorkRead {
        &self.unsupported
    }
}

impl WriteTransaction for MemTx<'_> {
    fn now_ms(&self) -> i64 {
        FixedClock.now_ms().get()
    }
    fn new_id(&mut self, kind: IdKind) -> Id {
        self.ids.next_id(kind)
    }
    fn store(&self) -> &StoreInfo {
        self.info
    }
    fn committed_sequence(&self) -> Result<i64, StorageError> {
        Ok(self.state.events.len() as i64)
    }
    fn savepoint(&mut self) -> Result<Savepoint, StorageError> {
        self.saved.push(self.state.clone());
        Ok(Savepoint::issue(MEM_TX, self.saved.len() as u32))
    }
    // SQLite semantics: unwinding an outer savepoint discards every inner one still open.
    fn rollback_to(&mut self, sp: Savepoint) -> Result<(), StorageError> {
        let depth = self.open(&sp)?;
        self.state = self.saved[depth - 1].clone();
        self.saved.truncate(depth - 1);
        Ok(())
    }
    fn release(&mut self, sp: Savepoint) -> Result<(), StorageError> {
        let depth = self.open(&sp)?;
        self.saved.truncate(depth - 1);
        Ok(())
    }
    fn actors(&mut self) -> &mut dyn ActorRegistry {
        self
    }
    fn receipts(&mut self) -> &mut dyn CommandLedger {
        self
    }
    fn history(&mut self) -> &mut dyn HistoryLog {
        self
    }
    fn operations(&mut self) -> &mut dyn OperationJournal {
        &mut self.unsupported
    }
    fn queue(&mut self) -> &mut dyn DurableQueue {
        &mut self.state.queue
    }
    fn grants(&mut self) -> &mut dyn GrantRead {
        &mut self.unsupported
    }
    fn grant_writes(
        &mut self,
        _: &devworks_core::authority::AuthorityCapability,
        _: &ActorId,
    ) -> Result<&mut dyn GrantLedger, StorageError> {
        Ok(&mut self.unsupported)
    }
    fn packages(&mut self) -> &mut dyn PackageHandlerLedger {
        &mut self.unsupported
    }
    fn configuration(&mut self) -> &mut dyn ConfigurationRepository {
        &mut self.unsupported
    }
    fn authority(&mut self) -> &mut dyn AuthorityRead {
        &mut self.unsupported
    }
    fn authority_writes(
        &mut self,
        _: &devworks_core::authority::AuthorityCapability,
        _: &ActorId,
    ) -> Result<&mut dyn AuthorityLedger, StorageError> {
        Ok(&mut self.unsupported)
    }
    fn authority_history(
        &mut self,
        _: &devworks_core::authority::AuthorityCapability,
        _: &ActorId,
    ) -> Result<&mut dyn HistoryLog, StorageError> {
        Ok(self)
    }
    fn artifacts(&mut self) -> &mut dyn ArtifactRepository {
        &mut self.unsupported
    }
    fn sessions(&mut self) -> &mut dyn SessionRepository {
        &mut self.unsupported
    }
    fn resources(&mut self) -> &mut dyn ResourceRepository {
        &mut self.unsupported
    }
    fn runs(&mut self) -> &mut dyn RunRepository {
        &mut self.unsupported
    }
    fn diagnostics(&mut self) -> &mut dyn DiagnosticRepository {
        &mut self.unsupported
    }
    fn git(&mut self) -> &mut dyn GitRepository {
        &mut self.unsupported
    }
    fn browser(&mut self) -> &mut dyn BrowserRepository {
        &mut self.unsupported
    }
    fn usage(&mut self) -> &mut dyn UsageRepository {
        &mut self.unsupported
    }
    fn decisions(&mut self) -> &mut dyn DecisionRepository {
        &mut self.unsupported
    }
    fn evals(&mut self) -> &mut dyn EvalRepository {
        &mut self.unsupported
    }
    fn watchers(&mut self) -> &mut dyn WatcherRepository {
        &mut self.unsupported
    }
    fn conventions(&mut self) -> &mut dyn ConventionRepository {
        &mut self.unsupported
    }
    fn schemas(&mut self) -> &mut dyn SchemaRepository {
        &mut self.unsupported
    }
    fn documents(&mut self) -> &mut dyn DocumentRepository {
        &mut self.unsupported
    }
    fn help(&mut self) -> &mut dyn HelpRepository {
        &mut self.unsupported
    }
    fn instructions(&mut self) -> &mut dyn InstructionRepository {
        &mut self.unsupported
    }
    fn github(&mut self) -> &mut dyn GithubRepository {
        &mut self.unsupported
    }
    fn router(&mut self) -> &mut dyn RouterRepository {
        &mut self.unsupported
    }
    fn projects(&mut self) -> &mut dyn ProjectRepository {
        &mut self.unsupported
    }
    fn indexing(&mut self) -> &mut dyn IndexRepository {
        &mut self.unsupported
    }
    fn graph(&mut self) -> &mut dyn GraphRepository {
        &mut self.unsupported
    }
    fn knowledge(&mut self) -> &mut dyn KnowledgeRepository {
        &mut self.unsupported
    }
    fn jobs(&mut self) -> &mut dyn JobRepository {
        &mut self.unsupported
    }
    fn work(&mut self) -> &mut dyn WorkRepository {
        &mut self.unsupported
    }
}
const MEM_TX: u64 = 7;
impl MemTx<'_> {
    fn open(&self, sp: &Savepoint) -> Result<usize, StorageError> {
        let depth = sp.depth() as usize;
        if sp.transaction() != MEM_TX || depth == 0 || depth > self.saved.len() {
            return Err(StorageError::InvalidRecord(
                "savepoint is not open in this transaction".into(),
            ));
        }
        Ok(depth)
    }
}
impl ActorRegistry for MemTx<'_> {
    fn register(&mut self, a: &ActorId, k: PrincipalKind, _: &str) -> Result<(), StorageError> {
        self.state.actors.insert(a.0.as_str().to_owned(), k);
        Ok(())
    }
}
impl CommandLedger for MemTx<'_> {
    fn record(&mut self, i: &ReceiptIdentity, r: &StoredReceipt) -> Result<(), StorageError> {
        self.recorded.bump();
        if self.state.receipts.contains_key(i) {
            return Err(StorageError::Duplicate {
                kind: "command_receipt",
                id: i.request_key().0.as_str().to_owned(),
            });
        }
        self.state
            .receipts
            .insert(i.clone(), LedgerEntry::Current(r.clone()));
        Ok(())
    }
    fn record_reply(&mut self, _: &ReceiptIdentity, _: &ReceiptReply) -> Result<(), StorageError> {
        nope()
    }
}
impl QueueRead for MemQueue {
    fn entry(&self, _: &Id) -> Result<Option<QueueEntry>, StorageError> {
        nope()
    }
    fn delivery(&self, _: &str, _: &EventId) -> Result<Option<QueueEntry>, StorageError> {
        nope()
    }
    fn status(&self, _: &str) -> Result<QueueStatus, StorageError> {
        nope()
    }
    fn deliveries(
        &self,
        _: &DeliveryFilter,
        _: Option<&DeliveryKey>,
        _: u16,
    ) -> Result<DeliveryPage, StorageError> {
        nope()
    }
    fn progress(&self, _: &str) -> Result<ConsumerProgress, StorageError> {
        nope()
    }
    fn checkpoint(&self, _: &str, _: Option<&str>) -> Result<i64, StorageError> {
        nope()
    }
    fn next_due_ms(&self, _: i64) -> Result<Option<i64>, StorageError> {
        nope()
    }
    fn awaiting_eligibility(
        &self,
        _: &str,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<QueueEntry>, StorageError> {
        nope()
    }
}
impl DurableQueue for MemQueue {
    fn hold_for_eligibility(
        &mut self,
        _: &Id,
        _: &QueueFence,
        _: &str,
    ) -> Result<QueueEntry, StorageError> {
        nope()
    }
    fn enqueue(&mut self, e: NewQueueEntry) -> Result<QueueEntry, StorageError> {
        self.0.push(e.clone());
        Ok(QueueEntry {
            id: e.id,
            consumer: e.consumer,
            event_id: e.event_id,
            root_id: e.root_id,
            operation_id: e.operation_id,
            priority: e.priority,
            ready_at_ms: e.ready_at_ms,
            state: QueueState::Ready,
            owner: None,
            lease_until_ms: None,
            generation: 0,
            cancellation_epoch: 0,
            attempt_count: 0,
            payload: e.payload,
            partition: e.partition,
            event_sequence: 0,
            revision: 1,
            last_error: None,
            claim_boot_id: None,
            cancel_requested: false,
            failed_at_ms: None,
            awaiting_eligibility: false,
        })
    }
    fn claim_ready(&mut self, _: &ClaimRequest) -> Result<Vec<QueueEntry>, StorageError> {
        nope()
    }
    fn renew(&mut self, _: &Id, _: &QueueFence, _: i64) -> Result<QueueEntry, StorageError> {
        nope()
    }
    fn settle(
        &mut self,
        _: &Id,
        _: &QueueFence,
        _: Settlement,
    ) -> Result<QueueEntry, StorageError> {
        nope()
    }
    fn fail(
        &mut self,
        _: &Id,
        _: &QueueFence,
        _: &str,
        _: FailureDisposition,
    ) -> Result<QueueEntry, StorageError> {
        nope()
    }
    fn cancel(&mut self, _: &Id) -> Result<QueueEntry, StorageError> {
        nope()
    }
    fn recover_claims(
        &mut self,
        _: ClaimRecovery,
        _: u16,
    ) -> Result<RecoveryOutcome, StorageError> {
        nope()
    }
    fn requeue(&mut self, _: &Id, _: i64) -> Result<QueueEntry, StorageError> {
        nope()
    }
    fn compact(&mut self, _: &Id) -> Result<(), StorageError> {
        nope()
    }
    fn hold(&mut self, _: &Id, _: &QueueFence, _: &str) -> Result<QueueEntry, StorageError> {
        nope()
    }
    fn dismiss(&mut self, _: &Id, _: &str) -> Result<QueueEntry, StorageError> {
        nope()
    }
    fn reassert(&mut self, _: &Id, _: &QueueFence, _: i64) -> Result<QueueEntry, StorageError> {
        nope()
    }
}
impl HistoryLog for MemTx<'_> {
    fn append(&mut self, e: NewEvent) -> Result<i64, StorageError> {
        let sequence = self.state.events.len() as i64 + 1;
        self.state.events.push(EventRecord {
            sequence,
            id: e.id,
            actor: e.actor,
            project_id: e.project_id,
            root_id: e.root_id,
            operation_id: e.operation_id,
            aggregate_kind: e.aggregate_kind,
            aggregate_id: e.aggregate_id,
            aggregate_revision: e.aggregate_revision,
            kind: e.kind,
            created_at_ms: FixedClock.now_ms().get(),
            payload: e.payload,
        });
        Ok(sequence)
    }
}

/// Repositories the in-memory substitute does not model (generated in `devworks-test-support`).
use devworks_test_support::{LoseAcks, Unmodelled as Unsupported, unmodelled as nope};

/// Scripted command use case: returns its configured decision and counts calls.
type DecideFn = dyn Fn(&AdmittedCommand) -> Result<Decided, HandlerFailure> + Send + Sync;
type AnswerFn = dyn Fn(&AdmittedQuery) -> Result<QueryAnswer, DomainFault> + Send + Sync;

pub struct Commands {
    pub decide: Box<DecideFn>,
    pub calls: Count,
}
impl Commands {
    pub fn new(
        f: impl Fn(&AdmittedCommand) -> Result<Decided, HandlerFailure> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            decide: Box::new(f),
            calls: Count::default(),
        })
    }
}
impl CommandHandler for Commands {
    fn decide(
        &self,
        _: &InvocationContext,
        command: &AdmittedCommand,
        _: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        self.calls.bump();
        (self.decide)(command)
    }
}

pub struct Queries {
    pub answer: Box<AnswerFn>,
    pub calls: Count,
}
impl Queries {
    pub fn new(
        f: impl Fn(&AdmittedQuery) -> Result<QueryAnswer, DomainFault> + Send + Sync + 'static,
    ) -> Self {
        Self {
            answer: Box::new(f),
            calls: Count::default(),
        }
    }
}
impl QueryHandler for Queries {
    fn query(
        &self,
        _: &InvocationContext,
        query: &AdmittedQuery,
        _: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        self.calls.bump();
        (self.answer)(query)
    }
}

pub fn fault(code: DomainErrorCode) -> DomainFault {
    serde_json::from_value(serde_json::json!({
        "code": code_name(code), "retry": "AfterBlockerClears", "correlationId": "corr-1", "blockerIds": ["hold-a"]
    }))
    .expect("fault")
}
fn code_name(code: DomainErrorCode) -> String {
    serde_json::to_value(code)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}

/// A read stamp matching the in-memory store at an empty history.
pub fn stamp() -> ReadStamp {
    serde_json::from_value(serde_json::json!({
        "storeId": "store-a", "restoreEpoch": "0", "sequence": "0", "policyRevision": "1", "scopeRevision": "1"
    }))
    .expect("stamp")
}

pub fn result(operation: &str, value: serde_json::Value) -> OperationResult {
    serde_json::from_value(serde_json::json!({"kind": operation, "value": value})).expect("result")
}

pub struct Harness {
    pub authority: Authority,
    pub store: Arc<MemoryStore>,
    /// The store behind the gateway: `acks.lose_nth_commit(1)` loses the next commit's acknowledgment.
    pub acks: Arc<LoseAcks>,
    pub ids: SeqIds,
}
impl Harness {
    pub fn new() -> Self {
        let store = Arc::new(MemoryStore::new(&["user-a", "user-b", "agent-a"]));
        Self {
            authority: Authority::default(),
            acks: LoseAcks::over(store.clone()),
            store,
            ids: SeqIds::default(),
        }
    }
    pub fn gateway(&self) -> Gateway<'_> {
        Gateway {
            authority: &self.authority,
            store: &*self.acks,
            ids: &self.ids,
        }
    }
}

pub fn contracts_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/architecture/contracts")
}
pub fn repo_file(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
}
