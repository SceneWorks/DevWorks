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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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

pub struct FixedClock;
impl Clock for FixedClock {
    fn now_ms(&self) -> Counter {
        Counter::new(1_700_000_000_000).expect("time")
    }
}

#[derive(Default)]
pub struct SeqIds(AtomicU64);
impl IdSource for SeqIds {
    fn next_id(&self, kind: IdKind) -> Id {
        let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        id(&format!("{kind:?}-{n}").to_lowercase())
    }
}

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
    /// The next successful commit reports a lost acknowledgment.
    pub lose_next_ack: AtomicBool,
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
            lose_next_ack: AtomicBool::new(false),
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
                if self.lose_next_ack.swap(false, Ordering::SeqCst) {
                    job.finish(Err(StorageError::CommitUnknown));
                } else {
                    job.finish(Ok(()));
                }
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
    fn grants(&mut self) -> &mut dyn GrantLedger {
        &mut self.unsupported
    }
    fn packages(&mut self) -> &mut dyn PackageHandlerLedger {
        &mut self.unsupported
    }
    fn configuration(&mut self) -> &mut dyn ConfigurationRepository {
        &mut self.unsupported
    }
    fn authority(&mut self) -> &mut dyn AuthorityLedger {
        &mut self.unsupported
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

/// Repositories the in-memory substitute does not model.
struct Unsupported;
fn nope<T>() -> Result<T, StorageError> {
    Err(StorageError::unavailable(
        Unavailable::Io,
        "not modelled by the in-memory substitute",
    ))
}
impl OperationRead for Unsupported {
    // No journal in memory: the gateway finds no operation to name as a decision's subject.
    fn operation(&self, _: &OperationId) -> Result<Option<OperationRecord>, StorageError> {
        Ok(None)
    }
    fn unresolved(&self, _: u16) -> Result<Vec<OperationRecord>, StorageError> {
        nope()
    }
    fn operations_for_capability(
        &self,
        _: &str,
        _: &[OperationState],
        _: Option<&OperationId>,
        _: u16,
    ) -> Result<Vec<OperationRecord>, StorageError> {
        nope()
    }
}
impl OperationJournal for Unsupported {
    fn record_intent(&mut self, _: NewOperation) -> Result<OperationRecord, StorageError> {
        nope()
    }
    fn begin_attempt(
        &mut self,
        _: &OperationId,
        _: i64,
        _: AttemptBinding,
    ) -> Result<AttemptRecord, StorageError> {
        nope()
    }
    fn transition(
        &mut self,
        _: &OperationId,
        _: i64,
        _: Option<&AttemptFence>,
        _: OperationState,
        _: Option<serde_json::Value>,
    ) -> Result<OperationRecord, StorageError> {
        nope()
    }
    fn observe(
        &mut self,
        _: &OperationId,
        _: i64,
        _: serde_json::Value,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn classify_interrupted(
        &mut self,
        _: u16,
    ) -> Result<Vec<(OperationId, OperationState)>, StorageError> {
        nope()
    }
}
impl QueueRead for Unsupported {
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
impl PackageHandlerRead for Unsupported {
    fn package_state(&self, _: &str) -> Result<Option<PackageState>, StorageError> {
        nope()
    }
    fn processing(
        &self,
        _: &str,
        _: &Id,
        _: &EventId,
    ) -> Result<Option<ProcessingRecord>, StorageError> {
        nope()
    }
}
impl PackageRegistryRead for Unsupported {
    fn registry_revision(&self) -> Result<i64, StorageError> {
        nope()
    }
    fn package(&self, _: &str) -> Result<Option<StoredPackage>, StorageError> {
        nope()
    }
    fn packages(&self, _: Option<&str>, _: u16) -> Result<Vec<StoredPackage>, StorageError> {
        nope()
    }
    fn package_version(
        &self,
        _: &str,
        _: &str,
    ) -> Result<Option<StoredPackageVersion>, StorageError> {
        nope()
    }
    fn package_versions(&self, _: &str) -> Result<Vec<StoredPackageVersion>, StorageError> {
        nope()
    }
    fn proposal(&self, _: &Id) -> Result<Option<StoredPackageProposal>, StorageError> {
        nope()
    }
    fn proposal_for(
        &self,
        _: &str,
        _: &str,
    ) -> Result<Option<StoredPackageProposal>, StorageError> {
        nope()
    }
    fn state_generation(
        &self,
        _: &str,
        _: i64,
    ) -> Result<Option<StoredStateGeneration>, StorageError> {
        nope()
    }
    fn generation_state(
        &self,
        _: &str,
        _: i64,
    ) -> Result<Option<(i64, serde_json::Value)>, StorageError> {
        nope()
    }
    fn indexes(&self) -> Result<Vec<StoredIndex>, StorageError> {
        nope()
    }
    fn observation(&self, _: &str) -> Result<Option<StoredObservation>, StorageError> {
        nope()
    }
}
impl PackageRegistry for Unsupported {
    fn advance_registry(&mut self, _: i64) -> Result<i64, StorageError> {
        nope()
    }
    fn insert_package(&mut self, _: &StoredPackage) -> Result<StoredPackage, StorageError> {
        nope()
    }
    fn update_package(&mut self, _: &StoredPackage, _: i64) -> Result<StoredPackage, StorageError> {
        nope()
    }
    fn insert_version(&mut self, _: &StoredPackageVersion) -> Result<(), StorageError> {
        nope()
    }
    fn update_version(
        &mut self,
        _: &str,
        _: &str,
        _: VersionPhase,
        _: &[String],
        _: Option<&serde_json::Value>,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn insert_proposal(&mut self, _: &StoredPackageProposal) -> Result<(), StorageError> {
        nope()
    }
    fn update_proposal(
        &mut self,
        _: &StoredPackageProposal,
        _: i64,
    ) -> Result<StoredPackageProposal, StorageError> {
        nope()
    }
    fn publish_generation(
        &mut self,
        _: &StoredStateGeneration,
        _: Option<&serde_json::Value>,
        _: i64,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn put_index(&mut self, _: &StoredIndex) -> Result<(), StorageError> {
        nope()
    }
    fn retain_indexes(&mut self, _: &[String]) -> Result<(), StorageError> {
        nope()
    }
    fn put_observation(&mut self, _: &StoredObservation) -> Result<(), StorageError> {
        nope()
    }
}
impl PackageHandlerLedger for Unsupported {
    fn compare_and_set_state(
        &mut self,
        _: &str,
        _: i64,
        _: i64,
        _: serde_json::Value,
    ) -> Result<PackageState, StorageError> {
        nope()
    }
    fn record_processing(&mut self, _: &ProcessingRecord) -> Result<(), StorageError> {
        nope()
    }
}
impl DurableQueue for Unsupported {
    fn hold_for_eligibility(
        &mut self,
        _: &Id,
        _: &QueueFence,
        _: &str,
    ) -> Result<QueueEntry, StorageError> {
        nope()
    }
    fn enqueue(&mut self, _: NewQueueEntry) -> Result<QueueEntry, StorageError> {
        nope()
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
impl GrantRead for Unsupported {
    fn grant(&self, _: &Id) -> Result<Option<GrantRecord>, StorageError> {
        nope()
    }
}
impl AuthorityRead for Unsupported {
    fn principal(&self, _: &ActorId) -> Result<Option<PrincipalBinding>, StorageError> {
        nope()
    }
    fn authority_grant(&self, _: &Id) -> Result<Option<AuthorityGrant>, StorageError> {
        nope()
    }
    fn registered_repositories(&self) -> Result<Vec<RegisteredRepository>, StorageError> {
        nope()
    }
}
impl AuthorityLedger for Unsupported {
    fn bind_principal(
        &mut self,
        _: NewPrincipalBinding,
        _: Option<i64>,
    ) -> Result<PrincipalBinding, StorageError> {
        nope()
    }
    fn revoke_principal(&mut self, _: &ActorId, _: i64) -> Result<PrincipalBinding, StorageError> {
        nope()
    }
    fn issue_grant(&mut self, _: NewAuthorityGrant) -> Result<AuthorityGrant, StorageError> {
        nope()
    }
    fn revoke_grant(
        &mut self,
        _: &Id,
        _: i64,
        _: &ActorId,
    ) -> Result<AuthorityGrant, StorageError> {
        nope()
    }
    fn consume_grant(&mut self, _: &GrantUse, _: &ActorId) -> Result<AuthorityGrant, StorageError> {
        nope()
    }
}
impl GrantLedger for Unsupported {
    fn issue(&mut self, _: NewGrant) -> Result<GrantRecord, StorageError> {
        nope()
    }
    fn consume(&mut self, _: &GrantUse, _: &ActorId) -> Result<GrantRecord, StorageError> {
        nope()
    }
    fn revoke(&mut self, _: &Id, _: i64) -> Result<GrantRecord, StorageError> {
        nope()
    }
}

impl ConfigurationRead for Unsupported {
    fn settings_secret(&self, _: &Id) -> Result<Option<StoredSettingsSecret>, StorageError> {
        nope()
    }
    fn settings_secret_referenced(&self, _: &Id) -> Result<bool, StorageError> {
        nope()
    }

    fn scope_exists(&self, _: &ConfigScope) -> Result<bool, StorageError> {
        nope()
    }
    fn current(&self, _: &ConfigScope, _: &str) -> Result<Option<ConfigRevision>, StorageError> {
        nope()
    }
    fn revision(
        &self,
        _: &ConfigScope,
        _: &str,
        _: i64,
    ) -> Result<Option<ConfigRevision>, StorageError> {
        nope()
    }
    fn history(
        &self,
        _: &ConfigScope,
        _: &str,
        _: Option<i64>,
        _: u16,
    ) -> Result<Vec<ConfigRevision>, StorageError> {
        nope()
    }
    fn proposal(&self, _: &Id) -> Result<Option<ProposalRecord>, StorageError> {
        nope()
    }
    fn proposals(
        &self,
        _: &ConfigScope,
        _: &str,
        _: Option<ProposalState>,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<ProposalRecord>, StorageError> {
        nope()
    }
}
impl ConfigurationRepository for Unsupported {
    fn reserve_settings_secret(&mut self, _: &Id, _: i64) -> Result<(), StorageError> {
        nope()
    }
    fn publish_settings_secret(&mut self, _: &Id) -> Result<bool, StorageError> {
        nope()
    }
    fn retain_settings_secret(&mut self, _: &Id) -> Result<(), StorageError> {
        nope()
    }
    fn retire_settings_secret(&mut self, _: &Id, _: i64, _: bool) -> Result<bool, StorageError> {
        nope()
    }
    fn finish_settings_secret_deletion(&mut self, _: &Id) -> Result<(), StorageError> {
        nope()
    }

    fn append_revision(
        &mut self,
        _: &ConfigScope,
        _: &str,
        _: i64,
        _: i64,
        _: serde_json::Value,
    ) -> Result<ConfigRevision, StorageError> {
        nope()
    }
    fn stage(&mut self, _: NewProposal) -> Result<ProposalRecord, StorageError> {
        nope()
    }
    fn update_proposal(
        &mut self,
        _: &Id,
        _: i64,
        _: ProposalChange,
    ) -> Result<ProposalRecord, StorageError> {
        nope()
    }
}

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
    pub store: MemoryStore,
    pub ids: SeqIds,
}
impl Harness {
    pub fn new() -> Self {
        Self {
            authority: Authority::default(),
            store: MemoryStore::new(&["user-a", "user-b", "agent-a"]),
            ids: SeqIds::default(),
        }
    }
    pub fn gateway(&self) -> Gateway<'_> {
        Gateway {
            authority: &self.authority,
            store: &self.store,
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

impl ArtifactRead for Unsupported {
    fn list_workspace(
        &self,
        _: Option<&ActorId>,
        _: &[String],
        _: Option<&ListPosition>,
        _: u16,
    ) -> Result<Vec<StoredArtifact>, StorageError> {
        nope()
    }
    fn workspace_usage(&self) -> Result<i64, StorageError> {
        nope()
    }
    fn receipt_usage(&self, _: Option<&ProjectId>) -> Result<i64, StorageError> {
        nope()
    }
    fn project_exists(&self, _: &ProjectId) -> Result<bool, StorageError> {
        nope()
    }
    fn subject_exists(&self, _: &ProjectId, _: &SubjectKey) -> Result<bool, StorageError> {
        nope()
    }
    fn artifact(&self, _: &Id) -> Result<Option<StoredArtifact>, StorageError> {
        nope()
    }
    fn version(&self, _: &Id) -> Result<Option<StoredVersion>, StorageError> {
        nope()
    }
    fn versions(&self, _: &Id) -> Result<Vec<StoredVersion>, StorageError> {
        nope()
    }
    fn version_by_operation(&self, _: &OperationId) -> Result<Option<StoredVersion>, StorageError> {
        nope()
    }
    fn list(
        &self,
        _: &ProjectId,
        _: Option<&ActorId>,
        _: &[String],
        _: Option<&ListPosition>,
        _: u16,
    ) -> Result<Vec<StoredArtifact>, StorageError> {
        nope()
    }
    fn tags(&self, _: &Id) -> Result<Vec<String>, StorageError> {
        nope()
    }
    fn shares(&self, _: &Id) -> Result<Vec<ProjectId>, StorageError> {
        nope()
    }
    fn repository_text_readers(&self, _: &Id) -> Result<Vec<ProjectId>, StorageError> {
        nope()
    }
    fn links(&self, _: &Id) -> Result<Vec<ArtifactLink>, StorageError> {
        nope()
    }
    fn linked_to(&self, _: &SubjectKey) -> Result<Vec<ArtifactLink>, StorageError> {
        nope()
    }
    fn blob(&self, _: &str) -> Result<Option<BlobRecord>, StorageError> {
        nope()
    }
    fn live_references(&self, _: &str) -> Result<i64, StorageError> {
        nope()
    }
    fn pinned(&self, _: &str) -> Result<bool, StorageError> {
        nope()
    }
    fn blobs_in(
        &self,
        _: BlobState,
        _: Option<&str>,
        _: u16,
    ) -> Result<Vec<BlobRecord>, StorageError> {
        nope()
    }
    fn project_usage(&self, _: &ProjectId) -> Result<i64, StorageError> {
        nope()
    }
    fn unresolved_staging(
        &self,
        _: Option<&OperationId>,
        _: u16,
    ) -> Result<Vec<OperationId>, StorageError> {
        nope()
    }
}
impl ArtifactRepository for Unsupported {
    fn put_blob(&mut self, _: &str, _: i64, _: &str, _: &str) -> Result<BlobRecord, StorageError> {
        nope()
    }
    fn set_blob_state(
        &mut self,
        _: &str,
        _: BlobState,
        _: BlobState,
        _: Option<&str>,
    ) -> Result<BlobRecord, StorageError> {
        nope()
    }
    fn insert_artifact(&mut self, _: &StoredArtifact) -> Result<(), StorageError> {
        nope()
    }
    fn insert_version(&mut self, _: &StoredVersion) -> Result<(), StorageError> {
        nope()
    }
    fn update_artifact(&mut self, _: &StoredArtifact) -> Result<(), StorageError> {
        nope()
    }
    fn tombstone_version(&mut self, _: &Id, _: i64) -> Result<(), StorageError> {
        nope()
    }
    fn add_link(&mut self, _: &ArtifactLink) -> Result<bool, StorageError> {
        nope()
    }
    fn set_tags(&mut self, _: &Id, _: &[String]) -> Result<(), StorageError> {
        nope()
    }
    fn add_share(&mut self, _: &Id, _: &ProjectId) -> Result<bool, StorageError> {
        nope()
    }
    fn remove_share(&mut self, _: &Id, _: &ProjectId) -> Result<bool, StorageError> {
        nope()
    }
    fn pin(&mut self, _: &Id, _: &str) -> Result<(), StorageError> {
        nope()
    }
    fn release_pins(&mut self, _: &Id) -> Result<i64, StorageError> {
        nope()
    }
    fn claim_blob(&mut self, _: &str, _: &str, _: &str) -> Result<bool, StorageError> {
        nope()
    }
    fn adopt_claim(&mut self, _: &str, _: &str, _: &str, _: &str) -> Result<bool, StorageError> {
        nope()
    }
    fn finish_claim(
        &mut self,
        _: &str,
        _: &str,
        _: &str,
        _: Option<&str>,
    ) -> Result<bool, StorageError> {
        nope()
    }
}
impl SessionRead for Unsupported {
    fn all_usage_facts(
        &self,
        _: Option<&SequenceKey>,
        _: u16,
    ) -> Result<Vec<UsageFact>, StorageError> {
        nope()
    }
    fn last_entry_at_ms(&self, _: &Id) -> Result<Option<i64>, StorageError> {
        nope()
    }
    // The in-memory store holds no work items or projects: the gateway resolves no command
    // target or lineage against it.
    fn work_root(&self, _: &RootId) -> Result<Option<WorkRoot>, StorageError> {
        Ok(None)
    }
    fn session(&self, _: &Id) -> Result<Option<StoredSession>, StorageError> {
        nope()
    }
    fn session_repository_scope(&self, _: &Id) -> Result<Vec<Id>, StorageError> {
        nope()
    }
    fn session_activity_at_ms(&self, _: &Id) -> Result<Option<i64>, StorageError> {
        nope()
    }
    fn sessions(
        &self,
        _: &SessionScope,
        _: Option<bool>,
        _: Option<&SessionPosition>,
        _: u16,
    ) -> Result<Vec<StoredSession>, StorageError> {
        nope()
    }
    fn entries(&self, _: &Id, _: i64, _: u16) -> Result<Vec<StoredEntry>, StorageError> {
        nope()
    }
    fn entry_exists(&self, _: &Id, _: &Id) -> Result<bool, StorageError> {
        nope()
    }
    fn batch(&self, _: &Id, _: &Id) -> Result<Option<StoredBatch>, StorageError> {
        nope()
    }
    fn project_exists(&self, _: &ProjectId) -> Result<bool, StorageError> {
        Ok(false)
    }
    fn work_membership(&self, _: &RootId) -> Result<String, StorageError> {
        nope()
    }
    fn project_has_repository(&self, _: &ProjectId, _: &Id) -> Result<bool, StorageError> {
        nope()
    }
    fn tool_call(&self, _: &Id, _: &Id, _: &str) -> Result<Option<StoredToolCall>, StorageError> {
        nope()
    }
    fn ordinal_taken(&self, _: &Id, _: &Id, _: i64) -> Result<bool, StorageError> {
        nope()
    }
    fn pending_calls(&self, _: &Id, _: u16) -> Result<Vec<StoredToolCall>, StorageError> {
        nope()
    }
    fn linked_operations(&self, _: &Id, _: u16) -> Result<Vec<StoredToolCall>, StorageError> {
        nope()
    }
    fn epoch(&self, _: &Id, _: i64) -> Result<Option<StoredEpoch>, StorageError> {
        nope()
    }
    fn provider_context_head(
        &self,
        _: &Id,
    ) -> Result<Option<StoredProviderContextHead>, StorageError> {
        nope()
    }
    fn context_sources(
        &self,
        _: &Id,
        _: i64,
        _: i64,
        _: u16,
    ) -> Result<Vec<StoredContextSource>, StorageError> {
        nope()
    }
    fn latest_checkpoint(&self, _: &Id) -> Result<Option<StoredCheckpoint>, StorageError> {
        nope()
    }
    fn usage(
        &self,
        _: &RootId,
        _: Option<&SequenceKey>,
        _: u16,
    ) -> Result<Vec<(SequenceKey, StoredUsage)>, StorageError> {
        nope()
    }
    fn link(&self, _: &Id) -> Result<Option<StoredLink>, StorageError> {
        nope()
    }
    fn links(
        &self,
        _: &RootId,
        _: &[&str],
        _: Option<&SequenceKey>,
        _: u16,
    ) -> Result<Vec<StoredLink>, StorageError> {
        nope()
    }
    fn links_to(&self, _: &str, _: &str, _: u16) -> Result<Vec<StoredLink>, StorageError> {
        nope()
    }
    fn lineage(
        &self,
        _: &RootId,
        _: Option<&SequenceKey>,
        _: u16,
    ) -> Result<Vec<LineageRow>, StorageError> {
        nope()
    }
    fn root_events(
        &self,
        _: &RootId,
        _: &[&str],
        _: i64,
        _: u16,
    ) -> Result<Vec<EventRecord>, StorageError> {
        nope()
    }
    fn decisions(
        &self,
        _: &HistoryScope,
        _: i64,
        _: u16,
    ) -> Result<Vec<EventRecord>, StorageError> {
        nope()
    }
    fn project_scope_revision(&self, _: &ProjectId) -> Result<Option<i64>, StorageError> {
        Ok(None)
    }
    fn usage_facts(
        &self,
        _: &HistoryScope,
        _: Option<&SequenceKey>,
        _: u16,
    ) -> Result<Vec<UsageFact>, StorageError> {
        nope()
    }
    fn decisions_before(
        &self,
        _: &HistoryScope,
        _: Option<i64>,
        _: u16,
    ) -> Result<Vec<EventRecord>, StorageError> {
        nope()
    }
}
impl SessionRepository for Unsupported {
    fn touch_session_activity(&mut self, _: &Id, _: i64) -> Result<(), StorageError> {
        nope()
    }
    fn insert_session(&mut self, _: &StoredSession) -> Result<(), StorageError> {
        nope()
    }
    fn update_session(&mut self, _: &StoredSession) -> Result<(), StorageError> {
        nope()
    }
    fn insert_entry(&mut self, _: &StoredEntry) -> Result<(), StorageError> {
        nope()
    }
    fn insert_batch(&mut self, _: &StoredBatch) -> Result<(), StorageError> {
        nope()
    }
    fn insert_tool_call(&mut self, _: &StoredToolCall) -> Result<(), StorageError> {
        nope()
    }
    fn record_response(&mut self, _: &StoredToolCall) -> Result<(), StorageError> {
        nope()
    }
    fn insert_epoch(&mut self, _: &StoredEpoch) -> Result<(), StorageError> {
        nope()
    }
    fn put_provider_context_head(
        &mut self,
        _: &StoredProviderContextHead,
        _: Option<i64>,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn insert_context_source(&mut self, _: &StoredContextSource) -> Result<(), StorageError> {
        nope()
    }
    fn insert_checkpoint(&mut self, _: &StoredCheckpoint) -> Result<(), StorageError> {
        nope()
    }
    fn insert_usage(&mut self, _: &StoredUsage) -> Result<(), StorageError> {
        nope()
    }
    fn insert_invocation(&mut self, _: &StoredInvocation) -> Result<(), StorageError> {
        nope()
    }
    fn insert_link(&mut self, _: &StoredLink) -> Result<(), StorageError> {
        nope()
    }
}
impl ResourceRead for Unsupported {
    fn resource(&self, _: &Id) -> Result<Option<StoredResource>, StorageError> {
        nope()
    }
    fn resource_by_key(&self, _: &str) -> Result<Option<StoredResource>, StorageError> {
        nope()
    }
    fn resources(
        &self,
        _: &ResourceFilter,
        _: Option<&str>,
        _: u16,
    ) -> Result<Vec<StoredResource>, StorageError> {
        nope()
    }
    fn lease(&self, _: &Id) -> Result<Option<StoredLease>, StorageError> {
        nope()
    }
    fn holder_lease(&self, _: &Id, _: &str) -> Result<Option<StoredLease>, StorageError> {
        nope()
    }
    fn holders(&self, _: &Id, _: i64, _: u16) -> Result<Vec<StoredLease>, StorageError> {
        nope()
    }
    fn waiters(
        &self,
        _: &Id,
        _: Option<QueuePosition>,
        _: u16,
    ) -> Result<Vec<StoredLease>, StorageError> {
        nope()
    }
    fn open_leases(
        &self,
        _: &OpenLeaseFilter,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredLease>, StorageError> {
        nope()
    }
    fn process(&self, _: &Id) -> Result<Option<StoredProcess>, StorageError> {
        nope()
    }
    fn processes(
        &self,
        _: &ProcessFilter,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredProcess>, StorageError> {
        nope()
    }
    fn container(&self, _: &Id) -> Result<Option<StoredContainer>, StorageError> {
        nope()
    }
    fn container_lease_in_use(&self, _: &Id) -> Result<bool, StorageError> {
        nope()
    }
    fn containers(
        &self,
        _: &ContainerFilter,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredContainer>, StorageError> {
        nope()
    }
}
impl JobRead for Unsupported {
    fn usage_history(
        &self,
        _: Option<&UsageJobPosition>,
        _: u16,
    ) -> Result<Vec<StoredJobUsage>, StorageError> {
        nope()
    }
    fn job(&self, _: &Id) -> Result<Option<StoredJob>, StorageError> {
        nope()
    }
    fn jobs(
        &self,
        _: &JobScope,
        _: Option<bool>,
        _: Option<(&str, &str)>,
        _: u16,
    ) -> Result<Vec<StoredJob>, StorageError> {
        nope()
    }
    fn job_titled(
        &self,
        _: Option<&ProjectId>,
        _: &str,
    ) -> Result<Option<StoredJob>, StorageError> {
        nope()
    }
    fn due_jobs(&self, _: i64, _: u16) -> Result<Vec<StoredJob>, StorageError> {
        nope()
    }
    fn all_jobs(&self, _: Option<&Id>, _: u16) -> Result<Vec<StoredJob>, StorageError> {
        nope()
    }
    fn job_run(&self, _: &Id) -> Result<Option<StoredJobRun>, StorageError> {
        nope()
    }
    fn job_runs(&self, _: &Id, _: Option<i64>, _: u16) -> Result<Vec<StoredJobRun>, StorageError> {
        nope()
    }
    fn active_run(&self, _: &Id) -> Result<Option<StoredJobRun>, StorageError> {
        nope()
    }
    fn runs_where(
        &self,
        _: &RunFilter,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredJobRun>, StorageError> {
        nope()
    }
    fn run_links(&self, _: &Id) -> Result<Vec<RootId>, StorageError> {
        nope()
    }
    fn checkpoint(&self, _: &Id, _: &str) -> Result<Option<StoredJobCheckpoint>, StorageError> {
        nope()
    }
    fn action(&self, _: &Id, _: &str) -> Result<Option<StoredJobAction>, StorageError> {
        nope()
    }
    fn unsettled_actions(&self, _: &Id) -> Result<Vec<StoredJobAction>, StorageError> {
        nope()
    }
    fn run_usage(
        &self,
        _: &Id,
        _: Option<i64>,
        _: u16,
    ) -> Result<Vec<StoredJobUsage>, StorageError> {
        nope()
    }
    fn usage_totals(&self, _: &Id) -> Result<JobUsageTotals, StorageError> {
        nope()
    }
}
impl JobRepository for Unsupported {
    fn insert_job(&mut self, _: &StoredJob) -> Result<StoredJob, StorageError> {
        nope()
    }
    fn update_job(&mut self, _: &StoredJob, _: i64) -> Result<StoredJob, StorageError> {
        nope()
    }
    fn set_job_schedule(
        &mut self,
        _: &Id,
        _: i64,
        _: Option<i64>,
        _: Option<&Id>,
    ) -> Result<StoredJob, StorageError> {
        nope()
    }
    fn insert_run(&mut self, _: &StoredJobRun) -> Result<StoredJobRun, StorageError> {
        nope()
    }
    fn update_run(&mut self, _: &StoredJobRun, _: i64) -> Result<StoredJobRun, StorageError> {
        nope()
    }
    fn set_run_checkpoint(&mut self, _: &Id, _: &str) -> Result<(), StorageError> {
        nope()
    }
    fn link_root(&mut self, _: &Id, _: &RootId) -> Result<(), StorageError> {
        nope()
    }
    fn put_checkpoint(
        &mut self,
        _: &StoredJobCheckpoint,
        _: Option<i64>,
    ) -> Result<StoredJobCheckpoint, StorageError> {
        nope()
    }
    fn insert_action(&mut self, _: &StoredJobAction) -> Result<StoredJobAction, StorageError> {
        nope()
    }
    fn update_action(
        &mut self,
        _: &StoredJobAction,
        _: i64,
    ) -> Result<StoredJobAction, StorageError> {
        nope()
    }
    fn record_usage(&mut self, _: &StoredJobUsage) -> Result<StoredJobUsage, StorageError> {
        nope()
    }
}
impl EmbeddingRead for Unsupported {
    fn embedding_price_subject(
        &self,
        _: &Id,
    ) -> Result<Option<StoredEmbeddingPriceSubject>, StorageError> {
        nope()
    }
    fn embedding_price_subject_for_candidate(
        &self,
        _: &Id,
    ) -> Result<Option<StoredEmbeddingPriceSubject>, StorageError> {
        nope()
    }
    fn embedding_price_subjects(
        &self,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredEmbeddingPriceSubject>, StorageError> {
        nope()
    }
    fn embedding_inventory_revision(&self) -> Result<i64, StorageError> {
        nope()
    }
    fn embedding_candidate(
        &self,
        _: &Id,
    ) -> Result<Option<StoredEmbeddingCandidate>, StorageError> {
        nope()
    }
    fn embedding_candidates(
        &self,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredEmbeddingCandidate>, StorageError> {
        nope()
    }
    fn embedding_qualification(
        &self,
        _: &Id,
    ) -> Result<Option<StoredEmbeddingQualification>, StorageError> {
        nope()
    }
    fn embedding_selection(&self) -> Result<StoredEmbeddingSelection, StorageError> {
        nope()
    }
}
impl EmbeddingRepository for Unsupported {
    fn insert_embedding_candidate(
        &mut self,
        _: &StoredEmbeddingCandidate,
        _: i64,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn start_embedding_qualification(
        &mut self,
        _: &Id,
        _: i64,
        _: &StoredEmbeddingProbe,
    ) -> Result<StoredEmbeddingQualification, StorageError> {
        nope()
    }
    fn stage_embedding_probe_evidence(
        &mut self,
        _: &Id,
        _: i64,
        _: &OperationId,
        _: &AttemptFence,
        _: &StoredEmbeddingEvidence,
    ) -> Result<StoredEmbeddingQualification, StorageError> {
        nope()
    }
    fn finish_embedding_qualification(
        &mut self,
        _: &Id,
        _: i64,
        _: &OperationId,
        _: Option<&AttemptFence>,
        _: EmbeddingQualificationState,
        _: Option<&str>,
    ) -> Result<StoredEmbeddingQualification, StorageError> {
        nope()
    }
    fn select_embedding_candidate(
        &mut self,
        _: i64,
        _: Option<(&Id, i64)>,
    ) -> Result<StoredEmbeddingSelection, StorageError> {
        nope()
    }
}
impl RouterRead for Unsupported {
    fn route_requests(
        &self,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredRouteRequest>, StorageError> {
        nope()
    }
    fn route_model(&self, _: &Id) -> Result<Option<StoredRouteModel>, StorageError> {
        nope()
    }
    fn route_models(
        &self,
        _: Option<(i64, &str)>,
        _: u16,
    ) -> Result<Vec<StoredRouteModel>, StorageError> {
        nope()
    }
    fn tier_cursor(&self, _: &str) -> Result<i64, StorageError> {
        nope()
    }
    fn session_pin(&self, _: &Id) -> Result<Option<StoredPin>, StorageError> {
        nope()
    }
    fn cooldowns(&self) -> Result<Vec<StoredCooldown>, StorageError> {
        nope()
    }
    fn route_request(&self, _: &Id) -> Result<Option<StoredRouteRequest>, StorageError> {
        nope()
    }
    fn connection_account_revision(&self, _: &Id) -> Result<Option<i64>, StorageError> {
        nope()
    }
    fn connection(&self, _: &Id) -> Result<Option<StoredConnection>, StorageError> {
        nope()
    }
    fn connections(&self, _: Option<&str>, _: u16) -> Result<Vec<StoredConnection>, StorageError> {
        nope()
    }
    fn decision_model(&self, _: &Id) -> Result<Option<StoredDecisionModel>, StorageError> {
        nope()
    }
    fn decision_models(
        &self,
        _: Option<&str>,
        _: u16,
    ) -> Result<Vec<StoredDecisionModel>, StorageError> {
        nope()
    }
}
impl RouterRepository for Unsupported {
    fn put_connection(
        &mut self,
        _: &StoredConnection,
        _: Option<i64>,
    ) -> Result<StoredConnection, StorageError> {
        nope()
    }
    fn delete_connection(&mut self, _: &Id, _: i64) -> Result<(), StorageError> {
        nope()
    }
    fn put_decision_model(
        &mut self,
        _: &StoredDecisionModel,
        _: Option<i64>,
    ) -> Result<StoredDecisionModel, StorageError> {
        nope()
    }
    fn put_route_model(
        &mut self,
        _: &StoredRouteModel,
        _: Option<i64>,
    ) -> Result<StoredRouteModel, StorageError> {
        nope()
    }
    fn advance_tier_cursor(&mut self, _: &str, _: i64) -> Result<i64, StorageError> {
        nope()
    }
    fn put_session_pin(
        &mut self,
        _: &StoredPin,
        _: Option<i64>,
    ) -> Result<StoredPin, StorageError> {
        nope()
    }
    fn record_cooldown(&mut self, _: &StoredCooldown) -> Result<StoredCooldown, StorageError> {
        nope()
    }
    fn clear_cooldown(&mut self, _: &Id) -> Result<bool, StorageError> {
        nope()
    }
    fn record_route_request(&mut self, _: &StoredRouteRequest) -> Result<(), StorageError> {
        nope()
    }
}
impl ProjectRead for Unsupported {
    fn purge_planning(&self, _: &ProjectId) -> Result<Option<ProjectPurgePlanning>, StorageError> {
        nope()
    }
    fn purge_candidates(
        &self,
        _: &ProjectId,
    ) -> Result<Option<ProjectPurgeCandidates>, StorageError> {
        nope()
    }
    fn managed_file_recovery(
        &self,
        _: &str,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<ProjectManagedFileRecovery>, StorageError> {
        nope()
    }

    fn pending_project_managed_writes(&self, _: &ProjectId) -> Result<bool, StorageError> {
        nope()
    }

    fn project_managed_file(
        &self,
        _: &ProjectId,
        _: &Id,
    ) -> Result<Option<StoredProjectManagedFile>, StorageError> {
        nope()
    }
    fn project_managed_files(
        &self,
        _: &ProjectId,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredProjectManagedFile>, StorageError> {
        nope()
    }

    fn purge_manifest(
        &self,
        _: &ProjectId,
        _: &Id,
    ) -> Result<Option<StoredProjectPurge>, StorageError> {
        nope()
    }

    fn project(&self, _: &ProjectId) -> Result<Option<StoredProject>, StorageError> {
        nope()
    }
    fn projects(
        &self,
        _: &ProjectFilter,
        _: Option<&ProjectPosition>,
        _: u16,
    ) -> Result<Vec<StoredProject>, StorageError> {
        nope()
    }
    fn members(&self, _: &ProjectId) -> Result<Vec<Id>, StorageError> {
        nope()
    }
    fn repository(&self, _: &Id) -> Result<Option<StoredRepository>, StorageError> {
        nope()
    }
    fn repositories(
        &self,
        _: &RepositoryFilter,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredRepository>, StorageError> {
        nope()
    }
    fn repository_projects(&self, _: &Id) -> Result<Vec<ProjectId>, StorageError> {
        nope()
    }
    fn index_views(&self, _: &Id) -> Result<Vec<StoredIndexView>, StorageError> {
        nope()
    }
    fn index_view(&self, _: &Id) -> Result<Option<StoredIndexView>, StorageError> {
        nope()
    }
    fn repository_text(
        &self,
        _: &Id,
        _: TextKind,
    ) -> Result<Option<StoredRepositoryText>, StorageError> {
        nope()
    }
    fn work_naming_repository(&self, _: &ProjectId, _: &Id) -> Result<i64, StorageError> {
        nope()
    }
    fn pending_archive_stops(&self, _: u16) -> Result<Vec<ProjectId>, StorageError> {
        nope()
    }
}
impl WorkRead for Unsupported {
    fn allocation_replay(&self, _: &str) -> Result<Option<StoredAllocationReplay>, StorageError> {
        nope()
    }
    fn allocation_claim(&self, _: &Id) -> Result<Option<StoredAllocationClaim>, StorageError> {
        nope()
    }
    fn allocation_claims(
        &self,
        _: &ProjectId,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredAllocationClaim>, StorageError> {
        nope()
    }
    fn allocation_by_invocation(
        &self,
        _: &Id,
        _: &Id,
    ) -> Result<Option<StoredAllocationClaim>, StorageError> {
        nope()
    }
    fn work(&self, _: &Id) -> Result<Option<StoredWork>, StorageError> {
        nope()
    }
    fn works(
        &self,
        _: &WorkFilter,
        _: Option<&WorkPosition>,
        _: u16,
    ) -> Result<Vec<StoredWork>, StorageError> {
        nope()
    }
    fn work_repositories(&self, _: &Id) -> Result<Vec<Id>, StorageError> {
        nope()
    }
    fn prerequisites(&self, _: &Id) -> Result<Vec<Id>, StorageError> {
        nope()
    }
    fn dependents(&self, _: &Id) -> Result<Vec<Id>, StorageError> {
        nope()
    }
    fn open_prerequisites(&self, _: &Id) -> Result<Vec<Id>, StorageError> {
        nope()
    }
    fn queue_revision(&self, _: &ProjectId, _: &str) -> Result<i64, StorageError> {
        nope()
    }
    fn next_rank(&self, _: &ProjectId, _: &str) -> Result<i64, StorageError> {
        nope()
    }
    fn statuses(&self, _: &ProjectId) -> Result<Vec<StoredStatus>, StorageError> {
        nope()
    }
    fn by_caller_ref(&self, _: &ProjectId, _: &Id) -> Result<Option<Id>, StorageError> {
        nope()
    }
    fn feature_plan(&self, _: &Id) -> Result<Option<StoredFeaturePlan>, StorageError> {
        nope()
    }
    fn proposal(&self, _: &Id) -> Result<Option<StoredProposal>, StorageError> {
        nope()
    }
    fn latest_proposal(&self, _: &Id) -> Result<Option<StoredProposal>, StorageError> {
        nope()
    }
    fn readiness(&self, _: &Id) -> Result<Option<StoredReadiness>, StorageError> {
        nope()
    }
}
impl WorkRepository for Unsupported {
    fn insert_allocation_replay(&mut self, _: &StoredAllocationReplay) -> Result<(), StorageError> {
        nope()
    }
    fn insert_allocation_claim(&mut self, _: &StoredAllocationClaim) -> Result<(), StorageError> {
        nope()
    }
    fn update_allocation_claim(
        &mut self,
        _: &StoredAllocationClaim,
        _: i64,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn update_status(&mut self, _: &StoredStatus, _: i64) -> Result<(), StorageError> {
        nope()
    }
    fn delete_status(&mut self, _: &ProjectId, _: &Id, _: i64) -> Result<(), StorageError> {
        nope()
    }
    fn insert_work(&mut self, _: &StoredWork) -> Result<(), StorageError> {
        nope()
    }
    fn update_work(&mut self, _: &StoredWork, _: i64) -> Result<(), StorageError> {
        nope()
    }
    fn set_rank(&mut self, _: &Id, _: i64) -> Result<(), StorageError> {
        nope()
    }
    fn set_work_repositories(&mut self, _: &Id, _: &[Id]) -> Result<(), StorageError> {
        nope()
    }
    fn set_prerequisites(&mut self, _: &Id, _: &[Id]) -> Result<(), StorageError> {
        nope()
    }
    fn bump_queue(&mut self, _: &ProjectId, _: &str) -> Result<i64, StorageError> {
        nope()
    }
    fn insert_status(&mut self, _: &StoredStatus) -> Result<(), StorageError> {
        nope()
    }
    fn put_feature_plan(
        &mut self,
        _: &StoredFeaturePlan,
        _: Option<i64>,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn put_proposal(&mut self, _: &StoredProposal, _: Option<i64>) -> Result<(), StorageError> {
        nope()
    }
    fn put_readiness(&mut self, _: &StoredReadiness, _: Option<i64>) -> Result<(), StorageError> {
        nope()
    }
}
impl ProjectRepository for Unsupported {
    fn reserve_project_managed_file(
        &mut self,
        _: &ProjectManagedFileReservation,
    ) -> Result<(), StorageError> {
        nope()
    }

    fn register_project_managed_file(
        &mut self,
        _: &StoredProjectManagedFile,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn finish_project_managed_file(&mut self, _: &ProjectId, _: &Id) -> Result<(), StorageError> {
        nope()
    }

    fn insert_purge_manifest(&mut self, _: &StoredProjectPurge) -> Result<(), StorageError> {
        nope()
    }

    fn insert_project(&mut self, _: &StoredProject) -> Result<(), StorageError> {
        nope()
    }
    fn update_project(&mut self, _: &StoredProject, _: i64) -> Result<(), StorageError> {
        nope()
    }
    fn set_members(&mut self, _: &ProjectId, _: &[Id]) -> Result<(), StorageError> {
        nope()
    }
    fn insert_repository(&mut self, _: &StoredRepository) -> Result<(), StorageError> {
        nope()
    }
    fn touch_repository(&mut self, _: &Id, _: i64) -> Result<i64, StorageError> {
        nope()
    }
    fn publish_index_view(&mut self, _: &StoredIndexView) -> Result<(), StorageError> {
        nope()
    }
    fn put_repository_text(&mut self, _: &StoredRepositoryText) -> Result<(), StorageError> {
        nope()
    }
    fn remove_index(&mut self, _: &Id) -> Result<u64, StorageError> {
        nope()
    }
    fn finish_archive_stop(&mut self, _: &ProjectId, _: i64) -> Result<bool, StorageError> {
        nope()
    }
}
impl IndexRead for Unsupported {
    fn source_config(&self, _: &str) -> Result<Option<StoredSourceConfig>, StorageError> {
        nope()
    }
    fn repository_source(&self, _: &Id) -> Result<Option<StoredRepositorySource>, StorageError> {
        nope()
    }
    fn repository_at(&self, _: &str) -> Result<Option<Id>, StorageError> {
        nope()
    }
    fn worktrees(&self, _: &Id) -> Result<Vec<StoredWorktree>, StorageError> {
        nope()
    }
    fn run(&self, _: &Id) -> Result<Option<StoredIndexRun>, StorageError> {
        nope()
    }
    fn runs(
        &self,
        _: &IndexRunFilter,
        _: Option<&RunPosition>,
        _: u16,
    ) -> Result<Vec<StoredIndexRun>, StorageError> {
        nope()
    }
    fn active_run(&self, _: &Id, _: Option<&Id>) -> Result<Option<StoredIndexRun>, StorageError> {
        nope()
    }
    fn batch_runs(&self, _: &Id) -> Result<Vec<StoredIndexRun>, StorageError> {
        nope()
    }
    fn latest_run(&self, _: &Id, _: Option<&Id>) -> Result<Option<StoredIndexRun>, StorageError> {
        nope()
    }
    fn claimable_runs(&self, _: i64, _: u16) -> Result<Vec<StoredIndexRun>, StorageError> {
        nope()
    }
    fn running_runs(&self, _: u16) -> Result<Vec<StoredIndexRun>, StorageError> {
        nope()
    }
    fn generation(&self, _: &Id) -> Result<Option<StoredGeneration>, StorageError> {
        nope()
    }
    fn generations(&self, _: &Id) -> Result<Vec<StoredGeneration>, StorageError> {
        nope()
    }
    fn generation_in_view(&self, _: &Id) -> Result<bool, StorageError> {
        nope()
    }
    fn generations_in_state(&self, _: &str, _: u16) -> Result<Vec<StoredGeneration>, StorageError> {
        nope()
    }
    fn units(&self, _: &Id, _: Option<&str>, _: u16) -> Result<Vec<StoredUnit>, StorageError> {
        nope()
    }
    fn unit(&self, _: &Id, _: &str) -> Result<Option<StoredUnit>, StorageError> {
        nope()
    }
    fn nodes(&self, _: &Id, _: &str) -> Result<Vec<StoredNode>, StorageError> {
        nope()
    }
    fn edges(&self, _: &Id, _: &str) -> Result<Vec<StoredEdge>, StorageError> {
        nope()
    }
    fn nodes_named(&self, _: &Id, _: &str, _: u16) -> Result<Vec<StoredNode>, StorageError> {
        nope()
    }
    fn nodes_qualified(&self, _: &Id, _: &str, _: u16) -> Result<Vec<StoredNode>, StorageError> {
        nope()
    }
    fn published(&self, _: &Id) -> Result<Option<StoredPublished>, StorageError> {
        nope()
    }
    fn view(&self, _: &Id) -> Result<Option<StoredView>, StorageError> {
        nope()
    }
    fn live_pins(&self, _: &Id, _: i64) -> Result<i64, StorageError> {
        nope()
    }
    fn overlay(&self, _: &Id) -> Result<Option<StoredOverlay>, StorageError> {
        nope()
    }
    fn overlay_for(&self, _: &Id, _: &Id) -> Result<Option<StoredOverlay>, StorageError> {
        nope()
    }
    fn overlay_at(&self, _: &Id, _: &str) -> Result<Option<StoredOverlay>, StorageError> {
        nope()
    }
    fn overlays(&self, _: &Id) -> Result<Vec<StoredOverlay>, StorageError> {
        nope()
    }
    fn overlays_due(&self, _: i64, _: u16) -> Result<Vec<StoredOverlay>, StorageError> {
        nope()
    }
    fn prunable_ref_overlays(
        &self,
        _: i64,
        _: u32,
        _: u16,
    ) -> Result<Vec<StoredOverlay>, StorageError> {
        nope()
    }
}
impl IndexRepository for Unsupported {
    fn put_source_config(&mut self, _: &StoredSourceConfig, _: i64) -> Result<(), StorageError> {
        nope()
    }
    fn put_repository_source(&mut self, _: &StoredRepositorySource) -> Result<(), StorageError> {
        nope()
    }
    fn put_worktrees(&mut self, _: &Id, _: &[StoredWorktree]) -> Result<(), StorageError> {
        nope()
    }
    fn insert_run(&mut self, _: &StoredIndexRun) -> Result<(), StorageError> {
        nope()
    }
    fn update_run(&mut self, _: &StoredIndexRun, _: i64) -> Result<(), StorageError> {
        nope()
    }
    fn insert_generation(&mut self, _: &StoredGeneration) -> Result<(), StorageError> {
        nope()
    }
    fn update_generation(&mut self, _: &StoredGeneration) -> Result<(), StorageError> {
        nope()
    }
    fn put_unit(
        &mut self,
        _: &StoredUnit,
        _: &[StoredNode],
        _: &[StoredEdge],
    ) -> Result<(), StorageError> {
        nope()
    }
    fn inherit_units(&mut self, _: &Id, _: &Id, _: &[String]) -> Result<u64, StorageError> {
        nope()
    }
    fn set_edge_targets(&mut self, _: &Id, _: &[EdgeTargetUpdate]) -> Result<(), StorageError> {
        nope()
    }
    fn reclaim_generation(&mut self, _: &Id, _: u32) -> Result<u64, StorageError> {
        nope()
    }
    fn insert_view(&mut self, _: &StoredView) -> Result<(), StorageError> {
        nope()
    }
    fn publish(&mut self, _: &Id, _: &Id, _: i64) -> Result<i64, StorageError> {
        nope()
    }
    fn remove_view(&mut self, _: &Id) -> Result<(), StorageError> {
        nope()
    }
    fn put_overlay(&mut self, _: &StoredOverlay, _: Option<i64>) -> Result<(), StorageError> {
        nope()
    }
    fn remove_overlay(&mut self, _: &Id, _: i64) -> Result<(), StorageError> {
        nope()
    }
}
impl DiagnosticRead for Unsupported {
    fn diagnostic(&self, _: &Id) -> Result<Option<StoredDiagnostic>, StorageError> {
        nope()
    }
    fn diagnostics(
        &self,
        _: &DiagnosticFilter,
        _: u16,
    ) -> Result<Vec<StoredDiagnostic>, StorageError> {
        nope()
    }
    fn count_diagnostics(&self, _: &DiagnosticFilter) -> Result<i64, StorageError> {
        nope()
    }
    fn diagnostic_stats(&self) -> Result<DiagnosticStats, StorageError> {
        nope()
    }
    fn export_intent(
        &self,
        _: &ActorId,
        _: &str,
        _: &str,
    ) -> Result<Option<ExportIntent>, StorageError> {
        nope()
    }
}
impl DiagnosticRepository for Unsupported {
    fn append_diagnostic(
        &mut self,
        _: &StoredDiagnostic,
    ) -> Result<StoredDiagnostic, StorageError> {
        nope()
    }
    fn prune_diagnostics(&mut self, _: i64, _: i64) -> Result<PrunedDiagnostics, StorageError> {
        nope()
    }
    fn claim_export_intent(&mut self, _: &ExportIntent) -> Result<ExportIntent, StorageError> {
        nope()
    }
    fn complete_export_intent(&mut self, _: &ExportIntent, _: &str) -> Result<bool, StorageError> {
        nope()
    }
    fn take_over_export_intent(&mut self, _: &ExportIntent, _: &str) -> Result<bool, StorageError> {
        nope()
    }
    fn release_export_intent(&mut self, _: &ExportIntent) -> Result<bool, StorageError> {
        nope()
    }
}
impl BrowserRead for Unsupported {
    fn browser_session(&self, _: &Id) -> Result<Option<StoredBrowserSession>, StorageError> {
        nope()
    }
    fn browser_sessions(
        &self,
        _: &BrowserFilter,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredBrowserSession>, StorageError> {
        nope()
    }
    fn browser_observations(
        &self,
        _: &Id,
        _: BrowserChannel,
        _: i64,
        _: u16,
    ) -> Result<Vec<StoredBrowserObservation>, StorageError> {
        nope()
    }
}
impl BrowserRepository for Unsupported {
    fn insert_browser_session(&mut self, _: &StoredBrowserSession) -> Result<(), StorageError> {
        nope()
    }
    fn update_browser_session(
        &mut self,
        _: &StoredBrowserSession,
        _: i64,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn next_observation_sequence(&mut self, _: &Id) -> Result<i64, StorageError> {
        nope()
    }
    fn set_last_action(&mut self, _: &Id, _: &OperationId) -> Result<(), StorageError> {
        nope()
    }
    fn insert_browser_observation(
        &mut self,
        _: &StoredBrowserObservation,
    ) -> Result<(), StorageError> {
        nope()
    }
}
impl ResourceRepository for Unsupported {
    fn insert_resource(&mut self, _: NewResource) -> Result<StoredResource, StorageError> {
        nope()
    }
    fn redefine_resource(
        &mut self,
        _: &Id,
        _: i64,
        _: i64,
        _: &[String],
        _: bool,
    ) -> Result<StoredResource, StorageError> {
        nope()
    }
    fn next_fence(&mut self, _: &Id) -> Result<i64, StorageError> {
        nope()
    }
    fn next_queue_ticket(&mut self, _: &Id) -> Result<i64, StorageError> {
        nope()
    }
    fn insert_lease(&mut self, _: &StoredLease) -> Result<(), StorageError> {
        nope()
    }
    fn update_lease(&mut self, _: &StoredLease, _: i64) -> Result<(), StorageError> {
        nope()
    }
    fn insert_process(&mut self, _: NewProcess) -> Result<StoredProcess, StorageError> {
        nope()
    }
    fn update_process(&mut self, _: &StoredProcess, _: i64) -> Result<(), StorageError> {
        nope()
    }
    fn insert_container(&mut self, _: &StoredContainer) -> Result<(), StorageError> {
        nope()
    }
    fn update_container(&mut self, _: &StoredContainer, _: i64) -> Result<(), StorageError> {
        nope()
    }
}
impl RunRead for Unsupported {
    fn agent_assignment_context(
        &self,
        _: &AssignmentId,
    ) -> Result<Option<StoredAgentAssignmentContext>, StorageError> {
        nope()
    }
    fn agent_message(&self, _: &Id) -> Result<Option<StoredAgentMessage>, StorageError> {
        nope()
    }
    fn agent_message_by_key(
        &self,
        _: &ProjectId,
        _: &ActorId,
        _: &str,
    ) -> Result<Option<StoredAgentMessage>, StorageError> {
        nope()
    }
    fn agent_messages(
        &self,
        _: &ProjectId,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredAgentMessage>, StorageError> {
        nope()
    }
    fn unhanded_direct_deliveries(
        &self,
        _: &ProjectId,
        _: i64,
        _: u16,
    ) -> Result<Vec<(StoredAgentDelivery, StoredAgentMessage)>, StorageError> {
        nope()
    }
    fn agent_message_history(
        &self,
        _: &AgentMessageHistoryFilter<'_>,
    ) -> Result<Vec<(i64, StoredAgentMessage)>, StorageError> {
        nope()
    }
    fn agent_assignment_thread(
        &self,
        _: &ProjectId,
        _: &AssignmentId,
        _: &str,
        _: bool,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<(StoredAgentMessage, Option<StoredAgentDelivery>)>, StorageError> {
        nope()
    }
    fn agent_delivery(
        &self,
        _: &Id,
        _: Option<&AssignmentId>,
    ) -> Result<Option<StoredAgentDelivery>, StorageError> {
        nope()
    }
    fn agent_session_delivery(
        &self,
        _: &Id,
        _: &Id,
    ) -> Result<Option<StoredAgentDelivery>, StorageError> {
        nope()
    }
    fn agent_recipient_count(&self, _: &Id) -> Result<usize, StorageError> {
        nope()
    }
    fn agent_session_activity_at(&self, _: &Id) -> Result<Option<i64>, StorageError> {
        nope()
    }
    fn open_blocking_question(&self, _: &RootId) -> Result<bool, StorageError> {
        nope()
    }
    fn agent_inbox(
        &self,
        _: &ProjectId,
        _: Option<&AssignmentId>,
        _: i64,
        _: bool,
        _: u16,
    ) -> Result<Vec<(StoredAgentDelivery, StoredAgentMessage)>, StorageError> {
        nope()
    }
    fn agent_session_inbox(
        &self,
        _: &ProjectId,
        _: &Id,
        _: i64,
        _: bool,
        _: u16,
    ) -> Result<Vec<(StoredAgentDelivery, StoredAgentMessage)>, StorageError> {
        nope()
    }
    fn agent_delivery_head(&self) -> Result<i64, StorageError> {
        nope()
    }
    fn agent_message_events(
        &self,
        _: &ProjectId,
        _: i64,
        _: u16,
    ) -> Result<Vec<StoredAgentMessageEvent>, StorageError> {
        nope()
    }
    fn agent_event_head(&self) -> Result<i64, StorageError> {
        nope()
    }
    fn inference_quiescent(&self, _root: &RootId) -> Result<bool, StorageError> {
        nope()
    }
    fn assignment_handoff_quiescent(&self, _: &AssignmentId) -> Result<bool, StorageError> {
        nope()
    }
    fn work_inherited_requirements(&self, _: &RootId) -> Result<Vec<Id>, StorageError> {
        nope()
    }
    fn live_assignment_for_work(
        &self,
        _: &RootId,
    ) -> Result<Option<StoredAssignment>, StorageError> {
        nope()
    }
    fn root_control(&self, _: &RootId) -> Result<Option<StoredRootControl>, StorageError> {
        nope()
    }
    fn pending_stops(
        &self,
        _: Option<&RootId>,
        _: u16,
    ) -> Result<Vec<StoredRootControl>, StorageError> {
        nope()
    }
    fn activation(&self, _: &Id) -> Result<Option<StoredActivation>, StorageError> {
        nope()
    }
    fn live_activation(&self, _: &RootId) -> Result<Option<StoredActivation>, StorageError> {
        nope()
    }
    fn activations(
        &self,
        _: &ActivationFilter,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredActivation>, StorageError> {
        nope()
    }
    fn blocker(&self, _: &Id) -> Result<Option<StoredBlocker>, StorageError> {
        nope()
    }
    fn blockers(
        &self,
        _: &BlockerFilter,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredBlocker>, StorageError> {
        nope()
    }
    fn assignment(&self, _: &AssignmentId) -> Result<Option<StoredAssignment>, StorageError> {
        nope()
    }
    fn assignments(
        &self,
        _: &RootId,
        _: bool,
        _: Option<&AssignmentId>,
        _: u16,
    ) -> Result<Vec<StoredAssignment>, StorageError> {
        nope()
    }
    fn instruction(&self, _: &Id) -> Result<Option<StoredInstruction>, StorageError> {
        nope()
    }
    fn instructions(
        &self,
        _: &RootId,
        _: i64,
        _: u16,
    ) -> Result<Vec<StoredInstruction>, StorageError> {
        nope()
    }
    fn session_turn(&self, _: &Id) -> Result<Option<StoredTurn>, StorageError> {
        nope()
    }
    fn turns_prior_boots(&self, _: &Id, _: u16) -> Result<Vec<StoredTurn>, StorageError> {
        nope()
    }
    fn feature_plan(&self, _: &RootId) -> Result<Option<StoredPlanState>, StorageError> {
        nope()
    }
    fn work_parent(&self, _: &RootId) -> Result<Option<Option<RootId>>, StorageError> {
        nope()
    }
    fn work_repository_ids(&self, _: &RootId) -> Result<Vec<Id>, StorageError> {
        nope()
    }
    fn open_prerequisites(&self, _: &RootId) -> Result<Vec<RootId>, StorageError> {
        nope()
    }
    fn project_lifecycle(&self, _: &ProjectId) -> Result<Option<String>, StorageError> {
        nope()
    }
    fn project_roots(
        &self,
        _: &ProjectId,
        _: Option<&RootId>,
        _: u16,
    ) -> Result<Vec<RootId>, StorageError> {
        nope()
    }
    fn root_deliveries(&self, _: &RootId, _: Option<&Id>, _: u16) -> Result<Vec<Id>, StorageError> {
        nope()
    }
    fn stop_coverage(
        &self,
        _: &OperationId,
        _: Option<&RootId>,
        _: u16,
    ) -> Result<Vec<StoredStopCoverage>, StorageError> {
        nope()
    }
}
impl RunRepository for Unsupported {
    fn insert_agent_assignment_context(
        &mut self,
        _: &StoredAgentAssignmentContext,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn insert_agent_message(&mut self, _: &StoredAgentMessage) -> Result<(), StorageError> {
        nope()
    }
    fn resolve_agent_message(
        &mut self,
        _: &StoredAgentMessage,
        _: i64,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn insert_agent_delivery(&mut self, _: &StoredAgentDelivery) -> Result<(), StorageError> {
        nope()
    }
    fn update_agent_delivery(
        &mut self,
        _: &StoredAgentDelivery,
        _: i64,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn insert_agent_message_event(
        &mut self,
        _: &StoredAgentMessageEvent,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn reparent_assignment(
        &mut self,
        _: &StoredAssignment,
        _: i64,
        _: i64,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn retire_membership_handoff(&mut self, _: &AssignmentId, _: i64) -> Result<(), StorageError> {
        nope()
    }
    fn insert_root_control(&mut self, _: &StoredRootControl) -> Result<(), StorageError> {
        nope()
    }
    fn update_root_control(&mut self, _: &StoredRootControl, _: i64) -> Result<(), StorageError> {
        nope()
    }
    fn insert_activation(&mut self, _: &StoredActivation) -> Result<(), StorageError> {
        nope()
    }
    fn update_activation(&mut self, _: &StoredActivation, _: i64) -> Result<(), StorageError> {
        nope()
    }
    fn insert_blocker(&mut self, _: &StoredBlocker) -> Result<(), StorageError> {
        nope()
    }
    fn update_blocker(&mut self, _: &StoredBlocker, _: i64) -> Result<(), StorageError> {
        nope()
    }
    fn insert_assignment(&mut self, _: &StoredAssignment) -> Result<(), StorageError> {
        nope()
    }
    fn update_assignment(&mut self, _: &StoredAssignment, _: i64) -> Result<(), StorageError> {
        nope()
    }
    fn insert_instruction(&mut self, _: &StoredInstruction) -> Result<(), StorageError> {
        nope()
    }
    fn update_instruction(&mut self, _: &StoredInstruction, _: i64) -> Result<(), StorageError> {
        nope()
    }
    fn insert_turn(&mut self, _: &StoredTurn) -> Result<(), StorageError> {
        nope()
    }
    fn delete_turn(&mut self, _: &Id, _: &Id) -> Result<(), StorageError> {
        nope()
    }
    fn cover_stop(&mut self, _: &StoredStopCoverage) -> Result<(), StorageError> {
        nope()
    }
}

impl KnowledgeRead for Unsupported {
    fn knowledge_review_candidates(
        &self,
        _: &[String],
        _: KnowledgeReviewKind,
        _: u16,
    ) -> Result<Vec<StoredClaim>, StorageError> {
        nope()
    }
    fn knowledge_review_contradictions(
        &self,
        _: &[String],
        _: u16,
    ) -> Result<Vec<StoredRelation>, StorageError> {
        nope()
    }
    fn knowledge_scope_revision(&self, _: &KnowledgeScope) -> Result<i64, StorageError> {
        nope()
    }
    fn knowledge_entity(&self, _: &Id) -> Result<Option<StoredEntity>, StorageError> {
        nope()
    }
    fn knowledge_entity_by_key(
        &self,
        _: &KnowledgeScope,
        _: &str,
    ) -> Result<Option<StoredEntity>, StorageError> {
        nope()
    }
    fn knowledge_claim(&self, _: &Id) -> Result<Option<StoredClaim>, StorageError> {
        nope()
    }
    fn knowledge_claims(
        &self,
        _: &ClaimQuery,
        _: Option<(i64, &str)>,
        _: u16,
    ) -> Result<Vec<StoredClaim>, StorageError> {
        nope()
    }
    fn knowledge_claims_about(
        &self,
        _: &[String],
        _: &Id,
        _: &str,
        _: u16,
    ) -> Result<Vec<StoredClaim>, StorageError> {
        nope()
    }
    fn knowledge_entity_claims(
        &self,
        _: &Id,
        _: &[String],
        _: u16,
    ) -> Result<Vec<StoredClaim>, StorageError> {
        nope()
    }
    fn knowledge_entity_edges(
        &self,
        _: &Id,
        _: &[String],
        _: u16,
    ) -> Result<Vec<StoredClaim>, StorageError> {
        nope()
    }
    fn knowledge_entities(
        &self,
        _: &[String],
        _: Option<(i64, &str)>,
        _: u16,
    ) -> Result<Vec<StoredEntity>, StorageError> {
        nope()
    }
    fn knowledge_entity_users(&self, _: &Id, _: u16) -> Result<Vec<StoredClaim>, StorageError> {
        nope()
    }
    fn knowledge_evidence(&self, _: &Id) -> Result<Option<StoredEvidence>, StorageError> {
        nope()
    }
    fn knowledge_evidence_users(&self, _: &Id, _: u16) -> Result<Vec<Id>, StorageError> {
        nope()
    }
    fn knowledge_relations(
        &self,
        _: &Id,
        _: &[String],
        _: u16,
    ) -> Result<Vec<StoredRelation>, StorageError> {
        nope()
    }
    fn knowledge_history(&self, _: &Id, _: u16) -> Result<Vec<StoredHistory>, StorageError> {
        nope()
    }
    fn knowledge_history_at(&self, _: &Id, _: i64) -> Result<Option<StoredHistory>, StorageError> {
        nope()
    }
    fn knowledge_write(&self, _: &Id) -> Result<Option<StoredWrite>, StorageError> {
        nope()
    }
    fn knowledge_recent_writes(
        &self,
        _: &KnowledgeScope,
        _: u16,
    ) -> Result<Vec<StoredWrite>, StorageError> {
        nope()
    }
    fn knowledge_lesson_event_count(&self, _: &Id) -> Result<i64, StorageError> {
        nope()
    }
    fn knowledge_lesson_events(
        &self,
        _: &Id,
        _: u16,
    ) -> Result<Vec<StoredLessonEvent>, StorageError> {
        nope()
    }
    fn knowledge_merged_events(
        &self,
        _: &Id,
        _: u16,
    ) -> Result<Vec<StoredLessonEvent>, StorageError> {
        nope()
    }
    fn knowledge_challenges(&self, _: &Id, _: u16) -> Result<Vec<StoredChallenge>, StorageError> {
        nope()
    }
    fn knowledge_challenge(&self, _: &Id) -> Result<Option<StoredChallenge>, StorageError> {
        nope()
    }
    fn knowledge_open_challenge_records(
        &self,
        _: &Id,
        _: u16,
    ) -> Result<Vec<StoredChallenge>, StorageError> {
        nope()
    }
    fn knowledge_open_challenges(&self, _: &Id) -> Result<i64, StorageError> {
        nope()
    }
    fn knowledge_addresses(&self, _: &Id) -> Result<Vec<StoredAddress>, StorageError> {
        nope()
    }
    fn knowledge_deliveries(&self, _: &Id) -> Result<DeliverySummary, StorageError> {
        nope()
    }
    fn knowledge_claim_stale(&self, _: &Id) -> Result<bool, StorageError> {
        nope()
    }
    fn knowledge_source_head(&self, _: &str) -> Result<Option<String>, StorageError> {
        nope()
    }
    fn knowledge_source_claims(
        &self,
        _: &KnowledgeScope,
        _: &str,
        _: Option<&str>,
        _: u16,
    ) -> Result<Vec<StoredClaim>, StorageError> {
        nope()
    }
    fn knowledge_source_entities(
        &self,
        _: &KnowledgeScope,
        _: &str,
        _: Option<&str>,
        _: u16,
    ) -> Result<Vec<StoredEntity>, StorageError> {
        nope()
    }
    fn knowledge_source_deletion(
        &self,
        _: &Id,
    ) -> Result<Option<StoredSourceDeletion>, StorageError> {
        nope()
    }
    fn knowledge_running_source_deletion(
        &self,
        _: &KnowledgeScope,
        _: Option<&str>,
    ) -> Result<Option<Id>, StorageError> {
        nope()
    }
    fn knowledge_relations_touching(&self, _: &[Id]) -> Result<Vec<StoredRelation>, StorageError> {
        nope()
    }
    fn knowledge_entity_used_beyond(
        &self,
        _: &Id,
        _: &DeletedClaims<'_>,
    ) -> Result<bool, StorageError> {
        nope()
    }
    fn knowledge_claim_referrer_outside(
        &self,
        _: &Id,
        _: &[Id],
        _: &KnowledgeScope,
    ) -> Result<Option<Id>, StorageError> {
        nope()
    }
    fn knowledge_evidence_cited_beyond(
        &self,
        _: &Id,
        _: &DeletedClaims<'_>,
    ) -> Result<bool, StorageError> {
        nope()
    }
    fn knowledge_counts(&self, _: &[String]) -> Result<KnowledgeCounts, StorageError> {
        nope()
    }
    fn knowledge_imported(&self, _: &Id, _: i64) -> Result<Option<Id>, StorageError> {
        nope()
    }
}
impl KnowledgeRepository for Unsupported {
    fn bump_scope(&mut self, _: &KnowledgeScope) -> Result<i64, StorageError> {
        nope()
    }
    fn put_knowledge_entity(
        &mut self,
        _: &StoredEntity,
        _: Option<i64>,
    ) -> Result<StoredEntity, StorageError> {
        nope()
    }
    fn put_knowledge_claim(
        &mut self,
        _: &StoredClaim,
        _: Option<i64>,
    ) -> Result<StoredClaim, StorageError> {
        nope()
    }
    fn put_knowledge_evidence(&mut self, _: &StoredEvidence, _: bool) -> Result<(), StorageError> {
        nope()
    }
    fn put_knowledge_relation(&mut self, _: &StoredRelation) -> Result<(), StorageError> {
        nope()
    }
    fn remove_knowledge_relation(&mut self, _: &Id, _: i64) -> Result<bool, StorageError> {
        nope()
    }
    fn set_knowledge_relation_ownership(
        &mut self,
        _: &Id,
        _: &[Id],
        _: &[(String, String)],
    ) -> Result<bool, StorageError> {
        nope()
    }
    fn put_knowledge_history(&mut self, _: &StoredHistory) -> Result<(), StorageError> {
        nope()
    }
    fn purge_knowledge_history(
        &mut self,
        _: &Id,
        _: &dyn Fn(&serde_json::Value) -> serde_json::Value,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn put_knowledge_write(&mut self, _: &StoredWrite) -> Result<(), StorageError> {
        nope()
    }
    fn extend_knowledge_write(
        &mut self,
        _: &Id,
        _: &[Id],
        _: &[Id],
        _: i64,
        _: &[serde_json::Value],
    ) -> Result<(), StorageError> {
        nope()
    }
    fn put_knowledge_source_deletion(
        &mut self,
        _: &StoredSourceDeletion,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn rescope_knowledge_relation(
        &mut self,
        _: &Id,
        _: &KnowledgeScope,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn add_knowledge_lesson_event(&mut self, _: &StoredLessonEvent) -> Result<bool, StorageError> {
        nope()
    }
    fn merge_knowledge_lesson_events(&mut self, _: &Id, _: &Id) -> Result<i64, StorageError> {
        nope()
    }
    fn unmerge_knowledge_lesson_events(&mut self, _: &Id) -> Result<i64, StorageError> {
        nope()
    }
    fn settle_knowledge_lesson_events(
        &mut self,
        _: &Id,
        _: &Id,
        _: &[Id],
    ) -> Result<i64, StorageError> {
        nope()
    }
    fn restore_merged_lesson_events(&mut self, _: &Id, _: &Id) -> Result<i64, StorageError> {
        nope()
    }
    fn refill_knowledge_lesson_events(
        &mut self,
        _: &Id,
        _: &[Vec<Id>],
    ) -> Result<i64, StorageError> {
        nope()
    }
    fn put_knowledge_challenge(&mut self, _: &StoredChallenge) -> Result<(), StorageError> {
        nope()
    }
    fn put_knowledge_address(&mut self, _: &StoredAddress) -> Result<(), StorageError> {
        nope()
    }
    fn record_knowledge_delivery(
        &mut self,
        _: &Id,
        _: &Id,
        _: i64,
        _: i64,
    ) -> Result<bool, StorageError> {
        nope()
    }
    fn remove_knowledge_relations_of(&mut self, _: &Id, _: i64) -> Result<i64, StorageError> {
        nope()
    }
    fn observe_knowledge_source(
        &mut self,
        _: &str,
        _: &str,
        _: i64,
    ) -> Result<Vec<(Id, KnowledgeScope)>, StorageError> {
        nope()
    }
    fn purge_knowledge_lesson_records(&mut self, _: &Id) -> Result<(), StorageError> {
        nope()
    }
    fn remove_live_knowledge_relation(
        &mut self,
        _: &KnowledgeScope,
        _: &Id,
        _: &str,
        _: &Id,
        _: i64,
    ) -> Result<Option<StoredRelation>, StorageError> {
        nope()
    }
    fn map_knowledge_import(&mut self, _: &Id, _: i64, _: &Id, _: &Id) -> Result<(), StorageError> {
        nope()
    }
}

impl GitRead for Unsupported {
    fn git_worktree_for_branch(
        &self,
        _: &Id,
        _: &str,
    ) -> Result<Option<StoredGitWorktree>, StorageError> {
        Ok(None)
    }
    fn git_pending_worktrees(
        &self,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredGitWorktree>, StorageError> {
        Ok(vec![])
    }
    fn git_worktree(&self, _: &Id) -> Result<Option<StoredGitWorktree>, StorageError> {
        Ok(None)
    }
    fn git_worktrees(
        &self,
        _: &Id,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredGitWorktree>, StorageError> {
        Ok(vec![])
    }
}
impl GitRepository for Unsupported {
    fn insert_git_worktree(&mut self, _: &StoredGitWorktree) -> Result<(), StorageError> {
        Err(StorageError::InvalidRecord(
            "Git test store not configured".into(),
        ))
    }
    fn update_git_worktree(&mut self, _: &StoredGitWorktree, _: i64) -> Result<(), StorageError> {
        Err(StorageError::InvalidRecord(
            "Git test store not configured".into(),
        ))
    }
}

impl EvalRead for Unsupported {
    fn eval_run(&self, _: &Id) -> Result<Option<StoredEvalRun>, StorageError> {
        nope()
    }
    fn eval_runs(
        &self,
        _: &Id,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredEvalRun>, StorageError> {
        nope()
    }
}
impl EvalRepository for Unsupported {
    fn insert_eval_run(&mut self, _: &StoredEvalRun) -> Result<(), StorageError> {
        nope()
    }
}

impl WatcherRead for Unsupported {
    fn watcher(&self, _: &Id) -> Result<Option<StoredWatcher>, StorageError> {
        nope()
    }
    fn watcher_for_process(&self, _: &OperationId) -> Result<Option<StoredWatcher>, StorageError> {
        nope()
    }
    fn watchers(
        &self,
        _: &WatcherFilter,
        _: &[WatcherPhase],
        _: Option<(i64, &Id)>,
        _: u16,
    ) -> Result<Vec<StoredWatcher>, StorageError> {
        nope()
    }
    fn pending_watch_events(
        &self,
        _: Option<(i64, &Id)>,
        _: u16,
    ) -> Result<Vec<StoredWatchEvent>, StorageError> {
        nope()
    }
    fn open_watchers(&self, _: Option<&Id>, _: u16) -> Result<Vec<StoredWatcher>, StorageError> {
        nope()
    }
    fn watch_event(&self, _: &Id) -> Result<Option<StoredWatchEvent>, StorageError> {
        nope()
    }
    fn watch_event_for_watcher(&self, _: &Id) -> Result<Option<StoredWatchEvent>, StorageError> {
        nope()
    }
    fn watch_events(
        &self,
        _: &Id,
        _: &[WatchEventState],
        _: u16,
    ) -> Result<Vec<StoredWatchEvent>, StorageError> {
        nope()
    }
}
impl WatcherRepository for Unsupported {
    fn insert_watcher(&mut self, _: &StoredWatcher) -> Result<(), StorageError> {
        nope()
    }
    fn update_watcher(&mut self, _: &StoredWatcher) -> Result<(), StorageError> {
        nope()
    }
    fn insert_watch_event(&mut self, _: &StoredWatchEvent) -> Result<(), StorageError> {
        nope()
    }
    fn deliver_watch_event(&mut self, _: &Id, _: i64, _: i64) -> Result<bool, StorageError> {
        nope()
    }
    fn carry_watch_events(&mut self, _: &Id, _: &Id) -> Result<u64, StorageError> {
        nope()
    }
    fn release_watch_events(
        &mut self,
        _: &Id,
        _: &Id,
        _: bool,
        _: i64,
    ) -> Result<u64, StorageError> {
        nope()
    }
}

impl DecisionRead for Unsupported {
    fn os_notification_delivery(
        &self,
        _: &ActorId,
        _: &Id,
    ) -> Result<Option<StoredOsNotificationDelivery>, StorageError> {
        nope()
    }
    fn notification_users(&self) -> Result<Vec<ActorId>, StorageError> {
        nope()
    }
    fn list_approval_sources(
        &self,
        _: &ApprovalSourceFilter,
        _: Option<&ApprovalSourcePosition>,
        _: u16,
    ) -> Result<Vec<ApprovalSourceRef>, StorageError> {
        nope()
    }
    fn approval(&self, _: &Id) -> Result<Option<StoredApproval>, StorageError> {
        nope()
    }
    fn human_decision(
        &self,
        _: ApprovalSource,
        _: &Id,
    ) -> Result<Option<StoredHumanDecision>, StorageError> {
        nope()
    }
    fn approval_resume(&self, _: &Id) -> Result<Option<StoredApprovalResume>, StorageError> {
        nope()
    }
    fn pending_approval_resumes(
        &self,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredApprovalResume>, StorageError> {
        nope()
    }
    fn notification(
        &self,
        _: &ActorId,
        _: &Id,
    ) -> Result<Option<StoredNotification>, StorageError> {
        nope()
    }
    fn notification_by_source(
        &self,
        _: &ActorId,
        _: &str,
    ) -> Result<Option<StoredNotification>, StorageError> {
        nope()
    }
    fn notifications(
        &self,
        _: &NotificationFilter,
        _: Option<&NotificationPosition>,
        _: u16,
    ) -> Result<Vec<StoredNotification>, StorageError> {
        nope()
    }
}
impl DecisionRepository for Unsupported {
    fn claim_os_notification_delivery(
        &mut self,
        _: &ActorId,
        _: &Id,
        _: Option<i64>,
        _: bool,
    ) -> Result<StoredOsNotificationDelivery, StorageError> {
        nope()
    }
    fn settle_os_notification_delivery(
        &mut self,
        _: &ActorId,
        _: &Id,
        _: &OsNotificationDeliveryFence,
        _: &str,
        _: Option<&str>,
    ) -> Result<StoredOsNotificationDelivery, StorageError> {
        nope()
    }
    fn recover_os_notification_deliveries(&mut self) -> Result<u64, StorageError> {
        nope()
    }
    fn bind_notification_user(&mut self, _: &ActorId) -> Result<(), StorageError> {
        nope()
    }
    fn put_approval(&mut self, _: &StoredApproval, _: Option<i64>) -> Result<(), StorageError> {
        nope()
    }
    fn insert_human_decision(&mut self, _: &StoredHumanDecision) -> Result<(), StorageError> {
        nope()
    }
    fn put_approval_resume(
        &mut self,
        _: &StoredApprovalResume,
        _: Option<i64>,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn put_notification(
        &mut self,
        _: &StoredNotification,
        _: Option<i64>,
    ) -> Result<(), StorageError> {
        nope()
    }
}

impl UsageRead for Unsupported {
    fn inference_attempt(&self, _id: &Id) -> Result<Option<StoredInferenceAttempt>, StorageError> {
        nope()
    }
    fn inference_attempts(
        &self,
        _filter: &UsageAttemptFilter,
        _after: Option<&Id>,
        _limit: u16,
    ) -> Result<Vec<StoredInferenceAttempt>, StorageError> {
        nope()
    }
    fn usage_observation(
        &self,
        _attempt: &Id,
        _key: &str,
    ) -> Result<Option<StoredUsageObservation>, StorageError> {
        nope()
    }
    fn usage_observations(
        &self,
        _attempt: &Id,
        _after_sequence: Option<i64>,
        _limit: u16,
    ) -> Result<Vec<StoredUsageObservation>, StorageError> {
        nope()
    }
    fn attempt_balance(&self, _attempt: &Id) -> Result<Option<StoredAttemptBalance>, StorageError> {
        nope()
    }
    fn feature_budget(&self, _feature: &Id) -> Result<Option<StoredFeatureBudget>, StorageError> {
        nope()
    }
    fn feature_usage_totals(
        &self,
        _feature: &Id,
    ) -> Result<StoredFeatureUsageTotals, StorageError> {
        nope()
    }
    fn usage_origins(
        &self,
        _filter: &UsageOriginFilter,
        _after: Option<&str>,
        _limit: u16,
    ) -> Result<Vec<StoredUsageOrigin>, StorageError> {
        nope()
    }
    fn usage_origin(
        &self,
        _kind: &str,
        _id: &str,
    ) -> Result<Option<StoredUsageOrigin>, StorageError> {
        nope()
    }
    fn usage_changes(
        &self,
        _feature: Option<&Id>,
        _after: Option<&Id>,
        _limit: u16,
    ) -> Result<Vec<StoredUsageChange>, StorageError> {
        nope()
    }
    fn usage_bootstrap(&self) -> Result<StoredUsageBootstrap, StorageError> {
        nope()
    }
    fn price_revision(&self) -> Result<i64, StorageError> {
        nope()
    }
    fn price_snapshots(
        &self,
        _provider: Option<&str>,
        _after: Option<&Id>,
        _limit: u16,
    ) -> Result<Vec<StoredPriceSnapshot>, StorageError> {
        nope()
    }
    fn price_snapshot(&self, _id: &Id) -> Result<Option<StoredPriceSnapshot>, StorageError> {
        nope()
    }
    fn current_price_snapshot(
        &self,
        _provider: &str,
    ) -> Result<Option<StoredPriceSnapshot>, StorageError> {
        nope()
    }
    fn price_status(&self, _provider: &str) -> Result<Option<StoredPriceStatus>, StorageError> {
        nope()
    }
    fn price_override_history(
        &self,
        _model: Option<&Id>,
        _after: Option<&PriceOverridePosition>,
        _limit: u16,
    ) -> Result<Vec<StoredPriceOverride>, StorageError> {
        nope()
    }
    fn price_override(&self, _model: &Id) -> Result<Option<StoredPriceOverride>, StorageError> {
        nope()
    }
    fn price_override_revision(&self, _model: &Id) -> Result<i64, StorageError> {
        nope()
    }
}

impl UsageRepository for Unsupported {
    fn insert_inference_attempt(
        &mut self,
        _attempt: &StoredInferenceAttempt,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn update_inference_attempt(
        &mut self,
        _attempt: &StoredInferenceAttempt,
        _expected: i64,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn insert_usage_observation(
        &mut self,
        _observation: &StoredUsageObservation,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn put_attempt_balance(
        &mut self,
        _balance: &StoredAttemptBalance,
        _expected: Option<i64>,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn put_feature_budget(
        &mut self,
        _budget: &StoredFeatureBudget,
        _expected: Option<i64>,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn insert_usage_origin(&mut self, _origin: &StoredUsageOrigin) -> Result<(), StorageError> {
        nope()
    }
    fn bind_usage_origin(
        &mut self,
        _kind: &str,
        _id: &str,
        _attempt: &Id,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn append_usage_change(&mut self, _change: &StoredUsageChange) -> Result<(), StorageError> {
        nope()
    }
    fn update_usage_bootstrap(
        &mut self,
        _bootstrap: &StoredUsageBootstrap,
        _expected: i64,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn advance_price_revision(&mut self, _expected: i64) -> Result<i64, StorageError> {
        nope()
    }
    fn put_price_status(
        &mut self,
        _status: &StoredPriceStatus,
        _expected: Option<i64>,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn publish_price_snapshot(
        &mut self,
        _snapshot: &StoredPriceSnapshot,
        _expected_status_revision: i64,
    ) -> Result<StoredPriceStatus, StorageError> {
        nope()
    }
    fn put_price_override(
        &mut self,
        _override_: &StoredPriceOverride,
        _expected: i64,
    ) -> Result<(), StorageError> {
        nope()
    }
}

impl HelpRead for Unsupported {
    fn help_generation_operation(&self, _: &Id) -> Result<Option<OperationId>, StorageError> {
        nope()
    }
    fn help_head(&self) -> Result<StoredHelpHead, StorageError> {
        nope()
    }
    fn help_drafts(&self, _: Option<&Id>, _: u16) -> Result<Vec<StoredHelpDraft>, StorageError> {
        nope()
    }
    fn help_topic(&self, _: &Id, _: &Id) -> Result<Option<StoredHelpTopic>, StorageError> {
        nope()
    }
    fn help_topics(
        &self,
        _: &Id,
        _: &HelpFilter,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredHelpTopic>, StorageError> {
        nope()
    }
    fn help_content(&self, _: &str) -> Result<Option<StoredHelpContent>, StorageError> {
        nope()
    }
    fn help_content_reference(&self, _: &str) -> Result<Option<serde_json::Value>, StorageError> {
        nope()
    }
    fn help_passage_source(
        &self,
        _: &Id,
        _: &Id,
        _: i64,
        _: u32,
    ) -> Result<Option<StoredHelpPassage>, StorageError> {
        nope()
    }
}
impl InstructionRead for Unsupported {
    fn instruction_item(&self, _: &Id) -> Result<Option<StoredInstructionItem>, StorageError> {
        nope()
    }
    fn instruction_items(
        &self,
        _: &InstructionFilter,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredInstructionItem>, StorageError> {
        nope()
    }
    fn instruction_revision(
        &self,
        _: &Id,
        _: i64,
    ) -> Result<Option<StoredInstructionRevision>, StorageError> {
        nope()
    }
    fn instruction_revisions(
        &self,
        _: &Id,
        _: Option<i64>,
        _: u16,
    ) -> Result<Vec<StoredInstructionRevision>, StorageError> {
        nope()
    }
    fn instruction_revision_by_approval(
        &self,
        _: &Id,
    ) -> Result<Option<StoredInstructionRevision>, StorageError> {
        nope()
    }
}
impl InstructionRepository for Unsupported {
    fn insert_instruction_item(&mut self, _: &StoredInstructionItem) -> Result<(), StorageError> {
        nope()
    }
    fn update_instruction_item(&mut self, _: &StoredInstructionItem) -> Result<(), StorageError> {
        nope()
    }
    fn insert_instruction_revision(
        &mut self,
        _: &StoredInstructionRevision,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn set_instruction_revision_state(
        &mut self,
        _: &Id,
        _: i64,
        _: InstructionRevisionState,
        _: InstructionRevisionState,
        _: Option<&Id>,
        _: Option<i64>,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn release_instruction_revision(
        &mut self,
        _: &Id,
        _: i64,
        _: &serde_json::Value,
    ) -> Result<(), StorageError> {
        nope()
    }
}
impl HelpRepository for Unsupported {
    fn set_help_generation_operation(
        &mut self,
        _: i64,
        _: i64,
        _: &OperationId,
    ) -> Result<StoredHelpHead, StorageError> {
        nope()
    }
    fn stage_help_document(
        &mut self,
        _: &StoredHelpDocument,
    ) -> Result<StoredHelpTopic, StorageError> {
        nope()
    }
    fn staged_help_topic(&self, _: &Id, _: &Id) -> Result<Option<StoredHelpTopic>, StorageError> {
        nope()
    }
    fn staged_help_topics(
        &self,
        _: &Id,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredHelpTopic>, StorageError> {
        nope()
    }
    fn publish_help_generation(
        &mut self,
        _: i64,
        _: i64,
        _: Option<&OperationId>,
        _: &StoredHelpHead,
        _: i64,
    ) -> Result<StoredHelpHead, StorageError> {
        nope()
    }
    fn discard_help_draft(
        &mut self,
        _: &Id,
        _: &OperationId,
        _: u16,
    ) -> Result<bool, StorageError> {
        nope()
    }
}

impl GraphRead for Unsupported {
    fn graph_units(
        &self,
        _: &GraphScope,
        _: Option<&GraphPosition>,
        _: u16,
    ) -> Result<GraphPage<GraphUnit>, StorageError> {
        nope()
    }
    fn graph_nodes(
        &self,
        _: &GraphScope,
        _: &GraphNodeFilter,
        _: Option<&GraphPosition>,
        _: u16,
    ) -> Result<GraphPage<GraphNode>, StorageError> {
        nope()
    }
    fn graph_node(
        &self,
        _: &GraphScope,
        _: &GraphNodeIdentity,
    ) -> Result<Option<GraphNode>, StorageError> {
        nope()
    }
    fn graph_edges(
        &self,
        _: &GraphScope,
        _: &GraphEdgeFilter,
        _: Option<&GraphPosition>,
        _: u16,
    ) -> Result<GraphPage<GraphEdge>, StorageError> {
        nope()
    }
    fn graph_counts(&self, _: &GraphScope) -> Result<GraphCounts, StorageError> {
        nope()
    }
    fn graph_kinds(
        &self,
        _: &GraphScope,
        _: GraphElementKind,
        _: Option<&str>,
        _: u16,
    ) -> Result<GraphPage<GraphKindCount>, StorageError> {
        nope()
    }
    fn graph_trust(
        &self,
        _: &GraphScope,
        _: &GraphNodeIdentity,
        _: &str,
    ) -> Result<Option<StoredGraphTrust>, StorageError> {
        nope()
    }
    fn graph_trust_revision(&self, _: &GraphScope) -> Result<i64, StorageError> {
        nope()
    }
    fn graph_retirable_views(
        &self,
        _: i64,
        _: Option<&Id>,
        _: u16,
    ) -> Result<GraphPage<StoredView>, StorageError> {
        nope()
    }
    fn graph_expired_pins(
        &self,
        _: i64,
        _: Option<&Id>,
        _: u16,
    ) -> Result<GraphPage<StoredGraphReadPin>, StorageError> {
        nope()
    }
    fn graph_pin(&self, _: &Id) -> Result<Option<StoredGraphReadPin>, StorageError> {
        nope()
    }
    fn graph_source_capture(
        &self,
        _: &Id,
    ) -> Result<Option<StoredGraphSourceCapture>, StorageError> {
        nope()
    }
}
impl GraphRepository for Unsupported {
    fn put_graph_trust(
        &mut self,
        _: &GraphScope,
        _: &StoredGraphTrust,
        _: i64,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn acquire_graph_pin(&mut self, _: &StoredGraphReadPin) -> Result<(), StorageError> {
        nope()
    }
    fn renew_graph_pin(
        &mut self,
        _: &Id,
        _: &str,
        _: i64,
        _: i64,
    ) -> Result<StoredGraphReadPin, StorageError> {
        nope()
    }
    fn retire_graph_pin(&mut self, _: &Id, _: &str, _: i64) -> Result<(), StorageError> {
        nope()
    }
    fn invalidate_graph_pins_for_view(&mut self, _: &Id) -> Result<(), StorageError> {
        nope()
    }
    fn insert_graph_source_capture(
        &mut self,
        _: &StoredGraphSourceCapture,
    ) -> Result<(), StorageError> {
        nope()
    }
}

impl GithubRead for Unsupported {
    fn github_adjudication_owner(
        &self,
        _: &Id,
        _: Option<&Id>,
    ) -> Result<Option<StoredGithubObligationRetirement>, StorageError> {
        nope()
    }
    fn github_related_work(
        &self,
        _: &GithubIssueKey,
        _: Option<&ProjectId>,
        _: Option<&RootId>,
        _: u16,
    ) -> Result<Vec<GithubWorkLink>, StorageError> {
        nope()
    }
    fn github_obligation_retirements(
        &self,
        _: &GithubIssueKey,
        _: Option<&ProjectId>,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredGithubObligationRetirement>, StorageError> {
        nope()
    }
    fn github_repository_by_remote_id(
        &self,
        _: &str,
    ) -> Result<Option<StoredGithubRepository>, StorageError> {
        nope()
    }
    fn github_repository_binding(
        &self,
        _: &Id,
    ) -> Result<Option<StoredGithubRepositoryBinding>, StorageError> {
        nope()
    }
    fn github_repository_binding_history(
        &self,
        _: &Id,
        _: Option<i64>,
        _: u16,
    ) -> Result<Vec<StoredGithubRepositoryBinding>, StorageError> {
        nope()
    }
    fn github_discovered_repository(
        &self,
        _: &Id,
    ) -> Result<Option<StoredGithubRepository>, StorageError> {
        nope()
    }
    fn github_account_repositories(
        &self,
        _: &Id,
        _: Option<&str>,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredGithubRepository>, StorageError> {
        nope()
    }
    fn github_account(&self) -> Result<Option<StoredGithubAccount>, StorageError> {
        nope()
    }
    fn github_repository(&self, _: &Id) -> Result<Option<StoredGithubRepository>, StorageError> {
        nope()
    }
    fn github_repositories(
        &self,
        _: &GithubScope,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredGithubRepository>, StorageError> {
        nope()
    }
    fn github_observation(
        &self,
        _: &Id,
        _: &GithubScope,
    ) -> Result<Option<StoredGithubObservation>, StorageError> {
        nope()
    }
    fn github_observations(
        &self,
        _: &GithubObservationFilter,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredGithubObservation>, StorageError> {
        nope()
    }
    fn github_poll_cursor(
        &self,
        _: &Id,
        _: &str,
    ) -> Result<Option<StoredGithubPollCursor>, StorageError> {
        nope()
    }
    fn github_issue(
        &self,
        _: &GithubIssueKey,
        _: &GithubScope,
    ) -> Result<Option<StoredGithubIssueIntake>, StorageError> {
        nope()
    }
    fn github_issues(
        &self,
        _: &GithubIntakeFilter,
        _: Option<&GithubIssueKey>,
        _: u16,
    ) -> Result<Vec<StoredGithubIssueIntake>, StorageError> {
        nope()
    }
    fn github_intake_clock(&self) -> Result<GithubIntakeClock, StorageError> {
        nope()
    }
    fn github_obligation_revision(&self, _: &GithubIssueKey) -> Result<i64, StorageError> {
        nope()
    }
    fn github_work_links(
        &self,
        _: &GithubIssueKey,
        _: Option<&ProjectId>,
        _: Option<&RootId>,
        _: u16,
    ) -> Result<Vec<GithubWorkLink>, StorageError> {
        nope()
    }
    fn github_resolution(&self, _: &Id) -> Result<Option<StoredGithubResolution>, StorageError> {
        nope()
    }
    fn github_resolutions(
        &self,
        _: &GithubIssueKey,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredGithubResolution>, StorageError> {
        nope()
    }
    fn github_resolution_witnesses(
        &self,
        _: &Id,
        _: Option<&ProjectId>,
        _: Option<&RootId>,
        _: u16,
    ) -> Result<Vec<StoredGithubResolutionWitness>, StorageError> {
        nope()
    }
}

impl GithubRepository for Unsupported {
    fn retire_github_work_link(
        &mut self,
        _: &StoredGithubObligationRetirement,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn put_github_repository_binding(
        &mut self,
        _: &StoredGithubRepositoryBinding,
        _: Option<i64>,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn put_github_account(
        &mut self,
        _: &StoredGithubAccount,
        _: Option<i64>,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn put_github_repository(
        &mut self,
        _: &StoredGithubRepository,
        _: Option<i64>,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn append_github_observation(
        &mut self,
        _: &StoredGithubObservation,
    ) -> Result<StoredGithubObservation, StorageError> {
        nope()
    }
    fn put_github_poll_cursor(
        &mut self,
        _: &StoredGithubPollCursor,
        _: Option<i64>,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn put_github_issue(
        &mut self,
        _: &StoredGithubIssueIntake,
        _: Option<i64>,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn set_github_intake_interval(
        &mut self,
        _: i64,
        _: i64,
    ) -> Result<GithubIntakeClock, StorageError> {
        nope()
    }
    fn admit_github_issue(
        &mut self,
        _: &GithubIssueKey,
        _: i64,
        _: i64,
        _: &OperationId,
    ) -> Result<Option<GithubAdmission>, StorageError> {
        nope()
    }
    fn link_github_work(
        &mut self,
        _: &GithubIssueKey,
        _: &RootId,
        _: i64,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn insert_github_resolution(
        &mut self,
        _: &StoredGithubResolution,
    ) -> Result<StoredGithubResolution, StorageError> {
        nope()
    }
    fn insert_github_resolution_witness(
        &mut self,
        _: &StoredGithubResolutionWitness,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn seal_github_resolution(
        &mut self,
        _: &Id,
        _: i64,
    ) -> Result<StoredGithubResolution, StorageError> {
        nope()
    }
}

impl ConventionRead for Unsupported {
    fn convention_max_sort_order(
        &self,
        _: &KnowledgeScope,
        _: Option<&Id>,
    ) -> Result<Option<i64>, StorageError> {
        nope()
    }
    fn convention(
        &self,
        _: &Id,
        _: Option<i64>,
        _: &[KnowledgeScope],
    ) -> Result<Option<StoredConvention>, StorageError> {
        nope()
    }
    fn conventions(
        &self,
        _: &ConventionQuery,
        _: Option<&ConventionPosition>,
        _: u16,
    ) -> Result<Vec<StoredConvention>, StorageError> {
        nope()
    }
    fn convention_versions(
        &self,
        _: &Id,
        _: &[KnowledgeScope],
        _: Option<i64>,
        _: u16,
    ) -> Result<Vec<StoredConvention>, StorageError> {
        nope()
    }
    fn convention_resource(
        &self,
        _: &Id,
        _: &Id,
        _: Option<i64>,
        _: &[KnowledgeScope],
    ) -> Result<Option<StoredConventionResource>, StorageError> {
        nope()
    }
    fn convention_resources(
        &self,
        _: &ConventionResourceQuery,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredConventionResource>, StorageError> {
        nope()
    }
    fn convention_scope_revision(&self, _: &KnowledgeScope) -> Result<i64, StorageError> {
        nope()
    }
    fn convention_index_source_revision(&self, _: &KnowledgeScope) -> Result<i64, StorageError> {
        nope()
    }
}

impl ConventionRepository for Unsupported {
    fn create_convention(&mut self, _: &StoredConvention) -> Result<(), StorageError> {
        nope()
    }
    fn revise_convention(&mut self, _: &StoredConvention, _: i64) -> Result<(), StorageError> {
        nope()
    }
    fn add_convention_resource(
        &mut self,
        _: &StoredConventionResource,
        _: i64,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn revise_convention_resource(
        &mut self,
        _: &StoredConventionResource,
        _: i64,
        _: i64,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn bump_convention_scope(&mut self, _: &KnowledgeScope) -> Result<i64, StorageError> {
        nope()
    }
}

impl ConventionIndexRead for Unsupported {
    fn convention_section(
        &self,
        _: &Id,
        _: &Id,
    ) -> Result<Option<StoredConventionSection>, StorageError> {
        nope()
    }
    fn convention_sections(
        &self,
        _: &Id,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredConventionSection>, StorageError> {
        nope()
    }
    fn convention_chunk(
        &self,
        _: &Id,
        _: &Id,
    ) -> Result<Option<StoredConventionChunk>, StorageError> {
        nope()
    }
    fn convention_chunk_count(&self, _: &Id) -> Result<u64, StorageError> {
        nope()
    }
    fn convention_index_status(
        &self,
        _: &Id,
    ) -> Result<Option<StoredConventionIndex>, StorageError> {
        nope()
    }
    fn convention_index_revision(&self, _: &[KnowledgeScope]) -> Result<i64, StorageError> {
        nope()
    }
    fn convention_embedding_inputs(
        &self,
        _: &Id,
    ) -> Result<Option<StoredConventionEmbeddingInputs>, StorageError> {
        nope()
    }
    fn convention_embedding_space(
        &self,
        _: &str,
    ) -> Result<Option<StoredConventionEmbeddingSpace>, StorageError> {
        nope()
    }
    fn convention_index_head(&self, _: &Id) -> Result<Option<StoredConventionIndex>, StorageError> {
        nope()
    }
    fn convention_index_generation(
        &self,
        _: &Id,
    ) -> Result<Option<StoredConventionIndex>, StorageError> {
        nope()
    }
    fn convention_index_generations(
        &self,
        _: Option<&Id>,
        _: Option<&devworks_core::protocol::OperationId>,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredConventionIndex>, StorageError> {
        nope()
    }
    fn convention_chunks(
        &self,
        _: &Id,
        _: Option<i64>,
        _: u16,
    ) -> Result<Vec<StoredConventionChunk>, StorageError> {
        nope()
    }
    fn convention_lexical_statistics(
        &self,
        _: &ConventionQuery,
        _: &[String],
    ) -> Result<ConventionLexicalStatistics, StorageError> {
        nope()
    }
    fn convention_ranked_chunks(
        &self,
        _: &ConventionQuery,
        _: &[ConventionWeightedTerm],
        _: f64,
        _: Option<&str>,
        _: Option<&[f32]>,
        _: u16,
    ) -> Result<Vec<ConventionRankedChunk>, StorageError> {
        nope()
    }
}

impl ConventionIndexRepository for Unsupported {
    fn stage_convention_section(
        &mut self,
        _: &StoredConventionSection,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn stage_convention_section_terms(
        &mut self,
        _: &Id,
        _: &Id,
        _: i64,
        _: &str,
        _: &[ConventionTerm],
    ) -> Result<(), StorageError> {
        nope()
    }
    fn stage_convention_chunk_terms(
        &mut self,
        _: &Id,
        _: &Id,
        _: i64,
        _: &str,
        _: &[ConventionTerm],
    ) -> Result<(), StorageError> {
        nope()
    }
    fn insert_convention_embedding_inputs(
        &mut self,
        _: &StoredConventionEmbeddingInputs,
    ) -> Result<StoredConventionEmbeddingInputs, StorageError> {
        nope()
    }
    fn put_convention_embedding_space(
        &mut self,
        _: &StoredConventionEmbeddingSpace,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn stage_convention_index(&mut self, _: &StoredConventionIndex) -> Result<(), StorageError> {
        nope()
    }
    fn stage_convention_chunks(
        &mut self,
        _: &Id,
        _: &[StoredConventionChunk],
    ) -> Result<(), StorageError> {
        nope()
    }
    fn set_convention_index_status(
        &mut self,
        _: &Id,
        _: &devworks_core::protocol::OperationId,
        _: &str,
        _: &str,
        _: Option<&str>,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn publish_convention_index(
        &mut self,
        _: &Id,
        _: i64,
        _: Option<&Id>,
        _: i64,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn discard_convention_index_chunks(
        &mut self,
        _: &Id,
        _: &devworks_core::protocol::OperationId,
        _: u16,
    ) -> Result<bool, StorageError> {
        nope()
    }
}

impl ConventionImportRead for Unsupported {
    fn convention_import_storage_usage(
        &self,
        _: &devworks_core::protocol::OperationId,
    ) -> Result<ConventionImportStorageUsage, StorageError> {
        nope()
    }
    fn convention_import(
        &self,
        _: &devworks_core::protocol::OperationId,
    ) -> Result<Option<StoredConventionImport>, StorageError> {
        nope()
    }
    fn convention_imports_pending(
        &self,
        _: Option<&devworks_core::protocol::OperationId>,
        _: u16,
    ) -> Result<Vec<StoredConventionImport>, StorageError> {
        nope()
    }
    fn convention_import_metadata(
        &self,
        _: &devworks_core::protocol::OperationId,
    ) -> Result<Option<StoredConvention>, StorageError> {
        nope()
    }
    fn convention_import_artifacts(
        &self,
        _: &devworks_core::protocol::OperationId,
        _: Option<i64>,
        _: u16,
    ) -> Result<Vec<StoredConventionImportArtifact>, StorageError> {
        nope()
    }
}

impl ConventionImportRepository for Unsupported {
    fn begin_convention_import(
        &mut self,
        _: &StoredConventionImport,
    ) -> Result<StoredConventionImport, StorageError> {
        nope()
    }
    fn stage_convention_import_artifact(
        &mut self,
        _: &StoredConventionImportArtifact,
    ) -> Result<StoredConventionImport, StorageError> {
        nope()
    }
    fn set_convention_import_metadata(
        &mut self,
        _: &devworks_core::protocol::OperationId,
        _: &StoredConvention,
        _: Option<i64>,
    ) -> Result<StoredConventionImport, StorageError> {
        nope()
    }
    fn set_convention_import_phase(
        &mut self,
        _: &devworks_core::protocol::OperationId,
        _: i64,
        _: &str,
        _: Option<i64>,
        _: Option<&str>,
        _: Option<&serde_json::Value>,
    ) -> Result<StoredConventionImport, StorageError> {
        nope()
    }
    fn publish_convention_import(
        &mut self,
        _: &devworks_core::protocol::OperationId,
        _: i64,
        _: i64,
        _: i64,
    ) -> Result<StoredConvention, StorageError> {
        nope()
    }
    fn discard_convention_import_artifacts(
        &mut self,
        _: &devworks_core::protocol::OperationId,
        _: u16,
    ) -> Result<bool, StorageError> {
        nope()
    }
}

impl SchemaRead for Unsupported {
    fn schema_certificate(&self, _: &Id) -> Result<Option<StoredSchemaCertificate>, StorageError> {
        nope()
    }
    fn schema_pin(&self, _: &Id) -> Result<Option<StoredSchemaReadPin>, StorageError> {
        nope()
    }
    fn schema_active_pin(
        &self,
        _: &str,
        _: i64,
    ) -> Result<Option<StoredSchemaReadPin>, StorageError> {
        nope()
    }
    fn schema_credential_in_use(&self, _: &Id) -> Result<bool, StorageError> {
        nope()
    }
    fn schema_inventory_revision(&self) -> Result<i64, StorageError> {
        nope()
    }
    fn schema_source(&self, _: &Id) -> Result<Option<StoredSchemaSource>, StorageError> {
        nope()
    }
    fn schema_sources(
        &self,
        _: &SchemaVisibility,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<StoredSchemaSource>, StorageError> {
        nope()
    }
    fn schema_view(&self, _: &Id) -> Result<Option<StoredSchemaView>, StorageError> {
        nope()
    }
    fn schema_objects(
        &self,
        _: &Id,
        _: &SchemaObjectFilter,
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<devworks_core::protocol::records::SchemaObject>, StorageError> {
        nope()
    }
    fn schema_relations(
        &self,
        _: &Id,
        _: &[Id],
        _: Option<&Id>,
        _: u16,
    ) -> Result<Vec<devworks_core::protocol::records::SchemaRelation>, StorageError> {
        nope()
    }
    fn schema_publication(
        &self,
        _: &OperationId,
    ) -> Result<Option<StoredSchemaPublication>, StorageError> {
        nope()
    }
}
impl SchemaRepository for Unsupported {
    fn prune_schema_views(&mut self, _: u16) -> Result<usize, StorageError> {
        nope()
    }
    fn set_schema_certificate(
        &mut self,
        _: &Id,
        _: Option<&StoredSchemaCertificate>,
    ) -> Result<(), StorageError> {
        nope()
    }
    fn create_schema_pin(&mut self, _: &StoredSchemaReadPin) -> Result<(), StorageError> {
        nope()
    }
    fn expire_schema_pins(&mut self, _: i64, _: u16) -> Result<usize, StorageError> {
        nope()
    }
    fn create_schema_source(&mut self, _: &StoredSchemaSource) -> Result<(), StorageError> {
        nope()
    }
    fn revise_schema_source(&mut self, _: &StoredSchemaSource, _: i64) -> Result<(), StorageError> {
        nope()
    }
    fn discard_schema_staging(&mut self, _: &Id, _: u16) -> Result<bool, StorageError> {
        nope()
    }
    fn create_schema_view(&mut self, _: &StoredSchemaView) -> Result<(), StorageError> {
        nope()
    }
    fn append_schema_rows(
        &mut self,
        _: &Id,
        _: &[devworks_core::protocol::records::SchemaObject],
        _: &[devworks_core::protocol::records::SchemaRelation],
    ) -> Result<(), StorageError> {
        nope()
    }
    fn seal_schema_view(&mut self, _: &Id) -> Result<(), StorageError> {
        nope()
    }
    fn remove_schema_views(&mut self, _: &Id) -> Result<(), StorageError> {
        nope()
    }
    fn record_schema_publication(
        &mut self,
        _: &StoredSchemaPublication,
    ) -> Result<(), StorageError> {
        nope()
    }
}
