//! Reusable behavioral contract suite for [`ApplicationStore`] implementations (STORE-11/12).
//!
//! Every implementation must pass these against its real engine (temporary storage, no
//! inference, no network). Engine-specific checks (effective durability settings, migrations,
//! data-root exclusivity, scheduling) live with the adapter. Enable with the `contract-suite`
//! feature and instantiate with [`crate::store_contract_tests!`]. Failures panic.
#![allow(clippy::result_large_err)]
use super::*;
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::{AdmittedCommand, Gateway};
use crate::ports::{
    AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization, CommandHandler, Decided,
    HandlerFailure, IdSource,
};
use crate::protocol::results::DomainDecision;
use crate::protocol::{CommandReply, Rejection};
use serde_json::json;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::sync::{Barrier, Mutex};

/// A store under test at one fixed location with a controllable deterministic clock.
pub trait StoreHarness {
    /// Opens (or, after the previous store was dropped, reopens) the same store.
    fn open(&mut self) -> Arc<dyn ApplicationStore>;
    /// Sets the store clock (UTC ms) used for transaction time, leases and deadlines.
    fn set_now(&self, ms: i64);
    fn now(&self) -> i64;
}

/// Generates one `#[test]` per contract case for `$harness` (an expression producing a
/// `StoreHarness`).
#[macro_export]
macro_rules! store_contract_tests {
    ($harness:expr) => {
        $crate::store_contract_tests!(@cases $harness;
            atomic_unit_of_work, savepoints_undo_only_inner_work, snapshot_isolation,
            deletion_and_old_snapshots, restart_persists_committed_state, gateway_command_unit,
            concurrent_duplicates_decide_once, cancellation_and_deadlines, history_paging,
            actor_registry, operation_lifecycle_and_fencing, interrupted_work_after_restart,
            queue_claims_and_fencing, queue_retry_terminal_and_cancel,
            refused_decisions_persist_only_receipt_and_history,
            nested_savepoint_conflict_is_a_durable_refusal);
    };
    (@cases $harness:expr; $($case:ident),*) => {$(
        #[test]
        fn $case() {
            let mut h = $harness;
            $crate::ports::storage::contract::$case(&mut h);
        }
    )*};
}

fn boom() -> Result<(), StorageError> {
    panic!("injected panic inside a write job")
}
fn id(s: &str) -> Id {
    Id::new(s).expect("fixture id")
}
fn write<T: Send + 'static>(
    s: &dyn ApplicationStore,
    f: impl FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
) -> Result<T, StorageError> {
    run_write(s, WriteOptions::default(), f)
}
fn read<T>(
    s: &dyn ApplicationStore,
    f: impl FnOnce(&dyn ReadSnapshot) -> Result<T, StorageError>,
) -> T {
    read_snapshot(s, &ReadOptions::default(), f).expect("read snapshot")
}
fn event(
    tx: &mut dyn WriteTransaction,
    aggregate: &str,
    kind: &str,
) -> Result<(EventId, i64), StorageError> {
    let event_id = EventId(tx.new_id(IdKind::Event));
    let actor = tx.store().host_actor.clone();
    let seq = tx.history().append(NewEvent {
        id: event_id.clone(),
        actor,
        project_id: None,
        root_id: None,
        operation_id: None,
        aggregate_kind: "fixture".into(),
        aggregate_id: aggregate.into(),
        aggregate_revision: 1,
        kind: kind.into(),
        payload: json!({"fixture": true}),
    })?;
    Ok((event_id, seq))
}
fn intent(tx: &mut dyn WriteTransaction) -> Result<OperationRecord, StorageError> {
    let op = OperationId(tx.new_id(IdKind::Operation));
    tx.operations().record_intent(NewOperation {
        id: op,
        root_id: None,
        assignment_id: None,
        capability: "fixture.effect".into(),
        schema_version: 1,
        args_digest: "digest".into(),
        payload: json!({}),
    })
}
fn enqueue(
    tx: &mut dyn WriteTransaction,
    consumer: &str,
    priority: i64,
    ready_at_ms: i64,
    operation_id: Option<OperationId>,
) -> Result<QueueEntry, StorageError> {
    let (event_id, _) = event(tx, "queue-fixture", "Changed")?;
    let entry_id = tx.new_id(IdKind::QueueEntry);
    tx.queue().enqueue(NewQueueEntry {
        id: entry_id,
        consumer: consumer.into(),
        event_id,
        root_id: None,
        operation_id,
        priority,
        ready_at_ms,
        payload: json!({}),
    })
}
fn claim(
    s: &dyn ApplicationStore,
    consumer: &str,
    owner: &str,
    lease_ms: i64,
    limit: u16,
) -> Result<Vec<QueueEntry>, StorageError> {
    let req = ClaimRequest {
        consumer: consumer.into(),
        owner: id(owner),
        lease_ms,
        limit,
    };
    write(s, move |tx| tx.queue().claim_ready(&req))
}
fn kinds(s: &dyn ApplicationStore, filter: EventFilter) -> Vec<String> {
    read(s, |r| r.history().page(&filter, 0, 500))
        .events
        .into_iter()
        .map(|e| e.kind)
        .collect()
}

/// Domain changes, history and outbox commit together; any error or panic leaves nothing.
pub fn atomic_unit_of_work(h: &mut dyn StoreHarness) {
    let s = h.open();
    let (e, op, q) = write(&*s, |tx| {
        let (e, _) = event(tx, "a", "Changed")?;
        let op = intent(tx)?.id;
        let q = enqueue(tx, "fixture", 1, 0, Some(op.clone()))?.id;
        Ok((e, op, q))
    })
    .unwrap();
    read(&*s, |r| {
        assert!(r.history().event(&e)?.is_some());
        assert!(r.operations().operation(&op)?.is_some());
        assert!(r.queue().entry(&q)?.is_some());
        Ok(())
    });

    let attempted = Arc::new(Mutex::new(None));
    let seen = attempted.clone();
    let out = write(&*s, move |tx| {
        let (e, _) = event(tx, "b", "Changed")?;
        let op = intent(tx)?.id;
        let q = enqueue(tx, "fixture", 1, 0, None)?.id;
        *seen.lock().unwrap() = Some((e, op, q));
        Err::<(), _>(StorageError::InvalidRecord(
            "injected after domain update".into(),
        ))
    });
    assert!(matches!(out, Err(StorageError::InvalidRecord(_))));
    let (e2, op2, q2) = attempted.lock().unwrap().clone().unwrap();
    read(&*s, |r| {
        assert!(
            r.history().event(&e2)?.is_none(),
            "rolled-back history visible"
        );
        assert!(
            r.operations().operation(&op2)?.is_none(),
            "rolled-back domain visible"
        );
        assert!(
            r.queue().entry(&q2)?.is_none(),
            "rolled-back outbox visible"
        );
        Ok(())
    });

    let before = read(&*s, |r| Ok(r.stamp().committed_sequence));
    let panicked = write(&*s, |tx| {
        event(tx, "c", "Changed")?;
        boom()
    });
    assert!(matches!(
        panicked,
        Err(StorageError::StorageUnavailable {
            cause: Unavailable::HandlerPanicked,
            ..
        })
    ));
    assert_eq!(read(&*s, |r| Ok(r.stamp().committed_sequence)), before);
    // The write owner survives and keeps serving.
    write(&*s, |tx| event(tx, "d", "Changed")).unwrap();
}

pub fn savepoints_undo_only_inner_work(h: &mut dyn StoreHarness) {
    let s = h.open();
    let (outer, inner) = write(&*s, |tx| {
        let (outer, _) = event(tx, "outer", "Kept")?;
        let sp = tx.savepoint()?;
        let (inner, _) = event(tx, "inner", "Undone")?;
        tx.rollback_to(sp)?;
        let sp2 = tx.savepoint()?;
        event(tx, "inner2", "Released")?;
        tx.release(sp2)?;
        // Unwinding an outer savepoint discards inner ones that are still open (SQLite rules).
        let a = tx.savepoint()?;
        let b = tx.savepoint()?;
        event(tx, "nested", "Discarded")?;
        let b_depth = b.depth();
        tx.rollback_to(a)?;
        assert!(matches!(
            tx.release(Savepoint::issue(b.transaction(), b_depth)),
            Err(StorageError::InvalidRecord(_))
        ));
        let _ = b;
        // A savepoint of another transaction is refused.
        let mine = tx.savepoint()?;
        assert!(matches!(
            tx.release(Savepoint::issue(
                mine.transaction().wrapping_add(1),
                mine.depth()
            )),
            Err(StorageError::InvalidRecord(_))
        ));
        tx.release(mine)?;
        Ok((outer, inner))
    })
    .unwrap();
    // A job that leaves a savepoint open is rolled back rather than committed piecemeal.
    let left_open = write(&*s, |tx| {
        event(tx, "dangling", "NeverCommitted")?;
        let _left_open = tx.savepoint()?;
        Ok(())
    });
    assert!(matches!(left_open, Err(StorageError::InvalidRecord(_))));
    read(&*s, |r| {
        assert!(r.history().event(&outer)?.is_some());
        assert!(r.history().event(&inner)?.is_none());
        Ok(())
    });
    assert_eq!(
        kinds(&*s, EventFilter::All),
        ["Kept".to_owned(), "Released".to_owned()]
    );
}

/// Same-connection visibility, cross-connection isolation, rollback then new commit.
pub fn snapshot_isolation(h: &mut dyn StoreHarness) {
    let s = h.open();
    // Same connection: the unit sees its own uncommitted writes.
    write(&*s, |tx| {
        let before = tx.committed_sequence()?;
        let (e, seq) = event(tx, "own", "Changed")?;
        assert!(tx.history().event(&e)?.is_some());
        assert_eq!(tx.committed_sequence()?, seq);
        assert!(seq > before);
        Ok(())
    })
    .unwrap();

    // A snapshot taken before a commit on the writer's connection never sees it.
    let committed = Arc::new(Mutex::new(None));
    let store = s.clone();
    let slot = committed.clone();
    read(&*s, move |r| {
        let stamp = r.stamp().clone();
        let page = r.history().page(&EventFilter::All, 0, 500)?;
        let (e, seq) = write(&*store, |tx| event(tx, "other", "Changed")).unwrap();
        assert!(
            r.history().event(&e)?.is_none(),
            "snapshot saw a later commit"
        );
        assert_eq!(r.stamp(), &stamp);
        assert_eq!(
            r.history().page(&EventFilter::All, 0, 500)?.watermark,
            page.watermark
        );
        *slot.lock().unwrap() = Some((e, seq, stamp));
        Ok(())
    });
    let (e, seq, old) = committed.lock().unwrap().clone().unwrap();
    read(&*s, |r| {
        assert!(r.history().event(&e)?.is_some());
        assert_eq!(r.stamp().committed_sequence, seq);
        assert!(r.stamp().committed_sequence > old.committed_sequence);
        assert_eq!(r.stamp().store_id, s.info().store_id);
        Ok(())
    });

    // Rollback then a new commit: only the new commit is ever visible.
    let rolled = Arc::new(Mutex::new(None));
    let slot = rolled.clone();
    let _ = write(&*s, move |tx| {
        *slot.lock().unwrap() = Some(event(tx, "rolled", "Changed")?.0);
        Err::<(), _>(StorageError::Cancelled)
    });
    let (fresh, _) = write(&*s, |tx| event(tx, "fresh", "Changed")).unwrap();
    let rolled = rolled.lock().unwrap().clone().unwrap();
    read(&*s, |r| {
        assert!(r.history().event(&rolled)?.is_none());
        assert!(r.history().event(&fresh)?.is_some());
        Ok(())
    });
}

/// A deletion is invisible to an older snapshot and visible to a newer one.
pub fn deletion_and_old_snapshots(h: &mut dyn StoreHarness) {
    let s = h.open();
    let entry = write(&*s, |tx| enqueue(tx, "compact", 1, 0, None)).unwrap();
    let claimed = claim(&*s, "compact", "owner-a", 1_000, 1).unwrap();
    let fence = claimed[0].fence().unwrap();
    let eid = entry.id.clone();
    write(&*s, move |tx| {
        tx.queue().settle(&eid, &fence, Settlement::Completed)
    })
    .unwrap();
    let store = s.clone();
    let eid = entry.id.clone();
    read(&*s, move |r| {
        assert!(r.queue().entry(&eid)?.is_some());
        let target = eid.clone();
        write(&*store, move |tx| tx.queue().compact(&target)).unwrap();
        assert!(r.queue().entry(&eid)?.is_some(), "old snapshot lost a row");
        Ok(())
    });
    let eid = entry.id.clone();
    read(&*s, move |r| {
        assert!(r.queue().entry(&eid)?.is_none());
        Ok(())
    });
}

pub fn restart_persists_committed_state(h: &mut dyn StoreHarness) {
    let s = h.open();
    let first = s.info();
    let (e, seq) = write(&*s, |tx| event(tx, "durable", "Changed")).unwrap();
    drop(s);
    let s = h.open();
    let again = s.info();
    assert_eq!(again.store_id, first.store_id);
    assert_ne!(
        again.boot_id, first.boot_id,
        "each open records a boot epoch"
    );
    read(&*s, |r| {
        assert_eq!(r.history().event(&e)?.map(|e| e.sequence), Some(seq));
        assert_eq!(r.stamp().committed_sequence, seq);
        Ok(())
    });
}

// --- gateway on the real store ---------------------------------------------------------------

struct AllowAll;
impl AuthorityPort for AllowAll {
    fn authorize(
        &self,
        _: &InvocationContext,
        _: &AuthorityRequest<'_>,
    ) -> Result<Authorization, AuthorityDenial> {
        Ok(Authorization::direct())
    }
}
#[derive(Default)]
struct Correlations(AtomicU64);
impl IdSource for Correlations {
    fn next_id(&self, _: IdKind) -> Id {
        id(&format!("corr-{}", self.0.fetch_add(1, Ordering::SeqCst)))
    }
}

/// `runs.start`: records an operation intent, a domain event and its outbox delivery, then
/// accepts. `mode` injects failures after those writes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StartMode {
    Accept,
    FailAfterWrites,
    ConflictAfterWrites,
    RefuseAfterWrites,
    NestedConflictAfterWrites,
}
struct Start {
    mode: StartMode,
    calls: AtomicUsize,
}
impl CommandHandler for Start {
    fn decide(
        &self,
        ctx: &InvocationContext,
        _: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let op = intent(tx)?.id;
        let event_id = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: event_id.clone(),
            actor: ctx.principal().clone(),
            project_id: None,
            root_id: None,
            operation_id: Some(op.clone()),
            aggregate_kind: "root".into(),
            aggregate_id: "root-a".into(),
            aggregate_revision: 2,
            kind: "ActivationRequested".into(),
            payload: json!({}),
        })?;
        let entry = tx.new_id(IdKind::QueueEntry);
        tx.queue().enqueue(NewQueueEntry {
            id: entry,
            consumer: "supervisor".into(),
            event_id,
            root_id: None,
            operation_id: Some(op.clone()),
            priority: 1,
            ready_at_ms: 0,
            payload: json!({}),
        })?;
        if self.mode == StartMode::NestedConflictAfterWrites {
            // The use case opens its own savepoint and returns early with it still open.
            let _inner = tx.savepoint()?;
            intent(tx)?;
        }
        match self.mode {
            StartMode::RefuseAfterWrites => Ok(Decided {
                decision: DomainDecision::Refused {
                    fault: crate::protocol::DomainFault {
                        code: crate::protocol::DomainErrorCode::DependencyBlocked,
                        retry: crate::protocol::RetryAdvice::AfterBlockerClears,
                        correlation_id: crate::protocol::CorrelationId(id("corr-refused")),
                        target_id: None,
                        target_kind: None,
                        revision_dimension: None,
                        expected_revision: None,
                        current_revision: None,
                        blocker_ids: crate::protocol::support::BoundedList(vec![
                            crate::protocol::HoldId(id("hold-a")),
                        ]),
                        diagnostic_id: None,
                    },
                },
                operation_id: None,
            }),
            StartMode::Accept => Ok(Decided {
                decision: DomainDecision::Accepted {
                    operation_id: op.clone(),
                },
                operation_id: Some(op),
            }),
            StartMode::FailAfterWrites => Err(HandlerFailure::Storage(StorageError::unavailable(
                Unavailable::Io,
                "injected after domain update",
            ))),
            StartMode::ConflictAfterWrites | StartMode::NestedConflictAfterWrites => {
                Err(HandlerFailure::Storage(StorageError::conflict(
                    RevisionDimension::Control,
                    "root",
                    id("root-a"),
                    Revision::new(1).unwrap(),
                    Revision::new(2).unwrap(),
                )))
            }
        }
    }
}
fn start_body(key: &str, control: &str) -> String {
    format!(
        r#"{{"protocol":"1.0-draft.1","requestKey":"{key}","command":{{"kind":"runs.start","rootId":"root-a","expectedWorkRevision":"1","expectedControlRevision":"{control}"}}}}"#
    )
}
fn principal(s: &dyn ApplicationStore) -> InvocationContext {
    let actor = ActorId(id("user-a"));
    let registered = actor.clone();
    write(s, move |tx| {
        tx.actors()
            .register(&registered, PrincipalKind::Human, "User A")
    })
    .unwrap();
    let info = s.info();
    InvocationContext::new(
        actor,
        PrincipalKind::Human,
        id("invocation-1"),
        id("ui-main"),
        info.store_id,
        crate::protocol::Counter::new(info.restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    )
}
fn counts(s: &dyn ApplicationStore) -> (usize, usize) {
    let events = kinds(s, EventFilter::All);
    let ops = read(s, |r| r.operations().unresolved(500)).len();
    (events.len(), ops)
}

/// Receipt + CommandDecided + domain + outbox commit as one unit on the real store.
pub fn gateway_command_unit(h: &mut dyn StoreHarness) {
    let s = h.open();
    let ctx = principal(&*s);
    let ids = Correlations::default();
    let gateway = Gateway {
        authority: &AllowAll,
        store: &*s,
        ids: &ids,
    };
    let start = Arc::new(Start {
        mode: StartMode::Accept,
        calls: AtomicUsize::new(0),
    });
    let reply = gateway
        .command(
            &ctx,
            "runs.start",
            start_body("k-1", "1").as_bytes(),
            start.clone(),
        )
        .unwrap();
    let CommandReply::Recorded {
        receipt,
        replayed: false,
        ..
    } = reply
    else {
        panic!("{reply:?}")
    };
    let events = read(&*s, |r| r.history().page(&EventFilter::All, 0, 500));
    let decided = events.events.last().unwrap();
    assert_eq!(decided.kind, "CommandDecided");
    assert_eq!(receipt.committed_sequence.get(), decided.sequence);
    assert_eq!(receipt.committed_sequence.get(), events.watermark);
    let op = receipt.operation_id.clone().unwrap();
    read(&*s, |r| {
        assert!(r.operations().operation(&op)?.is_some());
        assert_eq!(r.queue().status("supervisor")?.ready, 1);
        Ok(())
    });
    let baseline = counts(&*s);

    // Replay: same intent, same key.
    let again = gateway
        .command(
            &ctx,
            "runs.start",
            start_body("k-1", "1").as_bytes(),
            start.clone(),
        )
        .unwrap();
    assert!(
        matches!(&again, CommandReply::Recorded { replayed: true, receipt: r, .. } if r.command_id == receipt.command_id)
    );
    // Changed intent under the same key.
    let changed = gateway
        .command(
            &ctx,
            "runs.start",
            start_body("k-1", "2").as_bytes(),
            start.clone(),
        )
        .unwrap();
    assert!(matches!(
        changed,
        CommandReply::Rejected {
            code: Rejection::IdempotencyConflict,
            ..
        }
    ));
    assert_eq!(start.calls.load(Ordering::SeqCst), 1);
    assert_eq!(counts(&*s), baseline);

    // An error after the domain update leaves no receipt, history, operation or outbox.
    let failing = Arc::new(Start {
        mode: StartMode::FailAfterWrites,
        calls: AtomicUsize::new(0),
    });
    let reply = gateway
        .command(
            &ctx,
            "runs.start",
            start_body("k-2", "1").as_bytes(),
            failing.clone(),
        )
        .unwrap();
    assert!(matches!(
        reply,
        CommandReply::Rejected {
            code: Rejection::StorageUnavailable,
            ..
        }
    ));
    assert_eq!(counts(&*s), baseline);
    assert_eq!(read(&*s, |r| r.queue().status("supervisor")).ready, 1);
    // The exact retry later commits normally.
    let reply = gateway
        .command(
            &ctx,
            "runs.start",
            start_body("k-2", "1").as_bytes(),
            start.clone(),
        )
        .unwrap();
    assert!(matches!(
        reply,
        CommandReply::Recorded {
            replayed: false,
            ..
        }
    ));

    // A revision conflict after partial writes: the writes are undone, the conflict is durable.
    let before = counts(&*s);
    let conflicting = Arc::new(Start {
        mode: StartMode::ConflictAfterWrites,
        calls: AtomicUsize::new(0),
    });
    let reply = gateway
        .command(
            &ctx,
            "runs.start",
            start_body("k-3", "1").as_bytes(),
            conflicting.clone(),
        )
        .unwrap();
    let CommandReply::Recorded { receipt, .. } = &reply else {
        panic!("{reply:?}")
    };
    assert!(matches!(receipt.decision, DomainDecision::Refused { .. }));
    // Only the CommandDecided event was added; no operation or outbox from the handler.
    assert_eq!(counts(&*s), (before.0 + 1, before.1));
    assert_eq!(read(&*s, |r| r.queue().status("supervisor")).ready, 2);
}

/// Identical concurrent commands: one decides; every other replays its receipt.
pub fn concurrent_duplicates_decide_once(h: &mut dyn StoreHarness) {
    let s = h.open();
    let ctx = principal(&*s);
    let start = Arc::new(Start {
        mode: StartMode::Accept,
        calls: AtomicUsize::new(0),
    });
    let barrier = Arc::new(Barrier::new(4));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let (s, ctx, start, barrier) = (s.clone(), ctx.clone(), start.clone(), barrier.clone());
            std::thread::spawn(move || {
                let ids = Correlations::default();
                let gateway = Gateway {
                    authority: &AllowAll,
                    store: &*s,
                    ids: &ids,
                };
                barrier.wait();
                gateway
                    .command(
                        &ctx,
                        "runs.start",
                        start_body("k-race", "1").as_bytes(),
                        start,
                    )
                    .unwrap()
            })
        })
        .collect();
    let replies: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(start.calls.load(Ordering::SeqCst), 1);
    let fresh = replies
        .iter()
        .filter(|r| {
            matches!(
                r,
                CommandReply::Recorded {
                    replayed: false,
                    ..
                }
            )
        })
        .count();
    let replays = replies
        .iter()
        .filter(|r| matches!(r, CommandReply::Recorded { replayed: true, .. }))
        .count();
    assert_eq!((fresh, replays), (1, 3));
    assert_eq!(read(&*s, |r| r.queue().status("supervisor")).ready, 1);
}

/// Cancellation before admission prevents dispatch; after admission it rolls back before commit;
/// after commit the result stands.
pub fn cancellation_and_deadlines(h: &mut dyn StoreHarness) {
    let s = h.open();
    let base = read(&*s, |r| Ok(r.stamp().committed_sequence));
    let token = Cancellation::new();
    token.cancel();
    let ran = Arc::new(AtomicUsize::new(0));
    let r2 = ran.clone();
    let out = run_write(
        &*s,
        WriteOptions {
            cancellation: Some(token),
            ..Default::default()
        },
        move |tx| {
            r2.fetch_add(1, Ordering::SeqCst);
            event(tx, "x", "Changed")
        },
    );
    assert!(matches!(out, Err(StorageError::Cancelled)));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "a cancelled job was dispatched"
    );

    let past = h.now() - 1;
    let out = run_write(
        &*s,
        WriteOptions {
            deadline_ms: Some(past),
            ..Default::default()
        },
        |tx| event(tx, "x", "Changed"),
    );
    assert!(matches!(out, Err(StorageError::Cancelled)));

    // Cancelled after admission, before commit: rolled back.
    let token = Cancellation::new();
    let inner = token.clone();
    let out = run_write(
        &*s,
        WriteOptions {
            cancellation: Some(token),
            ..Default::default()
        },
        move |tx| {
            let e = event(tx, "x", "Changed")?;
            inner.cancel();
            Ok(e)
        },
    );
    assert!(matches!(out, Err(StorageError::Cancelled)));
    assert_eq!(read(&*s, |r| Ok(r.stamp().committed_sequence)), base);

    // Cancelled after commit: the commit stands and is reported.
    let token = Cancellation::new();
    let (e, _) = run_write(
        &*s,
        WriteOptions {
            cancellation: Some(token.clone()),
            ..Default::default()
        },
        |tx| event(tx, "y", "Changed"),
    )
    .unwrap();
    token.cancel();
    read(&*s, |r| {
        assert!(r.history().event(&e)?.is_some());
        Ok(())
    });

    // A cancelled read is not started.
    let token = Cancellation::new();
    token.cancel();
    let out = read_snapshot(
        &*s,
        &ReadOptions {
            cancellation: Some(token),
            deadline_ms: None,
        },
        |_| Ok(()),
    );
    assert!(matches!(out, Err(StorageError::Cancelled)));
}

pub fn history_paging(h: &mut dyn StoreHarness) {
    let s = h.open();
    write(&*s, |tx| {
        for i in 0..5 {
            event(
                tx,
                if i % 2 == 0 { "even" } else { "odd" },
                &format!("E{i}"),
            )?;
        }
        Ok(())
    })
    .unwrap();
    read(&*s, |r| {
        let wm = r.stamp().committed_sequence;
        let p1 = r.history().page(&EventFilter::All, 0, 2)?;
        assert_eq!(p1.events.len(), 2);
        assert!(p1.has_more);
        assert_eq!(p1.next_after, p1.events[1].sequence);
        let p2 = r.history().page(&EventFilter::All, p1.next_after, 500)?;
        assert!(!p2.has_more);
        assert_eq!(p2.next_after, wm);
        assert!(
            p1.events
                .iter()
                .chain(&p2.events)
                .map(|e| e.sequence)
                .is_sorted()
        );
        let even = r.history().page(
            &EventFilter::Aggregate {
                kind: "fixture".into(),
                id: "even".into(),
            },
            0,
            500,
        )?;
        assert_eq!(
            even.events
                .iter()
                .map(|e| e.kind.as_str())
                .collect::<Vec<_>>(),
            ["E0", "E2", "E4"]
        );
        // Polling at the tail returns a checkpoint without replay.
        let tail = r.history().page(&EventFilter::All, wm, 10)?;
        assert!(tail.events.is_empty() && tail.next_after == wm);
        assert!(matches!(
            r.history().page(&EventFilter::All, wm + 1, 10),
            Err(StorageError::CursorExpired)
        ));
        for bad in [0, MAX_PAGE + 1] {
            assert!(matches!(
                r.history().page(&EventFilter::All, 0, bad),
                Err(StorageError::InvalidRecord(_))
            ));
        }
        Ok(())
    });
}

pub fn actor_registry(h: &mut dyn StoreHarness) {
    let s = h.open();
    write(&*s, |tx| {
        let a = ActorId(id("agent-1"));
        tx.actors().register(&a, PrincipalKind::Agent, "Agent")?;
        tx.actors().register(&a, PrincipalKind::Agent, "Agent")?;
        assert!(matches!(
            tx.actors().register(&a, PrincipalKind::Human, "Agent"),
            Err(StorageError::Duplicate { .. })
        ));
        assert_eq!(tx.actors().actor_kind(&a)?, Some(PrincipalKind::Agent));
        let event_id = EventId(tx.new_id(IdKind::Event));
        let unknown = tx.history().append(NewEvent {
            id: event_id,
            actor: ActorId(id("nobody")),
            project_id: None,
            root_id: None,
            operation_id: None,
            aggregate_kind: "fixture".into(),
            aggregate_id: "x".into(),
            aggregate_revision: 1,
            kind: "Changed".into(),
            payload: json!({}),
        });
        assert!(matches!(
            unknown,
            Err(StorageError::NotFound { kind: "actor", .. })
        ));
        Ok(())
    })
    .unwrap();
}

/// The operation journal enforces its lifecycle, revisions and attempt fencing.
pub fn operation_lifecycle_and_fencing(h: &mut dyn StoreHarness) {
    use OperationState::*;
    let s = h.open();
    write(&*s, |tx| {
        let op = intent(tx)?;
        assert_eq!((op.state, op.revision), (IntentRecorded, 1));
        let id_ = op.id.clone();
        assert!(matches!(
            tx.operations()
                .begin_attempt(&id_, 7, AttemptBinding::default()),
            Err(StorageError::RevisionConflict(_))
        ));
        let binding = AttemptBinding {
            owner_generation: Some(3),
            ..Default::default()
        };
        let a1 = tx.operations().begin_attempt(&id_, 1, binding)?;
        assert_eq!((a1.attempt_no, a1.state), (1, Preflighted));
        let stale = AttemptFence {
            attempt_no: 1,
            owner_generation: Some(2),
        };
        assert!(matches!(
            tx.operations()
                .transition(&id_, 2, Some(&stale), Dispatching, None),
            Err(StorageError::LeaseLost { .. })
        ));
        assert!(matches!(
            tx.operations()
                .transition(&id_, 2, Some(&a1.fence()), ObservedComplete, None),
            Err(StorageError::InvalidRecord(_))
        ));
        let op = tx
            .operations()
            .transition(&id_, 2, Some(&a1.fence()), Dispatching, None)?;
        assert!(matches!(
            tx.operations()
                .begin_attempt(&id_, op.revision, AttemptBinding::default()),
            Err(StorageError::InvalidRecord(_))
        ));
        let op = tx.operations().transition(
            &id_,
            op.revision,
            Some(&a1.fence()),
            OutcomeUnknown,
            Some(json!({"timeout": true})),
        )?;
        // An uncertain effect can never start another attempt directly.
        assert!(matches!(
            tx.operations()
                .begin_attempt(&id_, op.revision, AttemptBinding::default()),
            Err(StorageError::InvalidRecord(_))
        ));
        // A superseded owner's observation is retained as evidence.
        tx.operations()
            .observe(&id_, 1, json!({"late": "result"}))?;
        let op = tx.operations().transition(
            &id_,
            op.revision,
            None,
            RetryPending,
            Some(json!({"reconciled": "no effect"})),
        )?;
        let a2 = tx
            .operations()
            .begin_attempt(&id_, op.revision, AttemptBinding::default())?;
        assert_eq!(a2.attempt_no, 2);
        let op = tx.operations().operation(&id_)?.unwrap();
        assert_eq!(op.attempts[0].observations.len(), 3);
        assert_eq!(op.attempts[0].state, RetryPending);
        assert!(tx.operations().unresolved(10)?.iter().any(|o| o.id == id_));
        let op =
            tx.operations()
                .transition(&id_, op.revision, Some(&a2.fence()), Dispatching, None)?;
        let op = tx.operations().transition(
            &id_,
            op.revision,
            Some(&a2.fence()),
            ObservedComplete,
            None,
        )?;
        assert!(is_terminal(op.state));
        assert!(!tx.operations().unresolved(10)?.iter().any(|o| o.id == id_));
        Ok(())
    })
    .unwrap();
}

/// After a restart, interrupted attempts are classified and stale claims recovered without
/// treating a possibly dispatched effect as safe to replay.
pub fn interrupted_work_after_restart(h: &mut dyn StoreHarness) {
    use OperationState::*;
    let s = h.open();
    let (pre, sent, held_entry, plain_entry) = write(&*s, |tx| {
        let pre = intent(tx)?;
        tx.operations()
            .begin_attempt(&pre.id, 1, AttemptBinding::default())?;
        let sent = intent(tx)?;
        let a = tx
            .operations()
            .begin_attempt(&sent.id, 1, AttemptBinding::default())?;
        tx.operations()
            .transition(&sent.id, 2, Some(&a.fence()), Dispatching, None)?;
        let held = enqueue(tx, "restart", 1, 0, Some(sent.id.clone()))?.id;
        let plain = enqueue(tx, "restart", 1, 0, None)?.id;
        Ok((pre.id, sent.id, held, plain))
    })
    .unwrap();
    assert_eq!(
        claim(&*s, "restart", "worker-old", 60_000, 10)
            .unwrap()
            .len(),
        2
    );
    drop(s);

    let s = h.open();
    // A claim made by the previous boot is not current, even with a matching fence.
    let stale = read(&*s, |r| r.queue().entry(&plain_entry)).unwrap();
    let (sid, f) = (stale.id.clone(), stale.fence().unwrap());
    assert!(matches!(
        write(&*s, move |tx| tx.queue().renew(&sid, &f, 1_000)),
        Err(StorageError::LeaseLost { .. })
    ));
    let classified = write(&*s, |tx| tx.operations().classify_interrupted(10)).unwrap();
    assert!(classified.contains(&(pre.clone(), RetryPending)));
    assert!(classified.contains(&(sent.clone(), OutcomeUnknown)));
    let recovered = write(&*s, |tx| {
        tx.queue().recover_claims(ClaimRecovery::PriorBoots, 10)
    })
    .unwrap();
    assert_eq!(recovered.requeued, std::slice::from_ref(&plain_entry));
    assert_eq!(recovered.held, std::slice::from_ref(&held_entry));
    let held = held_entry.clone();
    assert!(matches!(
        write(&*s, move |tx| tx.queue().requeue(&held, 0)),
        Err(StorageError::InvalidRecord(_))
    ));
    // The plain delivery is claimable again under a new generation.
    let again = claim(&*s, "restart", "worker-new", 60_000, 10).unwrap();
    assert_eq!(again.len(), 1);
    // Claim (1), recovery fence (2), new claim (3).
    assert_eq!((again[0].generation, again[0].attempt_count), (3, 2));
    // After reconciliation establishes the outcome, the held delivery may proceed.
    let (sent2, held) = (sent.clone(), held_entry.clone());
    write(&*s, move |tx| {
        let op = tx.operations().operation(&sent2)?.unwrap();
        tx.operations().transition(
            &sent2,
            op.revision,
            None,
            ObservedComplete,
            Some(json!({"observed": "done"})),
        )?;
        tx.queue().requeue(&held, 0)
    })
    .unwrap();
    assert!(
        kinds(
            &*s,
            EventFilter::Aggregate {
                kind: "queue_entry".into(),
                id: held_entry.as_str().into(),
            }
        )
        .contains(&"QueueClaimRecovered".to_owned())
    );
}

pub fn queue_claims_and_fencing(h: &mut dyn StoreHarness) {
    let s = h.open();
    let now = h.now();
    let (low, high_a, high_b, future) = write(&*s, move |tx| {
        let low = enqueue(tx, "work", 1, 0, None)?.id;
        let a = enqueue(tx, "work", 5, 0, None)?.id;
        let b = enqueue(tx, "work", 5, 0, None)?.id;
        let future = enqueue(tx, "work", 9, now + 10_000, None)?.id;
        Ok((low, a, b, future))
    })
    .unwrap();
    let first = claim(&*s, "work", "owner-a", 1_000, 2).unwrap();
    // Startup recovery of earlier boots never touches a live claim of the current boot.
    let untouched = write(&*s, |tx| {
        tx.queue().recover_claims(ClaimRecovery::PriorBoots, 10)
    })
    .unwrap();
    assert_eq!(untouched, RecoveryOutcome::default());
    assert!(first.iter().all(|e| e.claim_boot_id == s.info().boot_id));
    let mut expected = vec![high_a.clone(), high_b.clone()];
    expected.sort_by(|x, y| x.as_str().cmp(y.as_str()));
    assert_eq!(
        first.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
        expected
    );
    assert!(
        first
            .iter()
            .all(|e| e.generation == 1 && e.state == QueueState::Claimed)
    );
    // The future entry is not ready; only the low-priority one remains.
    let rest = claim(&*s, "work", "owner-b", 1_000, 10).unwrap();
    assert_eq!(
        rest.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
        std::slice::from_ref(&low)
    );
    let _ = future;

    // Two claimers race for one ready entry: exactly one wins.
    let single = write(&*s, |tx| enqueue(tx, "race", 1, 0, None)).unwrap().id;
    let barrier = Arc::new(Barrier::new(2));
    let threads: Vec<_> = ["racer-1", "racer-2"]
        .into_iter()
        .map(|owner| {
            let (s, barrier) = (s.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                claim(&*s, "race", owner, 1_000, 1).unwrap()
            })
        })
        .collect();
    let won: Vec<_> = threads
        .into_iter()
        .flat_map(|t| t.join().unwrap())
        .collect();
    assert_eq!(won.len(), 1);
    assert_eq!(won[0].id, single);

    // Stale fences are rejected.
    let entry = first[0].clone();
    let fence = entry.fence().unwrap();
    let wrong_gen = QueueFence {
        owner: fence.owner.clone(),
        generation: 0,
    };
    let wrong_owner = QueueFence {
        owner: id("owner-x"),
        generation: fence.generation,
    };
    for bad in [wrong_gen, wrong_owner] {
        let eid = entry.id.clone();
        assert!(matches!(
            write(&*s, move |tx| tx.queue().renew(&eid, &bad, 1_000)),
            Err(StorageError::LeaseLost { .. })
        ));
    }
    let (eid, f) = (entry.id.clone(), fence.clone());
    let renewed = write(&*s, move |tx| tx.queue().renew(&eid, &f, 5_000)).unwrap();
    assert_eq!(renewed.lease_until_ms, Some(h.now() + 5_000));

    // Lease expiry: renewal refused, recovery requeues under a new generation, old owner fenced.
    h.set_now(h.now() + 10_000);
    let (eid, f) = (entry.id.clone(), fence.clone());
    assert!(matches!(
        write(&*s, move |tx| tx.queue().renew(&eid, &f, 1_000)),
        Err(StorageError::LeaseLost { .. })
    ));
    let recovered = write(&*s, |tx| {
        tx.queue().recover_claims(ClaimRecovery::Expired, 10)
    })
    .unwrap();
    assert!(recovered.requeued.contains(&entry.id));
    let (eid, f) = (entry.id.clone(), fence.clone());
    assert!(matches!(
        write(&*s, move |tx| tx.queue().settle(
            &eid,
            &f,
            Settlement::Completed
        )),
        Err(StorageError::LeaseLost { .. })
    ));
    let reclaimed = claim(&*s, "work", "owner-c", 1_000, 10).unwrap();
    let mine = reclaimed.iter().find(|e| e.id == entry.id).unwrap();
    assert_eq!((mine.generation, mine.attempt_count), (3, 2));
}

pub fn queue_retry_terminal_and_cancel(h: &mut dyn StoreHarness) {
    let s = h.open();
    let e = write(&*s, |tx| enqueue(tx, "retry", 1, 0, None)).unwrap();
    let c = claim(&*s, "retry", "w", 1_000, 1).unwrap().remove(0);
    let retry = FailureDisposition::Retry {
        delay_ms: 100,
        max_attempts: 2,
    };
    let (eid, f) = (e.id.clone(), c.fence().unwrap());
    let after = write(&*s, move |tx| {
        tx.queue().fail(&eid, &f, "transient: busy", retry)
    })
    .unwrap();
    assert_eq!(
        (after.state, after.ready_at_ms),
        (QueueState::Ready, h.now() + 100)
    );
    assert!(
        claim(&*s, "retry", "w", 1_000, 1).unwrap().is_empty(),
        "claimed before backoff"
    );
    h.set_now(h.now() + 100);
    let c = claim(&*s, "retry", "w", 1_000, 1).unwrap().remove(0);
    assert_eq!(c.attempt_count, 2);
    let (eid, f) = (e.id.clone(), c.fence().unwrap());
    let after = write(&*s, move |tx| {
        tx.queue().fail(&eid, &f, "transient: busy", retry)
    })
    .unwrap();
    assert_eq!(
        after.state,
        QueueState::Failed,
        "exhausted retries are terminal"
    );
    let history = read(&*s, |r| {
        r.history().page(
            &EventFilter::Aggregate {
                kind: "queue_entry".into(),
                id: e.id.as_str().into(),
            },
            0,
            500,
        )
    });
    let failures: Vec<_> = history
        .events
        .iter()
        .filter(|e| e.kind == "QueueAttemptFailed")
        .collect();
    assert_eq!(failures.len(), 2);
    assert_eq!(failures[0].payload["cause"], "transient: busy");
    assert_eq!(failures[1].payload["terminal"], true);
    let status = read(&*s, |r| r.queue().status("retry"));
    assert_eq!((status.failed, status.ready), (1, 0));
    let eid = e.id.clone();
    assert!(matches!(
        write(&*s, move |tx| tx.queue().compact(&eid)),
        Err(StorageError::InvalidRecord(_))
    ));

    // A deterministic failure is terminal at once.
    let p = write(&*s, |tx| enqueue(tx, "poison", 1, 0, None)).unwrap();
    let c = claim(&*s, "poison", "w", 1_000, 1).unwrap().remove(0);
    let (pid, f) = (p.id.clone(), c.fence().unwrap());
    let after = write(&*s, move |tx| {
        tx.queue()
            .fail(&pid, &f, "malformed payload", FailureDisposition::Terminal)
    })
    .unwrap();
    assert_eq!(after.state, QueueState::Failed);

    // Cancellation: a ready entry is cancelled; a claimed one is asked to stop cooperatively.
    let ready = write(&*s, |tx| enqueue(tx, "cancel", 1, 0, None)).unwrap();
    let rid = ready.id.clone();
    let cancelled = write(&*s, move |tx| tx.queue().cancel(&rid)).unwrap();
    assert_eq!(cancelled.state, QueueState::Cancelled);
    let running = write(&*s, |tx| enqueue(tx, "cancel", 1, 0, None)).unwrap();
    let c = claim(&*s, "cancel", "w", 1_000, 1).unwrap().remove(0);
    assert_eq!(c.id, running.id);
    let rid = running.id.clone();
    let asked = write(&*s, move |tx| tx.queue().cancel(&rid)).unwrap();
    assert_eq!(asked.state, QueueState::Claimed);
    assert_eq!(asked.cancellation_epoch, c.cancellation_epoch + 1);
    let (rid, f) = (running.id.clone(), c.fence().unwrap());
    let done = write(&*s, move |tx| {
        tx.queue().settle(&rid, &f, Settlement::Cancelled)
    })
    .unwrap();
    assert_eq!(done.state, QueueState::Cancelled);
    let rid = running.id.clone();
    write(&*s, move |tx| tx.queue().compact(&rid)).unwrap();

    // A compacted delivery keeps its (consumer, event) tombstone: the event is never redelivered.
    let (consumer, event_id) = (running.consumer.clone(), running.event_id.clone());
    let redelivery = write(&*s, move |tx| {
        let id_ = tx.new_id(IdKind::QueueEntry);
        tx.queue().enqueue(NewQueueEntry {
            id: id_,
            consumer,
            event_id,
            root_id: None,
            operation_id: None,
            priority: 0,
            ready_at_ms: 0,
            payload: json!({}),
        })
    });
    assert!(
        matches!(redelivery, Err(StorageError::Duplicate { .. })),
        "{redelivery:?}"
    );
    let rid = running.id.clone();
    assert!(read(&*s, move |r| r.queue().entry(&rid)).is_none());

    // Requeue from Failed restarts the retry budget (history keeps the failures).
    let eid = e.id.clone();
    let requeued = write(&*s, move |tx| tx.queue().requeue(&eid, 0)).unwrap();
    assert_eq!(
        (requeued.state, requeued.attempt_count),
        (QueueState::Ready, 0)
    );

    // A retryable failure of a delivery whose operation may have dispatched is held, not
    // returned to Ready: the retry hint never authorizes replaying a possible effect.
    let (d, op) = write(&*s, |tx| {
        let op = intent(tx)?;
        let a = tx
            .operations()
            .begin_attempt(&op.id, 1, AttemptBinding::default())?;
        tx.operations().transition(
            &op.id,
            2,
            Some(&a.fence()),
            OperationState::Dispatching,
            None,
        )?;
        Ok((enqueue(tx, "effects", 1, 0, Some(op.id.clone()))?, op.id))
    })
    .unwrap();
    let c = claim(&*s, "effects", "w", 1_000, 1).unwrap().remove(0);
    let (did, f) = (d.id.clone(), c.fence().unwrap());
    let held = write(&*s, move |tx| {
        tx.queue().fail(&did, &f, "connection reset", retry)
    })
    .unwrap();
    assert_eq!(held.state, QueueState::Reconciling);
    // Requeue from Reconciling keeps the attempt count once the outcome is established.
    let did = d.id.clone();
    let requeued = write(&*s, move |tx| {
        let rec = tx.operations().operation(&op)?.unwrap();
        tx.operations().transition(
            &op,
            rec.revision,
            rec.latest_attempt().map(AttemptRecord::fence).as_ref(),
            OperationState::RetryPending,
            Some(json!({"observed": "not applied"})),
        )?;
        tx.queue().requeue(&did, 0)
    })
    .unwrap();
    assert_eq!(
        (requeued.state, requeued.attempt_count),
        (QueueState::Ready, 1)
    );

    // Delivery dedupe is per (consumer, event); unknown events are refused.
    write(&*s, |tx| {
        let (event_id, _) = event(tx, "dedupe", "Changed")?;
        let mk = |tx: &mut dyn WriteTransaction, consumer: &str, ev: EventId| {
            let id_ = tx.new_id(IdKind::QueueEntry);
            tx.queue().enqueue(NewQueueEntry {
                id: id_,
                consumer: consumer.into(),
                event_id: ev,
                root_id: None,
                operation_id: None,
                priority: 0,
                ready_at_ms: 0,
                payload: json!({}),
            })
        };
        mk(tx, "a", event_id.clone())?;
        mk(tx, "b", event_id.clone())?;
        assert!(matches!(
            mk(tx, "a", event_id),
            Err(StorageError::Duplicate { .. })
        ));
        assert!(matches!(
            mk(tx, "a", EventId(id("missing-event"))),
            Err(StorageError::NotFound { kind: "event", .. })
        ));
        Ok(())
    })
    .unwrap();
}

fn gateway_for<'a>(s: &'a dyn ApplicationStore, ids: &'a Correlations) -> Gateway<'a> {
    Gateway {
        authority: &AllowAll,
        store: s,
        ids,
    }
}

/// Receipt-only decisions do not mutate work: a handler that wrote domain, journal and outbox
/// state and then refused leaves only the receipt and its CommandDecided history.
pub fn refused_decisions_persist_only_receipt_and_history(h: &mut dyn StoreHarness) {
    let s = h.open();
    let ctx = principal(&*s);
    let ids = Correlations::default();
    let before = counts(&*s);
    let refusing = Arc::new(Start {
        mode: StartMode::RefuseAfterWrites,
        calls: AtomicUsize::new(0),
    });
    let reply = gateway_for(&*s, &ids)
        .command(
            &ctx,
            "runs.start",
            start_body("k-refuse", "1").as_bytes(),
            refusing.clone(),
        )
        .unwrap();
    let CommandReply::Recorded {
        receipt,
        replayed: false,
        ..
    } = &reply
    else {
        panic!("{reply:?}")
    };
    assert!(matches!(receipt.decision, DomainDecision::Refused { .. }));
    assert_eq!(counts(&*s), (before.0 + 1, before.1));
    assert_eq!(
        kinds(&*s, EventFilter::All).last().map(String::as_str),
        Some("CommandDecided")
    );
    assert_eq!(read(&*s, |r| r.queue().status("supervisor")).ready, 0);
    let again = gateway_for(&*s, &ids)
        .command(
            &ctx,
            "runs.start",
            start_body("k-refuse", "1").as_bytes(),
            refusing.clone(),
        )
        .unwrap();
    assert!(matches!(
        again,
        CommandReply::Recorded { replayed: true, .. }
    ));
    assert_eq!(refusing.calls.load(Ordering::SeqCst), 1);
}

/// A handler that leaves its own savepoint open and hits a revision conflict still yields a
/// durable `RevisionConflict` refusal (not a storage failure), with all its writes undone.
pub fn nested_savepoint_conflict_is_a_durable_refusal(h: &mut dyn StoreHarness) {
    let s = h.open();
    let ctx = principal(&*s);
    let ids = Correlations::default();
    let before = counts(&*s);
    let nested = Arc::new(Start {
        mode: StartMode::NestedConflictAfterWrites,
        calls: AtomicUsize::new(0),
    });
    let reply = gateway_for(&*s, &ids)
        .command(
            &ctx,
            "runs.start",
            start_body("k-nested", "1").as_bytes(),
            nested.clone(),
        )
        .unwrap();
    let CommandReply::Recorded {
        receipt,
        replayed: false,
        ..
    } = &reply
    else {
        panic!("{reply:?}")
    };
    let DomainDecision::Refused { fault } = &receipt.decision else {
        panic!("{receipt:?}")
    };
    assert!(matches!(
        fault.code,
        crate::protocol::DomainErrorCode::RevisionConflict
    ));
    assert!(matches!(
        fault.retry,
        crate::protocol::RetryAdvice::RefreshAndNewRequest
    ));
    assert_eq!(counts(&*s), (before.0 + 1, before.1));
    assert_eq!(read(&*s, |r| r.queue().status("supervisor")).ready, 0);
    let again = gateway_for(&*s, &ids)
        .command(
            &ctx,
            "runs.start",
            start_body("k-nested", "1").as_bytes(),
            nested.clone(),
        )
        .unwrap();
    assert!(matches!(
        again,
        CommandReply::Recorded { replayed: true, .. }
    ));
    assert_eq!(nested.calls.load(Ordering::SeqCst), 1);
}
