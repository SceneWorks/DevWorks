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
}

impl ConfigurationRead for Unsupported {
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
    fn workspace_usage(&self) -> Result<i64, StorageError> {
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
        _: Option<&ListPosition>,
        _: u16,
    ) -> Result<Vec<StoredArtifact>, StorageError> {
        nope()
    }
    fn shares(&self, _: &Id) -> Result<Vec<ProjectId>, StorageError> {
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
    // The in-memory store holds no work items or projects: the gateway resolves no command
    // target or lineage against it.
    fn work_root(&self, _: &RootId) -> Result<Option<WorkRoot>, StorageError> {
        Ok(None)
    }
    fn session(&self, _: &Id) -> Result<Option<StoredSession>, StorageError> {
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
}
impl SessionRepository for Unsupported {
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
    fn insert_context_source(&mut self, _: &StoredContextSource) -> Result<(), StorageError> {
        nope()
    }
    fn insert_checkpoint(&mut self, _: &StoredCheckpoint) -> Result<(), StorageError> {
        nope()
    }
    fn insert_usage(&mut self, _: &StoredUsage) -> Result<(), StorageError> {
        nope()
    }
    fn insert_link(&mut self, _: &StoredLink) -> Result<(), StorageError> {
        nope()
    }
}
