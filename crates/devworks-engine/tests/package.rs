//! Package event handler transactions: state CAS, typed effect intents, the host
//! processing receipt and the subscription checkpoint commit together; crashes injected before
//! and after the commit replay once without losing or duplicating state or effects.
mod common;
use common::*;
use devworks_core::ports::storage::*;
use devworks_core::protocol::{Id, OperationId};
use devworks_engine::package::{
    CommitOutcome, CommitRefusal, PackageInvocation, commit_event, expected_checkpoint,
};
use devworks_engine::{
    AsyncDelivery, AsyncDeliveryHandler, AsyncFuture, AsyncOutcome, Delivery, DeliveryHandler,
    Outcome,
};
use devworks_extension_protocol::package::{EventCommit, RpcResult};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const PACKAGE: &str = "acme/demo";
const CONSUMER: &str = "pkg-acme-demo-sub-1";
const BROKER: &str = "package-broker";

fn artifact(name: &str) -> serde_json::Value {
    json!({"artifactId": name, "versionId": "v1", "sha256": "a".repeat(64), "byteLength": "10", "mediaType": "application/json"})
}
fn commit_for(
    session: &str,
    d: &Delivery,
    checkpoint: i64,
    revision: i64,
    proposal: &str,
) -> EventCommit {
    serde_json::from_value(json!({
        "processSessionId": session,
        "invocationId": format!("inv-{}", d.entry.id.as_str()),
        "invocationToken": "token-1",
        "subscriptionId": "sub-1",
        "eventId": d.event.id.0.as_str(),
        "expectedCheckpoint": checkpoint.to_string(),
        "stateGeneration": "1",
        "expectedStateRevision": revision.to_string(),
        "stateProposal": artifact(proposal),
        "intents": [
            {"actionKey": "notify", "call": {"kind": "ArtifactRead", "artifact": artifact("note")}},
            {"actionKey": "index", "call": {"kind": "ArtifactWrite", "proposal": artifact("idx")}}
        ]
    }))
    .unwrap()
}
fn invocation(
    session: &str,
    d: &Delivery,
    actor: devworks_core::protocol::ActorId,
) -> PackageInvocation {
    PackageInvocation {
        package_id: PACKAGE.into(),
        subscription_id: id("sub-1"),
        process_session_id: session.into(),
        invocation_id: format!("inv-{}", d.entry.id.as_str()),
        invocation_token: "token-1".into(),
        state_generation: 1,
        delivery_id: d.entry.id.clone(),
        fence: d.fence(),
        cancellation_epoch: d.entry.cancellation_epoch,
        actor,
        broker_consumer: BROKER.into(),
    }
}

/// A scripted package subscription handler (the process host is a later item; this drives the
/// same host commit path a package's `broker.commit_event` reaches).
struct DemoPackage {
    session: String,
    crash_before_commit: AtomicBool,
    calls: AtomicUsize,
    commits: Mutex<Vec<(EventCommit, RpcResult)>>,
}
impl DemoPackage {
    fn new(session: &str, crash: bool) -> Arc<Self> {
        Arc::new(Self {
            session: session.into(),
            crash_before_commit: AtomicBool::new(crash),
            calls: AtomicUsize::new(0),
            commits: Mutex::new(Vec::new()),
        })
    }
}
impl DeliveryHandler for DemoPackage {
    fn apply(&self, tx: &mut dyn WriteTransaction, d: &Delivery) -> Result<Outcome, StorageError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let state = tx.packages().package_state(PACKAGE)?.expect("activated");
        let checkpoint = expected_checkpoint(&*tx.queue(), &d.entry)?;
        let commit = commit_for(&self.session, d, checkpoint, state.revision, "state-2");
        let actor = tx.store().host_actor.clone();
        let outcome = commit_event(tx, &invocation(&self.session, d, actor), &commit)?;
        match outcome {
            CommitOutcome::Accepted(reply) | CommitOutcome::Replayed(reply) => {
                if self.crash_before_commit.swap(false, Ordering::SeqCst) {
                    panic!("injected crash after the handler's writes, before commit");
                }
                self.commits.lock().unwrap().push((commit, reply));
                Ok(Outcome::Committed)
            }
            CommitOutcome::Refused(r) => Ok(Outcome::Retry(format!("{r:?}"))),
        }
    }
}

fn activate(s: &dyn ApplicationStore) {
    write(s, |tx| {
        tx.packages()
            .compare_and_set_state(PACKAGE, 1, 0, json!({"initial": true}))
    })
    .unwrap();
}
struct Counts {
    state_revision: i64,
    processed: bool,
    intents: usize,
    outbox: i64,
    checkpoint: i64,
}
fn counts(s: &dyn ApplicationStore, e: &QueueEntry) -> Counts {
    read(s, |r| {
        Ok(Counts {
            state_revision: r.packages().package_state(PACKAGE)?.unwrap().revision,
            processed: r
                .packages()
                .processing(PACKAGE, &id("sub-1"), &e.event_id)?
                .is_some(),
            intents: r
                .operations()
                .unresolved(500)?
                .iter()
                .filter(|o| o.capability.starts_with("package.broker."))
                .count(),
            outbox: r.queue().status(BROKER)?.ready,
            checkpoint: r.queue().checkpoint(CONSUMER, Some("acme"))?,
        })
    })
}

#[test]
fn a_crash_before_commit_leaves_nothing_and_replays_once() {
    let h = Harness::new();
    let s = h.open();
    activate(&*s);
    let e = enqueue(&*s, CONSUMER, Some("acme"), json!({}));
    let pkg = DemoPackage::new("session-1", true);
    let d = dispatcher(&h, s.clone(), vec![consumer(CONSUMER, pkg.clone())]);
    runtime().block_on(d.run_until_idle()).unwrap();
    let c = counts(&*s, &e);
    assert_eq!(
        (
            c.state_revision,
            c.processed,
            c.intents,
            c.outbox,
            c.checkpoint
        ),
        (1, false, 0, 0, 0),
        "no state, receipt, intent or checkpoint survives the crash"
    );
    let retry = entry(&*s, &e.id);
    assert_eq!(retry.state, QueueState::Ready);
    // The process restarts; the delivery is replayed once.
    drop(d);
    drop(s);
    let s = h.open();
    h.advance(retry.ready_at_ms - h.now());
    let pkg2 = DemoPackage::new("session-2", false);
    let d = dispatcher(&h, s.clone(), vec![consumer(CONSUMER, pkg2.clone())]);
    runtime().block_on(d.run_until_idle()).unwrap();
    let c = counts(&*s, &e);
    assert_eq!(
        (
            c.state_revision,
            c.processed,
            c.intents,
            c.outbox,
            c.checkpoint
        ),
        (2, true, 2, 2, e.event_sequence)
    );
    assert_eq!(entry(&*s, &e.id).state, QueueState::Completed);
    assert_eq!(pkg2.calls.load(Ordering::SeqCst), 1);
    let RpcResult::EventCommitAccepted {
        operation_ids,
        state_revision,
        ..
    } = pkg2.commits.lock().unwrap()[0].1.clone()
    else {
        panic!()
    };
    assert_eq!(operation_ids.len(), 2);
    assert_eq!(String::from(state_revision), "2");
}

/// Reports one confirmed commit as `CommitUnknown`: the reply is lost after the commit. `lose`
/// counts down confirmed commits; the one that brings it from 1 to 0 loses its reply.
struct LossyStore {
    inner: Arc<dyn ApplicationStore>,
    lose: Arc<AtomicUsize>,
}
struct LossyJob {
    inner: Box<dyn WriteJob>,
    lose: Arc<AtomicUsize>,
}
impl WriteJob for LossyJob {
    fn run(&mut self, tx: &mut dyn WriteTransaction) -> Result<(), StorageError> {
        self.inner.run(tx)
    }
    fn finish(self: Box<Self>, outcome: Result<(), StorageError>) {
        let lost = outcome.is_ok()
            && self
                .lose
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                == Ok(1);
        self.inner.finish(if lost {
            Err(StorageError::CommitUnknown)
        } else {
            outcome
        });
    }
}
impl ApplicationStore for LossyStore {
    fn info(&self) -> StoreInfo {
        self.inner.info()
    }
    fn submit_write(
        &self,
        options: WriteOptions,
        job: Box<dyn WriteJob>,
    ) -> Result<(), StorageError> {
        self.inner.submit_write(
            options,
            Box::new(LossyJob {
                inner: job,
                lose: self.lose.clone(),
            }),
        )
    }
    fn read(
        &self,
        options: &ReadOptions,
        work: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.inner.read(options, work)
    }
}

#[test]
fn a_crash_after_commit_before_ack_replays_as_a_no_op() {
    let h = Harness::new();
    let s = h.open();
    activate(&*s);
    let e = enqueue(&*s, CONSUMER, Some("acme"), json!({}));
    let lose = Arc::new(AtomicUsize::new(0));
    let lossy: Arc<dyn ApplicationStore> = Arc::new(LossyStore {
        inner: s.clone(),
        lose: lose.clone(),
    });
    let pkg = DemoPackage::new("session-1", false);
    let d = dispatcher(&h, lossy.clone(), vec![consumer(CONSUMER, pkg.clone())]);
    let rt = runtime();
    // A pass commits expired-claim recovery, then the claim, then the delivery: lose the third.
    rt.block_on(d.recover()).unwrap();
    lose.store(3, Ordering::SeqCst);
    rt.block_on(d.run_until_idle()).unwrap();
    assert_eq!(lose.load(Ordering::SeqCst), 0, "the reply was lost");
    assert!(
        pkg.commits.lock().unwrap().len() == 1,
        "the handler committed exactly once"
    );
    let after = counts(&*s, &e);
    assert_eq!(
        (
            after.state_revision,
            after.processed,
            after.intents,
            after.outbox,
            after.checkpoint
        ),
        (2, true, 2, 2, e.event_sequence)
    );
    let (original_commit, original_reply) = pkg.commits.lock().unwrap()[0].clone();

    // The package never saw the reply; after restarting it resubmits the same commit from a new
    // process session with its old (now settled) claim. It gets the original reply, unchanged.
    let claimed = entry(&*s, &e.id);
    let event = read(&*s, |r| r.history().event(&e.event_id)).unwrap();
    let mut resubmitted = serde_json::to_value(&original_commit).unwrap();
    resubmitted["processSessionId"] = json!("session-restarted");
    resubmitted["invocationToken"] = json!("token-2");
    let resubmitted: EventCommit = serde_json::from_value(resubmitted).unwrap();
    let stale = Delivery {
        entry: QueueEntry {
            owner: Some(id("dispatcher-1")),
            generation: claimed.generation,
            ..claimed.clone()
        },
        event: event.clone(),
    };
    let (sd, rc) = (stale.clone(), resubmitted.clone());
    let replay = write(&*s, move |tx| {
        let actor = tx.store().host_actor.clone();
        commit_event(tx, &invocation("session-restarted", &sd, actor), &rc)
    })
    .unwrap();
    let CommitOutcome::Replayed(reply) = replay else {
        panic!("expected a replay, got {replay:?}")
    };
    assert_eq!(
        serde_json::to_value(&reply).unwrap(),
        serde_json::to_value(&original_reply).unwrap()
    );
    // A different commit for the same event is refused, not applied.
    let mut changed = serde_json::to_value(&original_commit).unwrap();
    changed["stateProposal"] = artifact("other-state");
    let changed: EventCommit = serde_json::from_value(changed).unwrap();
    let sd = stale.clone();
    let refused = write(&*s, move |tx| {
        let actor = tx.store().host_actor.clone();
        commit_event(tx, &invocation("session-restarted", &sd, actor), &changed)
    })
    .unwrap();
    assert!(matches!(
        refused,
        CommitOutcome::Refused(CommitRefusal::ChangedReplay)
    ));
    drop(d);
    drop(lossy);
    drop(s);

    // Restart: nothing is redelivered and nothing is duplicated.
    let s = h.open();
    let pkg2 = DemoPackage::new("session-3", false);
    let d = dispatcher(&h, s.clone(), vec![consumer(CONSUMER, pkg2.clone())]);
    runtime().block_on(d.run_until_idle()).unwrap();
    assert_eq!(pkg2.calls.load(Ordering::SeqCst), 0);
    let c = counts(&*s, &e);
    assert_eq!(
        (
            c.state_revision,
            c.processed,
            c.intents,
            c.outbox,
            c.checkpoint
        ),
        (2, true, 2, 2, e.event_sequence)
    );
}

#[test]
fn commits_are_validated_before_anything_is_written() {
    let h = Harness::new();
    let s = h.open();
    activate(&*s);
    let e = enqueue(&*s, CONSUMER, Some("acme"), json!({}));
    let claimed = write(&*s, |tx| {
        tx.queue().claim_ready(&ClaimRequest {
            consumer: CONSUMER.into(),
            owner: id("host-invoker"),
            lease_ms: 60_000,
            limit: 1,
        })
    })
    .unwrap()
    .remove(0);
    let event = read(&*s, |r| r.history().event(&e.event_id)).unwrap();
    let d = Delivery {
        entry: claimed.clone(),
        event,
    };
    let attempt = |d: Delivery, commit: EventCommit, inv: PackageInvocation| {
        write(&*s, move |tx| {
            let _ = &d;
            commit_event(tx, &inv, &commit)
        })
        .unwrap()
    };
    let actor = s.info().host_actor.clone();
    let good = commit_for("s", &d, 0, 1, "next");
    let mut wrong_fence = invocation("s", &d, actor.clone());
    wrong_fence.fence.generation += 1;
    let cases: Vec<(EventCommit, PackageInvocation, CommitRefusal)> = vec![
        (
            commit_for("s", &d, 5, 1, "next"),
            invocation("s", &d, actor.clone()),
            CommitRefusal::CheckpointConflict {
                expected: 5,
                current: 0,
            },
        ),
        (
            commit_for("s", &d, 0, 3, "next"),
            invocation("s", &d, actor.clone()),
            CommitRefusal::StateConflict {
                expected: 3,
                current: 1,
            },
        ),
        (good.clone(), wrong_fence, CommitRefusal::StaleInvocation),
        // An invocation started under an earlier cancellation epoch is refused.
        (
            good.clone(),
            PackageInvocation {
                cancellation_epoch: claimed.cancellation_epoch + 1,
                ..invocation("s", &d, actor.clone())
            },
            CommitRefusal::Cancelled,
        ),
        (
            commit_for("other-session", &d, 0, 1, "next"),
            invocation("s", &d, actor.clone()),
            CommitRefusal::BindingMismatch,
        ),
        (
            good.clone(),
            PackageInvocation {
                state_generation: 2,
                ..invocation("s", &d, actor.clone())
            },
            CommitRefusal::GenerationMismatch {
                pinned: 1,
                current: Some(1),
            },
        ),
    ];
    for (commit, inv, expected) in cases {
        match attempt(d.clone(), commit, inv) {
            CommitOutcome::Refused(r) => assert_eq!(r, expected),
            other => panic!("expected {expected:?}, got {other:?}"),
        }
    }
    let mut dup = serde_json::to_value(&good).unwrap();
    dup["intents"][1]["actionKey"] = json!("notify");
    let dup: EventCommit = serde_json::from_value(dup).unwrap();
    assert!(matches!(
        attempt(d.clone(), dup, invocation("s", &d, actor.clone())),
        CommitOutcome::Refused(CommitRefusal::Invalid(_))
    ));
    let c = counts(&*s, &e);
    assert_eq!(
        (c.state_revision, c.processed, c.intents, c.outbox),
        (1, false, 0, 0)
    );
    assert_eq!(entry(&*s, &e.id).state, QueueState::Claimed);
    // The valid commit is accepted with the host-minted identities.
    let CommitOutcome::Accepted(RpcResult::EventCommitAccepted {
        operation_ids,
        checkpoint,
        ..
    }) = attempt(d.clone(), good, invocation("s", &d, actor))
    else {
        panic!("accepted")
    };
    assert_eq!(String::from(checkpoint), e.event_sequence.to_string());
    let ops: Vec<OperationId> = operation_ids
        .iter()
        .map(|o| OperationId(Id::new(o.0.as_str()).unwrap()))
        .collect();
    for op in ops {
        let rec = read(&*s, |r| r.operations().operation(&op)).unwrap();
        assert_eq!(rec.payload["packageId"], PACKAGE);
    }
}

/// An invocation whose delivery is cancelled while it is claimed cannot commit: the refusal
/// writes nothing, and the delivery stays claimed for its owner to settle as cancelled.
#[test]
fn a_cancelled_invocation_is_refused_without_writes() {
    let h = Harness::new();
    let s = h.open();
    activate(&*s);
    let e = enqueue(&*s, CONSUMER, Some("acme"), json!({}));
    let claimed = write(&*s, |tx| {
        tx.queue().claim_ready(&ClaimRequest {
            consumer: CONSUMER.into(),
            owner: id("host-invoker"),
            lease_ms: 60_000,
            limit: 1,
        })
    })
    .unwrap()
    .remove(0);
    let event = read(&*s, |r| r.history().event(&e.event_id)).unwrap();
    let d = Delivery {
        entry: claimed.clone(),
        event,
    };
    let inv = invocation("s", &d, s.info().host_actor.clone());
    let qid = e.id.clone();
    write(&*s, move |tx| tx.queue().cancel(&qid)).unwrap();
    let commit = commit_for("s", &d, 0, 1, "next");
    let outcome = write(&*s, move |tx| commit_event(tx, &inv, &commit)).unwrap();
    assert!(
        matches!(outcome, CommitOutcome::Refused(CommitRefusal::Cancelled)),
        "{outcome:?}"
    );
    let c = counts(&*s, &e);
    assert_eq!(
        (
            c.state_revision,
            c.processed,
            c.intents,
            c.outbox,
            c.checkpoint
        ),
        (1, false, 0, 0, 0)
    );
    assert_eq!(entry(&*s, &e.id).state, QueueState::Claimed);
}

/// Unordered subscriptions use a per-delivery checkpoint: two deliveries commit in either order
/// with the same expected checkpoint, and each reply's checkpoint is its own event sequence. A
/// consumer-wide watermark would refuse the second commit and lie below the first event.
#[test]
fn unordered_subscriptions_checkpoint_per_delivery() {
    let h = Harness::new();
    let s = h.open();
    activate(&*s);
    let first = enqueue(&*s, CONSUMER, None, json!({}));
    let second = enqueue(&*s, CONSUMER, None, json!({}));
    let claimed = write(&*s, |tx| {
        tx.queue().claim_ready(&ClaimRequest {
            consumer: CONSUMER.into(),
            owner: id("host-invoker"),
            lease_ms: 60_000,
            limit: 2,
        })
    })
    .unwrap();
    assert_eq!(claimed.len(), 2);
    let actor = s.info().host_actor.clone();
    // The later event commits first, then the earlier one.
    for (target, revision) in [(&second, 1), (&first, 2)] {
        let claim = claimed.iter().find(|c| c.id == target.id).unwrap().clone();
        let event = read(&*s, |r| r.history().event(&target.event_id)).unwrap();
        let d = Delivery {
            entry: claim,
            event,
        };
        let expected = read(&*s, |r| expected_checkpoint(r.queue(), &d.entry));
        assert_eq!(expected, 0);
        let commit = commit_for("s", &d, expected, revision, &format!("state-{revision}"));
        let inv = invocation("s", &d, actor.clone());
        let outcome = write(&*s, move |tx| commit_event(tx, &inv, &commit)).unwrap();
        let CommitOutcome::Accepted(RpcResult::EventCommitAccepted { checkpoint, .. }) = outcome
        else {
            panic!("{outcome:?}")
        };
        assert_eq!(String::from(checkpoint), target.event_sequence.to_string());
    }
    for e in [&first, &second] {
        assert_eq!(entry(&*s, &e.id).state, QueueState::Completed);
    }
}

/// A package process modelled as an asynchronous handler: it starts, signals, and commits only
/// when the test opens its gate or the dispatcher signals a cancellation.
struct ProcessPackage {
    store: Arc<dyn ApplicationStore>,
    started: Arc<tokio::sync::Notify>,
    gate: tokio::sync::watch::Receiver<bool>,
    outcome: Mutex<Option<CommitOutcome>>,
    saw_cancel: AtomicBool,
}
struct Process(Arc<ProcessPackage>);
impl AsyncDeliveryHandler for Process {
    fn start(&self, delivery: AsyncDelivery) -> AsyncFuture {
        let me = self.0.clone();
        Box::pin(async move {
            me.started.notify_one();
            let mut gate = me.gate.clone();
            let mut cancelled = delivery.cancelled.clone();
            loop {
                if *cancelled.borrow() {
                    me.saw_cancel.store(true, Ordering::SeqCst);
                    break;
                }
                if *gate.borrow() {
                    break;
                }
                let mut g = std::pin::pin!(gate.changed());
                let mut c = std::pin::pin!(cancelled.changed());
                std::future::poll_fn(|cx| {
                    use std::future::Future;
                    if g.as_mut().poll(cx).is_ready() || c.as_mut().poll(cx).is_ready() {
                        std::task::Poll::Ready(())
                    } else {
                        std::task::Poll::Pending
                    }
                })
                .await;
            }
            // The process commits through the host path even after a cancellation (a racing
            // package cannot be trusted to stop): the host refuses it.
            let d = delivery.delivery;
            let state = read(&*me.store, |r| r.packages().package_state(PACKAGE))
                .unwrap()
                .revision;
            let expected = read(&*me.store, |r| expected_checkpoint(r.queue(), &d.entry));
            let commit = commit_for("session-p", &d, expected, state, "state-2");
            let inv = invocation("session-p", &d, me.store.info().host_actor.clone());
            let outcome = write(&*me.store, move |tx| commit_event(tx, &inv, &commit)).unwrap();
            let result = match &outcome {
                CommitOutcome::Accepted(_) | CommitOutcome::Replayed(_) => AsyncOutcome::Committed,
                CommitOutcome::Refused(r) => AsyncOutcome::Retry(format!("{r:?}")),
            };
            *me.outcome.lock().unwrap() = Some(outcome);
            result
        })
    }
}

fn process_package(
    s: &Arc<dyn ApplicationStore>,
) -> (Arc<ProcessPackage>, tokio::sync::watch::Sender<bool>) {
    let (gate_tx, gate) = tokio::sync::watch::channel(false);
    let pkg = Arc::new(ProcessPackage {
        store: s.clone(),
        started: Arc::new(tokio::sync::Notify::new()),
        gate,
        outcome: Mutex::new(None),
        saw_cancel: AtomicBool::new(false),
    });
    (pkg, gate_tx)
}

/// The dispatcher keeps an asynchronous handler's claim live: its lease is renewed while the
/// handler runs (here past the original expiry), and the handler then commits under the claim's
/// fence and epoch.
#[test]
fn an_async_handler_keeps_a_renewed_claim_and_commits_under_its_fence() {
    let h = Harness::new();
    let s = h.open();
    activate(&*s);
    let e = enqueue(&*s, CONSUMER, Some("acme"), json!({}));
    let (pkg, gate) = process_package(&s);
    let mut c = consumer(CONSUMER, Recorder::ok());
    c.async_handler = Some(Arc::new(Process(pkg.clone())));
    // Sleep/wake reassertion stays out of the way: only the handler supervisor renews.
    let d = dispatcher_with(&h, s.clone(), vec![c], |c| c.wake_jump_ms = i64::MAX);
    let rt = runtime();
    rt.block_on(async {
        assert_eq!(d.tick().await.unwrap().claimed, 1);
        pkg.started.notified().await;
        let original = entry(&*s, &e.id);
        let lease = original.lease_until_ms.unwrap();
        // 20 s later the supervisor renews when prompted.
        h.advance(20_000);
        let mut status = d.subscribe();
        d.wakeup().wake();
        let want = h.now() + 30_000;
        while entry(&*s, &e.id).lease_until_ms != Some(want) {
            status.changed().await.unwrap();
        }
        // Past the original expiry, recovery leaves the renewed claim alone.
        h.advance(20_000);
        assert!(h.now() > lease);
        let report = d.tick().await.unwrap();
        assert!(report.recovered.requeued.is_empty() && report.recovered.held.is_empty());
        let still = entry(&*s, &e.id);
        assert_eq!(
            (still.state, still.generation),
            (QueueState::Claimed, original.generation)
        );
        gate.send(true).unwrap();
        d.idle().await;
    });
    assert!(matches!(
        pkg.outcome.lock().unwrap().as_ref(),
        Some(CommitOutcome::Accepted(_))
    ));
    let c = counts(&*s, &e);
    assert_eq!(
        (
            c.state_revision,
            c.processed,
            c.intents,
            c.outbox,
            c.checkpoint
        ),
        (2, true, 2, 2, e.event_sequence)
    );
    assert_eq!(entry(&*s, &e.id).state, QueueState::Completed);
}

/// A cancellation requested while an asynchronous handler runs is observed by the supervisor
/// and signalled to the handler; its commit is refused without writes and the delivery is
/// settled `Cancelled`.
#[test]
fn an_async_handler_observes_cancellation_and_cannot_commit_after_it() {
    let h = Harness::new();
    let s = h.open();
    activate(&*s);
    let e = enqueue(&*s, CONSUMER, Some("acme"), json!({}));
    let (pkg, _gate) = process_package(&s);
    let mut c = consumer(CONSUMER, Recorder::ok());
    c.async_handler = Some(Arc::new(Process(pkg.clone())));
    let d = dispatcher(&h, s.clone(), vec![c]);
    let rt = runtime();
    rt.block_on(async {
        assert_eq!(d.tick().await.unwrap().claimed, 1);
        pkg.started.notified().await;
        let qid = e.id.clone();
        write(&*s, move |tx| tx.queue().cancel(&qid)).unwrap();
        d.wakeup().wake();
        d.idle().await;
    });
    assert!(
        pkg.saw_cancel.load(Ordering::SeqCst),
        "cancellation not signalled"
    );
    assert!(matches!(
        pkg.outcome.lock().unwrap().as_ref(),
        Some(CommitOutcome::Refused(CommitRefusal::Cancelled))
    ));
    let c = counts(&*s, &e);
    assert_eq!(
        (c.state_revision, c.processed, c.intents, c.outbox),
        (1, false, 0, 0)
    );
    assert_eq!(entry(&*s, &e.id).state, QueueState::Cancelled);
}
