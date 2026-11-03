//! Durable delivery against the real SQLite adapter: claims, fences, ordering, retries, poison,
//! wakeups, sleep/wake and restart recovery (CAP-QUEUE).
mod common;
use common::*;
use devworks_core::context::PrincipalKind;
use devworks_core::gateway::Gateway;
use devworks_core::ports::IdKind;
use devworks_core::ports::storage::*;
use devworks_core::protocol::Id;
use devworks_core::protocol::results::DomainDecision;
use devworks_core::protocol::{CommandReply, OperationState};
use devworks_core::services::UseCases;
use devworks_engine::{Dispatcher, EffectResult, Outcome};
use serde_json::json;
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn kinds(s: &dyn ApplicationStore, entry: &devworks_core::protocol::Id) -> Vec<String> {
    read(s, |r| {
        r.history().page(
            &EventFilter::Aggregate {
                kind: "queue_entry".into(),
                id: entry.as_str().into(),
            },
            0,
            500,
        )
    })
    .events
    .into_iter()
    .map(|e| e.kind)
    .collect()
}

/// Waits on dispatcher status hints until `done` holds, re-checking at least every 20 ms (a
/// spawned effect may progress without a hint), within a 10 s wall-clock bound.
async fn until(d: &Dispatcher, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut rx = d.subscribe();
    while !done() {
        assert!(
            std::time::Instant::now() < deadline,
            "condition not reached"
        );
        let _ = tokio::time::timeout(Duration::from_millis(20), rx.changed()).await;
    }
}

#[test]
fn restart_recovers_before_claiming_and_never_replays_a_possible_effect() {
    let h = Harness::new();
    let s = h.open();
    let recorder = Recorder::ok();
    let fx = Effects::new(EffectResult::Completed(json!({"remote": "done"})));
    let gate = fx.gated();
    // Boot 1: an effect heads partition root-a with a dependent behind it; root-b and an
    // unordered delivery are unrelated. One more delivery is stranded in a claim of this boot.
    let (op, effect, dependent, unrelated, free) = write(&*s, |tx| {
        let op = intent(tx)?;
        let effect = enqueue_tx(tx, "effects", Some("root-a"), Some(op.clone()), json!({}))?;
        let dependent = enqueue_tx(tx, "effects", Some("root-a"), None, json!({}))?;
        let unrelated = enqueue_tx(tx, "effects", Some("root-b"), None, json!({}))?;
        let free = enqueue_tx(tx, "plain", None, None, json!({}))?;
        Ok((op, effect, dependent, unrelated, free))
    })
    .unwrap();
    enqueue(&*s, "plain", None, json!({}));
    let ghost = ClaimRequest {
        consumer: "plain".into(),
        owner: id("ghost"),
        lease_ms: 3_600_000,
        limit: 1,
    };
    let stranded = write(&*s, move |tx| tx.queue().claim_ready(&ghost))
        .unwrap()
        .remove(0);
    let mut c = consumer("effects", recorder.clone());
    c.effect = Some(fx.executor());
    let consumers = vec![c, consumer("plain", recorder.clone())];
    let rt = runtime();
    let d1 = dispatcher(&h, s.clone(), consumers.clone());
    rt.block_on(async {
        d1.tick().await.unwrap();
        until(&d1, || fx.executed() == 1 && d1.in_flight() == 1).await;
    });
    assert_eq!(
        read(&*s, |r| r.operations().operation(&op)).unwrap().state,
        OperationState::Dispatching
    );
    // The process dies while the effect is in flight.
    drop(rt);
    drop(gate);
    drop(d1);
    drop(s);

    // Boot 2.
    let s = h.open();
    let fx2 = Effects::new(EffectResult::Completed(json!({"remote": "again"})));
    *fx2.observation.lock().unwrap() = EffectResult::Completed(json!({"observed": "done"}));
    let mut c = consumer("effects", recorder.clone());
    c.effect = Some(fx2.executor());
    let d2 = dispatcher(&h, s.clone(), vec![c, consumer("plain", recorder.clone())]);
    let rt = runtime();
    let late = rt.block_on(async {
        // The first pass recovers before it claims: the stranded prior-boot claim is requeued
        // and delivered in the same pass; the possibly dispatched effect is held.
        let report = d2.tick().await.unwrap();
        d2.idle().await;
        assert!(report.claimed >= 1);
        d2.run_until_idle().await.unwrap();
        let late = enqueue(&*s, "effects", Some("root-b"), json!({}));
        d2.run_until_idle().await.unwrap();
        late
    });
    assert_eq!(
        read(&*s, |r| r.operations().operation(&op)).unwrap().state,
        OperationState::OutcomeUnknown
    );
    assert_eq!(entry(&*s, &effect.id).state, QueueState::Reconciling);
    assert_eq!(
        entry(&*s, &dependent.id).state,
        QueueState::Ready,
        "dependent paused"
    );
    for done in [&unrelated, &free, &stranded, &late] {
        assert_eq!(entry(&*s, &done.id).state, QueueState::Completed);
    }
    assert!(kinds(&*s, &stranded.id).contains(&"QueueClaimRecovered".to_owned()));
    assert_eq!(fx2.executed(), 0, "an uncertain effect is never replayed");

    // Observation establishes the outcome but does not redeliver by itself.
    let resolved = rt.block_on(d2.observe_unknown()).unwrap();
    assert_eq!(resolved, 1);
    assert_eq!(
        read(&*s, |r| r.operations().operation(&op)).unwrap().state,
        OperationState::ObservedComplete
    );
    assert_eq!(entry(&*s, &effect.id).state, QueueState::Reconciling);

    // An operator reconciles through the shared gateway; the delivery completes without
    // dispatching again and its dependent follows in order.
    let ctx = principal(&*s, "operator", PrincipalKind::Human);
    let ids = h.ids.clone();
    let gateway = Gateway {
        authority: &Authority,
        store: &*s,
        ids: &*ids,
    };
    let held = entry(&*s, &effect.id);
    let body = format!(
        r#"{{"mode":"Command","protocol":"1.0-draft.1","requestKey":"reconcile-1","body":{{"kind":"queue.reconcile","deliveryId":"{}","expectedRevision":"{}","expectedFence":"{}","resolution":"Reconcile","reason":"observed remote"}}}}"#,
        held.id.as_str(),
        held.revision,
        held.generation
    );
    let reply = UseCases::new()
        .with_queue()
        .command(&gateway, &ctx, "queue.reconcile", body.as_bytes())
        .unwrap();
    assert!(matches!(
        reply,
        CommandReply::Recorded {
            receipt: devworks_core::protocol::Receipt {
                decision: DomainDecision::Applied { .. },
                ..
            },
            ..
        }
    ));
    rt.block_on(d2.run_until_idle()).unwrap();
    assert_eq!(fx2.executed(), 0);
    assert_eq!(entry(&*s, &effect.id).state, QueueState::Completed);
    assert_eq!(entry(&*s, &dependent.id).state, QueueState::Completed);
    let order: Vec<i64> = recorder
        .seen()
        .into_iter()
        .filter(|(_, p, _)| p.as_deref() == Some("root-a"))
        .map(|(_, _, seq)| seq)
        .collect();
    assert_eq!(order, [effect.event_sequence, dependent.event_sequence]);
}

#[test]
fn concurrent_dispatchers_claim_each_delivery_once() {
    let h = Harness::new();
    let s = h.open();
    for _ in 0..120 {
        enqueue(&*s, "race", None, json!({}));
    }
    let recorder = Recorder::ok();
    let a = dispatcher_with(
        &h,
        s.clone(),
        vec![consumer("race", recorder.clone())],
        |c| {
            c.owner = id("owner-a");
            c.claim_batch = 7;
        },
    );
    let b = dispatcher_with(
        &h,
        s.clone(),
        vec![consumer("race", recorder.clone())],
        |c| {
            c.owner = id("owner-b");
            c.claim_batch = 5;
        },
    );
    runtime().block_on(async {
        let (a2, b2) = (a.clone(), b.clone());
        let x = tokio::spawn(async move { a2.run_until_idle().await });
        let y = tokio::spawn(async move { b2.run_until_idle().await });
        let total = x.await.unwrap().unwrap() + y.await.unwrap().unwrap();
        assert_eq!(total, 120);
    });
    let seen = recorder.seen();
    let unique: HashSet<_> = seen.iter().map(|(_, _, seq)| *seq).collect();
    assert_eq!((seen.len(), unique.len()), (120, 120));
}

#[test]
fn expired_leases_are_reclaimed_and_stale_owners_fenced() {
    let h = Harness::new();
    let s = h.open();
    let e = enqueue(&*s, "work", None, json!({}));
    let ghost = ClaimRequest {
        consumer: "work".into(),
        owner: id("stalled"),
        lease_ms: 1_000,
        limit: 1,
    };
    let stale = write(&*s, move |tx| tx.queue().claim_ready(&ghost))
        .unwrap()
        .remove(0);
    let recorder = Recorder::ok();
    let d = dispatcher(&h, s.clone(), vec![consumer("work", recorder.clone())]);
    let rt = runtime();
    assert_eq!(
        rt.block_on(d.run_until_idle()).unwrap(),
        0,
        "a live lease is respected"
    );
    h.advance(2_000);
    let report = rt.block_on(d.tick()).unwrap();
    rt.block_on(d.idle());
    assert!(!report.woke);
    assert_eq!(report.recovered.requeued, std::slice::from_ref(&e.id));
    assert_eq!(report.claimed, 1);
    assert_eq!(entry(&*s, &e.id).state, QueueState::Completed);
    let (eid, f) = (e.id.clone(), stale.fence().unwrap());
    assert!(matches!(
        write(&*s, move |tx| tx.queue().settle(
            &eid,
            &f,
            Settlement::Completed
        )),
        Err(StorageError::LeaseLost { .. })
    ));
    assert_eq!(recorder.seen().len(), 1);
}

#[test]
fn partitions_are_ordered_and_independent_and_poison_blocks_only_its_partition() {
    let h = Harness::new();
    let s = h.open();
    let mut expected: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    let mut bad = Vec::new();
    for round in 0..4 {
        for p in ["p-a", "p-b", "p-bad"] {
            let e = enqueue(&*s, "ord", Some(p), json!({"round": round}));
            if p == "p-bad" {
                bad.push(e);
            } else {
                expected.entry(p.into()).or_default().push(e.event_sequence);
            }
        }
        enqueue(&*s, "ord", None, json!({}));
    }
    let head = bad[0].event_sequence;
    let recorder = Recorder::new(move |d, _| {
        if d.entry.event_sequence == head {
            Outcome::Poison("malformed payload".into())
        } else {
            Outcome::Completed
        }
    });
    let d = dispatcher(&h, s.clone(), vec![consumer("ord", recorder.clone())]);
    runtime().block_on(d.run_until_idle()).unwrap();
    let mut got: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    let mut unordered = 0;
    for (_, p, seq) in recorder.seen() {
        match p {
            Some(p) => got.entry(p).or_default().push(seq),
            None => unordered += 1,
        }
    }
    assert_eq!(
        got, expected,
        "each partition in event order; p-bad stopped at its head"
    );
    assert_eq!(unordered, 4);
    let failed = entry(&*s, &bad[0].id);
    assert_eq!(failed.state, QueueState::Failed);
    assert_eq!(failed.last_error.as_deref(), Some("malformed payload"));
    for e in &bad[1..] {
        assert_eq!(entry(&*s, &e.id).state, QueueState::Ready);
    }
    let progress = read(&*s, |r| r.queue().progress("ord"));
    assert!(
        progress.watermark < head,
        "the watermark never passes the poison delivery"
    );
}

#[test]
fn transient_failures_back_off_durably_then_fail_visibly() {
    let h = Harness::new();
    let s = h.open();
    let e = enqueue(&*s, "flaky", None, json!({}));
    let recorder = Recorder::new(|_, _| Outcome::Retry("upstream busy".into()));
    let d = dispatcher(&h, s.clone(), vec![consumer("flaky", recorder.clone())]);
    let rt = runtime();
    let mut delays = Vec::new();
    for _ in 0..3 {
        rt.block_on(d.run_until_idle()).unwrap();
        let now = entry(&*s, &e.id);
        if now.state == QueueState::Failed {
            break;
        }
        assert_eq!(now.state, QueueState::Ready);
        assert_eq!(now.last_error.as_deref(), Some("upstream busy"));
        let delay = now.ready_at_ms - h.now();
        delays.push(delay);
        // Not claimable before its saved ready time.
        assert_eq!(rt.block_on(d.run_until_idle()).unwrap(), 0);
        h.advance(delay);
    }
    assert_eq!(delays, [100, 200], "durable exponential backoff");
    let failed = entry(&*s, &e.id);
    assert_eq!(
        (failed.state, failed.attempt_count),
        (QueueState::Failed, 3)
    );
    let attempts = kinds(&*s, &e.id)
        .into_iter()
        .filter(|k| k == "QueueAttemptFailed")
        .count();
    assert_eq!(attempts, 3);
    assert_eq!(read(&*s, |r| r.queue().status("flaky")).failed, 1);
}

/// Idempotent handler state for at-least-once delivery: applies each event once.
struct Ledger {
    calls: std::sync::atomic::AtomicUsize,
}
impl devworks_engine::DeliveryHandler for Ledger {
    fn apply(
        &self,
        tx: &mut dyn WriteTransaction,
        d: &devworks_engine::Delivery,
    ) -> Result<Outcome, StorageError> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let state = tx.packages().package_state("ledger")?;
        let (rev, mut seen) = match &state {
            Some(s) => (
                s.revision,
                s.state["seen"].as_array().cloned().unwrap_or_default(),
            ),
            None => (0, Vec::new()),
        };
        let key = json!(d.event.id.0.as_str());
        if !seen.contains(&key) {
            seen.push(key);
            tx.packages()
                .compare_and_set_state("ledger", 1, rev, json!({"seen": seen}))?;
        }
        if n == 0 {
            panic!("injected crash after the handler's writes, before commit");
        }
        Ok(Outcome::Completed)
    }
}

#[test]
fn at_least_once_redelivery_applies_idempotently() {
    let h = Harness::new();
    let s = h.open();
    let e = enqueue(&*s, "ledger", None, json!({}));
    let ledger = Arc::new(Ledger {
        calls: Default::default(),
    });
    let d = dispatcher(&h, s.clone(), vec![consumer("ledger", ledger.clone())]);
    let rt = runtime();
    rt.block_on(d.run_until_idle()).unwrap();
    let after = entry(&*s, &e.id);
    assert_eq!(
        after.state,
        QueueState::Ready,
        "the crashed attempt rolled back"
    );
    assert!(read(&*s, |r| r.packages().package_state("ledger")).is_none());
    h.advance(after.ready_at_ms - h.now());
    rt.block_on(d.run_until_idle()).unwrap();
    assert_eq!(ledger.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    let state = read(&*s, |r| r.packages().package_state("ledger")).unwrap();
    assert_eq!(
        (
            state.revision,
            state.state["seen"].as_array().unwrap().len()
        ),
        (1, 1)
    );
    assert_eq!(entry(&*s, &e.id).state, QueueState::Completed);
    // Delivery identity dedupes a second enqueue of the same event for the same consumer.
    let (ev, c) = (e.event_id.clone(), e.consumer.clone());
    assert!(matches!(
        write(&*s, move |tx| {
            let id_ = tx.new_id(IdKind::QueueEntry);
            tx.queue().enqueue(NewQueueEntry {
                id: id_,
                consumer: c,
                event_id: ev,
                root_id: None,
                operation_id: None,
                partition: None,
                priority: 0,
                ready_at_ms: 0,
                payload: json!({}),
            })
        }),
        Err(StorageError::Duplicate { .. })
    ));
}

/// The supervised loop with an injected sweep timer: every wait the loop asks for and every
/// sweep it takes is driven by the test, so no wall-clock bound decides the outcome. A delivery
/// committed without a wakeup waits for the next sweep, the sweep wait is bounded by the sweep
/// interval and shortened to the next saved ready time, and a wakeup needs no sweep.
#[test]
fn a_missing_wakeup_is_repaired_by_the_bounded_sweep() {
    let h = Harness::new();
    let s = h.open();
    let recorder = Recorder::ok();
    let interval = Duration::from_secs(3_600);
    let lease = Duration::from_secs(30);
    let d = dispatcher_with(
        &h,
        s.clone(),
        vec![consumer("work", recorder.clone())],
        |c| {
            c.sweep_interval = interval;
            c.lease_ms = 30_000;
        },
    );
    let (asked_tx, mut asked) = tokio::sync::mpsc::unbounded_channel::<Duration>();
    let sweep = Arc::new(tokio::sync::Notify::new());
    runtime().block_on(within(
        "a_missing_wakeup_is_repaired_by_the_bounded_sweep",
        async {
            let (tick, d2) = (sweep.clone(), d.clone());
            let looping = tokio::spawn(async move {
                d2.run_with_sweep(move |wait| {
                    let (asked_tx, tick) = (asked_tx.clone(), tick.clone());
                    async move {
                        let _ = asked_tx.send(wait);
                        tick.notified().await;
                    }
                })
                .await
            });
            // First pass over an empty queue: the loop waits one full sweep interval.
            assert_eq!(asked.recv().await, Some(interval));
            // A delivery due later, committed without a wakeup: the loop stays parked until the
            // sweep; that pass sees nothing ready and shortens its next wait to the saved time.
            let later = write(&*s, |tx| {
                let event_id = event(tx, "Later")?;
                let entry_id = tx.new_id(IdKind::QueueEntry);
                let ready_at_ms = tx.now_ms() + 5_000;
                tx.queue().enqueue(NewQueueEntry {
                    id: entry_id,
                    consumer: "work".into(),
                    event_id,
                    root_id: None,
                    operation_id: None,
                    partition: None,
                    priority: 0,
                    ready_at_ms,
                    payload: json!({}),
                })
            })
            .unwrap();
            assert!(asked.is_empty() && recorder.seen().is_empty());
            sweep.notify_one();
            assert_eq!(asked.recv().await, Some(Duration::from_millis(5_000)));
            // Time passes; without a sweep nothing is delivered. The sweep delivers it.
            h.advance(5_000);
            assert!(recorder.seen().is_empty());
            sweep.notify_one();
            // The pass that claimed it waits at most until the claim's lease would expire, if the
            // claim was still live when the next due time was read.
            let claimed_wait = |w: Option<Duration>| w == Some(interval) || w == Some(lease);
            assert!(claimed_wait(asked.recv().await));
            d.idle().await;
            assert_eq!(recorder.seen().len(), 1);
            assert_eq!(entry(&*s, &later.id).state, QueueState::Completed);
            // The finished delivery freed capacity, which wakes exactly one further pass.
            assert_eq!(asked.recv().await, Some(interval));
            // A wakeup needs no sweep.
            let woken = enqueue(&*s, "work", None, json!({}));
            d.wakeup().wake();
            assert!(claimed_wait(asked.recv().await));
            d.idle().await;
            assert_eq!(entry(&*s, &woken.id).state, QueueState::Completed);
            assert_eq!(recorder.seen().len(), 2);
            d.stop();
            looping.await.unwrap();
        },
    ));
}

#[test]
fn sleep_and_wake_repairs_live_leases_before_recovering_abandoned_ones() {
    let h = Harness::new();
    let s = h.open();
    let fx = Effects::new(EffectResult::Completed(json!({"ok": true})));
    let gate = fx.gated();
    let effect = write(&*s, |tx| {
        let op = intent(tx)?;
        enqueue_tx(tx, "effects", None, Some(op), json!({}))
    })
    .unwrap();
    let abandoned = enqueue(&*s, "plain", None, json!({}));
    let ghost = ClaimRequest {
        consumer: "plain".into(),
        owner: id("ghost"),
        lease_ms: 30_000,
        limit: 1,
    };
    write(&*s, move |tx| tx.queue().claim_ready(&ghost)).unwrap();
    let wake_at = h.now() + 60_000;
    let delayed = write(&*s, move |tx| {
        let event_id = event(tx, "Delayed")?;
        let id_ = tx.new_id(IdKind::QueueEntry);
        tx.queue().enqueue(NewQueueEntry {
            id: id_,
            consumer: "plain".into(),
            event_id,
            root_id: None,
            operation_id: None,
            partition: None,
            priority: 0,
            ready_at_ms: wake_at,
            payload: json!({}),
        })
    })
    .unwrap();
    let recorder = Recorder::ok();
    let mut c = consumer("effects", recorder.clone());
    c.effect = Some(fx.executor());
    let d = dispatcher(&h, s.clone(), vec![c, consumer("plain", recorder.clone())]);
    runtime().block_on(async {
        d.tick().await.unwrap();
        until(&d, || fx.executed() == 1).await;
        // The machine sleeps for an hour while the effect is in flight.
        h.advance(3_600_000);
        let report = d.tick().await.unwrap();
        assert!(report.woke);
        assert_eq!(report.reasserted, 1);
        assert_eq!(
            report.recovered.requeued,
            std::slice::from_ref(&abandoned.id)
        );
        assert!(
            report.recovered.held.is_empty(),
            "the live effect was not taken away"
        );
        assert_eq!(
            report.claimed, 2,
            "the abandoned and the now-due delayed delivery"
        );
        gate.send(true).unwrap();
        d.idle().await;
    });
    for e in [&effect, &abandoned, &delayed] {
        assert_eq!(entry(&*s, &e.id).state, QueueState::Completed);
    }
    assert_eq!(fx.executed(), 1);
    assert!(kinds(&*s, &effect.id).contains(&"QueueLeaseReasserted".to_owned()));
}

#[test]
fn bounded_fan_out_under_saturation() {
    // A bounded deterministic functional check (not the W08 measurement, which is owned elsewhere):
    // 100 events fanned out to four consumers with 3% poison heads, plus 20 effects, through four
    // worker permits.
    let h = Harness::new();
    let s = h.open();
    let consumers = ["fan-a", "fan-b", "fan-c", "fan-d"];
    write(&*s, move |tx| {
        for i in 0..100 {
            let event_id = event(tx, "Fan")?;
            for c in consumers {
                let id_ = tx.new_id(IdKind::QueueEntry);
                tx.queue().enqueue(NewQueueEntry {
                    id: id_,
                    consumer: c.into(),
                    event_id: event_id.clone(),
                    root_id: None,
                    operation_id: None,
                    partition: Some(format!("agg-{}", i % 10)),
                    priority: 0,
                    ready_at_ms: 0,
                    payload: json!({"i": i, "poison": ([12, 53, 74].contains(&i))}),
                })?;
            }
        }
        for _ in 0..20 {
            let op = intent(tx)?;
            enqueue_tx(tx, "fan-effects", None, Some(op), json!({}))?;
        }
        Ok(())
    })
    .unwrap();
    let recorder = Recorder::new(|d, _| {
        if d.entry.payload["poison"] == true {
            Outcome::Poison("poison".into())
        } else {
            Outcome::Completed
        }
    });
    let fx = Effects::new(EffectResult::Completed(json!({})));
    let mut list: Vec<_> = consumers
        .iter()
        .map(|c| consumer(c, recorder.clone()))
        .collect();
    let mut fc = consumer("fan-effects", recorder.clone());
    fc.effect = Some(fx.executor());
    list.push(fc);
    let d = dispatcher_with(&h, s.clone(), list, |c| c.max_in_flight = 4);
    runtime().block_on(async {
        loop {
            let r = d.tick().await.unwrap();
            assert!(d.in_flight() <= 4);
            if r.claimed == 0 && d.in_flight() == 0 {
                break;
            }
            let mut rx = d.subscribe();
            let _ = tokio::time::timeout(Duration::from_secs(1), rx.changed()).await;
        }
    });
    assert!(fx.max_running.load(std::sync::atomic::Ordering::SeqCst) <= 4);
    assert_eq!(fx.executed(), 20, "each effect executed exactly once");
    let seen = recorder.seen();
    let unique: HashSet<_> = seen.iter().map(|(c, _, seq)| (c.clone(), *seq)).collect();
    assert_eq!(seen.len(), unique.len(), "no duplicate application");
    for c in consumers {
        let status = read(&*s, |r| r.queue().status(c));
        // Each poison event fails visibly at the head of its partition (agg-2/3/4); the later
        // deliveries of exactly those partitions wait behind it, every other one completes.
        assert_eq!(status.failed, 3);
        assert_eq!(status.claimed, 0);
        let blocked = [12, 53, 74].iter().map(|i| (99 - i) / 10).sum::<i64>();
        assert_eq!(status.ready, blocked);
        assert_eq!(status.completed, 100 - 3 - blocked);
    }
}

type HeldWrite = (WriteOptions, Box<dyn WriteJob>);

/// Passes every write through, except that the first one submitted while `entry` is claimed is
/// handed to the test instead of being admitted; the test admits it explicitly. The claim commits
/// before its delivery task exists, so that write is the delivery's first transaction.
struct Interposer {
    inner: Arc<dyn ApplicationStore>,
    hold: Mutex<Option<(Id, tokio::sync::mpsc::UnboundedSender<HeldWrite>)>>,
}
impl Interposer {
    fn new(inner: Arc<dyn ApplicationStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            hold: Mutex::new(None),
        })
    }
    fn hold_while_claimed(&self, entry: &Id) -> tokio::sync::mpsc::UnboundedReceiver<HeldWrite> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        *self.hold.lock().unwrap() = Some((entry.clone(), tx));
        rx
    }
}
impl ApplicationStore for Interposer {
    fn info(&self) -> StoreInfo {
        self.inner.info()
    }
    fn submit_write(
        &self,
        options: WriteOptions,
        job: Box<dyn WriteJob>,
    ) -> Result<(), StorageError> {
        let mut hold = self.hold.lock().unwrap();
        let claimed = hold.as_ref().is_some_and(|(id, _)| {
            read(&*self.inner, |r| r.queue().entry(id))
                .is_some_and(|e| e.state == QueueState::Claimed)
        });
        match hold.take_if(|_| claimed) {
            Some((_, held)) => held
                .send((options, job))
                .map_err(|_| StorageError::Cancelled),
            None => self.inner.submit_write(options, job),
        }
    }
    fn read(
        &self,
        options: &ReadOptions,
        work: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.inner.read(options, work)
    }
}

/// Cooperative cancellation of a claimed delivery, admitted while the dispatcher holds the claim
/// and before its delivery transaction runs. The cancel raises the epoch but leaves the claim
/// and its fence in place; the dispatcher observes the raised epoch at its next check and
/// settles the delivery as cancelled without running the handler or dispatching the effect (an
/// undispatched operation is cancelled with it). The stale claim can no longer settle it.
#[test]
fn a_claimed_delivery_cancelled_before_its_next_check_never_applies_or_dispatches() {
    for with_effect in [false, true] {
        let h = Harness::new();
        let s = h.open();
        let recorder = Recorder::ok();
        let fx = Effects::new(EffectResult::Completed(json!({"remote": "done"})));
        let mut c = consumer("work", recorder.clone());
        if with_effect {
            c.effect = Some(fx.executor());
        }
        let (op, queued) = write(&*s, move |tx| {
            let op = if with_effect { Some(intent(tx)?) } else { None };
            let e = enqueue_tx(tx, "work", None, op.clone(), json!({}))?;
            Ok((op, e))
        })
        .unwrap();
        let interposer = Interposer::new(s.clone());
        let d = dispatcher(&h, interposer.clone(), vec![c]);
        let mut held = interposer.hold_while_claimed(&queued.id);
        let rt = runtime();
        let claimed = rt.block_on(within(
            "a_claimed_delivery_cancelled_before_its_next_check_never_applies_or_dispatches",
            async {
                assert_eq!(d.tick().await.unwrap().claimed, 1);
                let (options, job) = held.recv().await.expect("the delivery's first transaction");
                // The dispatcher holds the claim; nothing of the delivery has run yet.
                let claimed = entry(&*s, &queued.id);
                assert_eq!(claimed.state, QueueState::Claimed);
                assert_eq!(d.in_flight(), 1);
                assert_eq!(recorder.calls.load(Ordering::SeqCst), 0);
                let qid = queued.id.clone();
                let asked = write(&*s, move |tx| tx.queue().cancel(&qid)).unwrap();
                assert_eq!(
                    (asked.state, asked.generation, asked.cancellation_epoch),
                    (
                        QueueState::Claimed,
                        claimed.generation,
                        claimed.cancellation_epoch + 1
                    ),
                    "cancellation asks the owner to stop; it does not revoke the claim"
                );
                s.submit_write(options, job).unwrap();
                d.idle().await;
                claimed
            },
        ));
        let after = entry(&*s, &queued.id);
        assert_eq!(after.state, QueueState::Cancelled, "effect={with_effect}");
        assert_eq!(recorder.calls.load(Ordering::SeqCst), 0, "handler ran");
        assert_eq!(
            fx.executed(),
            0,
            "an effect was dispatched after cancellation"
        );
        if let Some(op) = &op {
            assert_eq!(
                read(&*s, |r| r.operations().operation(op)).unwrap().state,
                OperationState::CancelledBeforeDispatch
            );
        }
        // The claim's fence is spent: a late settlement by its holder is refused.
        let (qid, fence) = (queued.id.clone(), claimed.fence().unwrap());
        let late = write(&*s, move |tx| {
            tx.queue().settle(&qid, &fence, Settlement::Completed)
        });
        assert!(
            matches!(late, Err(StorageError::LeaseLost { .. })),
            "{late:?}"
        );
        assert_eq!(entry(&*s, &queued.id).state, QueueState::Cancelled);
        // Nothing is redelivered.
        assert_eq!(rt.block_on(d.run_until_idle()).unwrap(), 0);
        assert_eq!(
            (recorder.calls.load(Ordering::SeqCst), fx.executed()),
            (0, 0)
        );
    }
}

/// Cancellation admitted while an effect is already dispatched cannot take the effect back: the
/// outcome is recorded from the target's evidence and the delivery is settled by that evidence
/// (applied when the effect completed, held for reconciliation when unknown). The effect is
/// never executed again and the operation is never marked cancelled.
#[test]
fn cancelling_after_dispatch_reconciles_the_effect_instead_of_undoing_it() {
    for result in [
        EffectResult::Completed(json!({"remote": "done"})),
        EffectResult::Unknown("response lost".into()),
    ] {
        let h = Harness::new();
        let s = h.open();
        let recorder = Recorder::ok();
        let fx = Effects::new(result.clone());
        let gate = fx.gated();
        let mut c = consumer("work", recorder.clone());
        c.effect = Some(fx.executor());
        let (op, queued) = write(&*s, |tx| {
            let op = intent(tx)?;
            let e = enqueue_tx(tx, "work", None, Some(op.clone()), json!({}))?;
            Ok((op, e))
        })
        .unwrap();
        let d = dispatcher(&h, s.clone(), vec![c]);
        let rt = runtime();
        rt.block_on(within(
            "cancelling_after_dispatch_reconciles_the_effect_instead_of_undoing_it",
            async {
                assert_eq!(d.tick().await.unwrap().claimed, 1);
                fx.started.notified().await;
                // `Dispatching` is committed and the effect is running under the claim.
                assert_eq!(
                    read(&*s, |r| r.operations().operation(&op)).unwrap().state,
                    OperationState::Dispatching
                );
                let qid = queued.id.clone();
                let asked = write(&*s, move |tx| tx.queue().cancel(&qid)).unwrap();
                assert_eq!(asked.state, QueueState::Claimed);
                gate.send(true).unwrap();
                d.idle().await;
            },
        ));
        let rec = read(&*s, |r| r.operations().operation(&op)).unwrap();
        let after = entry(&*s, &queued.id);
        assert_eq!(fx.executed(), 1, "dispatched exactly once");
        assert_eq!(after.cancellation_epoch, queued.cancellation_epoch + 1);
        match result {
            EffectResult::Completed(_) => {
                assert_eq!(rec.state, OperationState::ObservedComplete);
                assert_eq!(after.state, QueueState::Completed);
                assert_eq!(recorder.calls.load(Ordering::SeqCst), 1);
            }
            _ => {
                assert_eq!(rec.state, OperationState::OutcomeUnknown);
                assert_eq!(after.state, QueueState::Reconciling);
                assert_eq!(recorder.calls.load(Ordering::SeqCst), 0);
                // Observation establishes what happened; it neither redelivers nor re-executes.
                *fx.observation.lock().unwrap() = EffectResult::Completed(json!({"seen": true}));
                assert_eq!(rt.block_on(d.observe_unknown()).unwrap(), 1);
                assert_eq!(
                    read(&*s, |r| r.operations().operation(&op)).unwrap().state,
                    OperationState::ObservedComplete
                );
                assert_eq!(entry(&*s, &queued.id).state, QueueState::Reconciling);
            }
        }
        assert_eq!(rt.block_on(d.run_until_idle()).unwrap(), 0);
        assert_eq!(fx.executed(), 1, "an effect was re-executed");
    }
}

/// One effect delivery for consumer `work`, gated, with a dispatcher over it.
fn gated_effect(
    h: &Harness,
    s: &Arc<dyn ApplicationStore>,
    result: EffectResult,
) -> (
    Effects,
    tokio::sync::watch::Sender<bool>,
    devworks_core::protocol::OperationId,
    QueueEntry,
    Dispatcher,
) {
    let fx = Effects::new(result);
    let gate = fx.gated();
    let mut c = consumer("work", Recorder::ok());
    c.effect = Some(fx.executor());
    let (op, queued) = write(&**s, |tx| {
        let op = intent(tx)?;
        let e = enqueue_tx(tx, "work", None, Some(op.clone()), json!({}))?;
        Ok((op, e))
    })
    .unwrap();
    let d = dispatcher(h, s.clone(), vec![c]);
    (fx, gate, op, queued, d)
}

/// A cancellation admitted while an effect is dispatched, then `NotApplied`: the effect did not
/// happen, so the operation is cancelled with the delivery. It is neither retried nor dispatched
/// again, however long the dispatcher keeps running.
#[test]
fn cancel_during_dispatch_then_not_applied_cancels_instead_of_retrying() {
    let h = Harness::new();
    let s = h.open();
    let (fx, gate, op, queued, d) = gated_effect(&h, &s, EffectResult::NotApplied("503".into()));
    let rt = runtime();
    rt.block_on(within(
        "cancel_during_dispatch_then_not_applied_cancels_instead_of_retrying",
        async {
            assert_eq!(d.tick().await.unwrap().claimed, 1);
            fx.started.notified().await;
            let qid = queued.id.clone();
            let asked = write(&*s, move |tx| tx.queue().cancel(&qid)).unwrap();
            assert!(asked.cancel_requested && asked.state == QueueState::Claimed);
            gate.send(true).unwrap();
            d.idle().await;
        },
    ));
    // Settled by the attempt that saw NotApplied, not left ready for another attempt.
    let settled = entry(&*s, &queued.id);
    assert_eq!(
        settled.state,
        QueueState::Cancelled,
        "{:?}",
        settled.last_error
    );
    h.advance(3_600_000);
    rt.block_on(d.run_until_idle()).unwrap();
    assert_eq!(fx.executed(), 1, "effect re-dispatched after cancellation");
    assert_eq!(entry(&*s, &queued.id).state, QueueState::Cancelled);
    assert_eq!(
        read(&*s, |r| r.operations().operation(&op)).unwrap().state,
        OperationState::CancelledBeforeDispatch
    );
}

/// The cancellation request survives a hold: an unknown outcome is held, observed later as
/// `NotApplied`, and the operator's retry or reconcile then cancels the delivery and its
/// operation instead of redelivering it. The request may arrive during dispatch or while held.
#[test]
fn a_cancel_request_survives_a_hold_and_wins_over_a_later_retry() {
    for (cancel_while_held, resolution, operation) in [
        (false, "Reconcile", "queue.reconcile"),
        (true, "Retry", "queue.retry"),
    ] {
        let h = Harness::new();
        let s = h.open();
        let (fx, gate, op, queued, d) =
            gated_effect(&h, &s, EffectResult::Unknown("response lost".into()));
        let rt = runtime();
        let cancel = |s: &Arc<dyn ApplicationStore>| {
            let qid = queued.id.clone();
            write(&**s, move |tx| tx.queue().cancel(&qid)).unwrap()
        };
        rt.block_on(within(
            "a_cancel_request_survives_a_hold_and_wins_over_a_later_retry",
            async {
                assert_eq!(d.tick().await.unwrap().claimed, 1);
                fx.started.notified().await;
                if !cancel_while_held {
                    cancel(&s);
                }
                gate.send(true).unwrap();
                d.idle().await;
            },
        ));
        let held = entry(&*s, &queued.id);
        assert_eq!(held.state, QueueState::Reconciling, "{resolution}");
        if cancel_while_held {
            // The outcome is unknown: the request is recorded; the delivery stays held.
            let asked = cancel(&s);
            assert_eq!(
                (asked.state, asked.cancel_requested),
                (QueueState::Reconciling, true)
            );
        }
        *fx.observation.lock().unwrap() = EffectResult::NotApplied("not found".into());
        assert_eq!(rt.block_on(d.observe_unknown()).unwrap(), 1);
        let ctx = principal(&*s, "operator", PrincipalKind::Human);
        let now = entry(&*s, &queued.id);
        let reply = resolve_queue(&h, &*s, &ctx, operation, "r-1", &now, resolution);
        assert!(
            matches!(
                reply,
                CommandReply::Recorded {
                    receipt: devworks_core::protocol::Receipt {
                        decision: DomainDecision::Applied { .. },
                        ..
                    },
                    ..
                }
            ),
            "{reply:?}"
        );
        // The resolution itself cancels it: nothing is made ready for another dispatch.
        assert_eq!(entry(&*s, &queued.id).state, QueueState::Cancelled);
        h.advance(3_600_000);
        rt.block_on(d.run_until_idle()).unwrap();
        assert_eq!(entry(&*s, &queued.id).state, QueueState::Cancelled);
        assert_eq!(
            read(&*s, |r| r.operations().operation(&op)).unwrap().state,
            OperationState::CancelledBeforeDispatch
        );
        assert_eq!(fx.executed(), 1, "effect re-dispatched after cancellation");
    }
}

/// Fails the first write submitted after the effect started (the transaction recording its
/// outcome), reporting a storage error to it; everything else passes through.
struct FailOutcomeWrite {
    inner: Arc<dyn ApplicationStore>,
    effects: Effects,
    failed: std::sync::atomic::AtomicBool,
}
struct FailingJob(Box<dyn WriteJob>);
impl WriteJob for FailingJob {
    fn run(&mut self, _: &mut dyn WriteTransaction) -> Result<(), StorageError> {
        Err(StorageError::Busy)
    }
    fn finish(self: Box<Self>, r: Result<(), StorageError>) {
        self.0.finish(r)
    }
}
impl ApplicationStore for FailOutcomeWrite {
    fn info(&self) -> StoreInfo {
        self.inner.info()
    }
    fn submit_write(&self, o: WriteOptions, job: Box<dyn WriteJob>) -> Result<(), StorageError> {
        if self.effects.executed() >= 1 && !self.failed.swap(true, Ordering::SeqCst) {
            return self.inner.submit_write(o, Box::new(FailingJob(job)));
        }
        self.inner.submit_write(o, job)
    }
    fn read(
        &self,
        o: &ReadOptions,
        w: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.inner.read(o, w)
    }
}

/// The last allowed attempt dispatches and then its outcome cannot be recorded. The effect may
/// have happened, so the delivery is held for reconciliation (never `Failed` poison) and its
/// operation is marked `OutcomeUnknown`, which observation can then establish.
#[test]
fn a_possibly_dispatched_delivery_is_held_whatever_its_attempt_budget() {
    let h = Harness::new();
    let s = h.open();
    let fx = Effects::new(EffectResult::Completed(json!({"ok": true})));
    let recorder = Recorder::ok();
    let mut c = consumer("work", recorder.clone());
    c.retry.max_attempts = 1;
    c.effect = Some(fx.executor());
    let (op, queued) = write(&*s, |tx| {
        let op = intent(tx)?;
        let e = enqueue_tx(tx, "work", None, Some(op.clone()), json!({}))?;
        Ok((op, e))
    })
    .unwrap();
    let failing: Arc<dyn ApplicationStore> = Arc::new(FailOutcomeWrite {
        inner: s.clone(),
        effects: fx.clone(),
        failed: Default::default(),
    });
    let d = dispatcher(&h, failing, vec![c]);
    let rt = runtime();
    rt.block_on(d.run_until_idle()).unwrap();
    let held = entry(&*s, &queued.id);
    assert_eq!(held.state, QueueState::Reconciling, "{:?}", held.last_error);
    assert_eq!(
        read(&*s, |r| r.operations().operation(&op)).unwrap().state,
        OperationState::OutcomeUnknown
    );
    *fx.observation.lock().unwrap() = EffectResult::Completed(json!({"seen": true}));
    assert_eq!(rt.block_on(d.observe_unknown()).unwrap(), 1);
    let qid = queued.id.clone();
    write(&*s, move |tx| tx.queue().requeue(&qid, 0)).unwrap();
    rt.block_on(d.run_until_idle()).unwrap();
    assert_eq!(entry(&*s, &queued.id).state, QueueState::Completed);
    assert_eq!(fx.executed(), 1, "the effect ran exactly once");
    assert_eq!(recorder.seen().len(), 1);
}

/// A cancelled claim whose operation was already preflighted (an attempt started, the effect
/// not yet dispatched) cancels the operation too instead of orphaning it.
#[test]
fn cancelling_a_claim_with_a_preflighted_operation_cancels_the_operation() {
    let h = Harness::new();
    let s = h.open();
    let fx = Effects::new(EffectResult::Completed(json!({})));
    let mut c = consumer("work", Recorder::ok());
    c.effect = Some(fx.executor());
    let (op, queued) = write(&*s, |tx| {
        let op = intent(tx)?;
        let e = enqueue_tx(tx, "work", None, Some(op.clone()), json!({}))?;
        let rec = tx.operations().operation(&op)?.unwrap();
        tx.operations()
            .begin_attempt(&op, rec.revision, AttemptBinding::default())?;
        Ok((op, e))
    })
    .unwrap();
    assert_eq!(
        read(&*s, |r| r.operations().operation(&op)).unwrap().state,
        OperationState::Preflighted
    );
    let interposer = Interposer::new(s.clone());
    let d = dispatcher(&h, interposer.clone(), vec![c]);
    let mut held = interposer.hold_while_claimed(&queued.id);
    runtime().block_on(within(
        "cancelling_a_claim_with_a_preflighted_operation_cancels_the_operation",
        async {
            assert_eq!(d.tick().await.unwrap().claimed, 1);
            let (options, job) = held.recv().await.expect("the delivery's first transaction");
            let qid = queued.id.clone();
            write(&*s, move |tx| tx.queue().cancel(&qid)).unwrap();
            s.submit_write(options, job).unwrap();
            d.idle().await;
        },
    ));
    assert_eq!(entry(&*s, &queued.id).state, QueueState::Cancelled);
    assert_eq!(
        read(&*s, |r| r.operations().operation(&op)).unwrap().state,
        OperationState::CancelledBeforeDispatch
    );
    assert_eq!(fx.executed(), 0);
}

/// Unknown outcomes are observed across every page of held deliveries, not only the first.
#[test]
fn observing_unknown_outcomes_walks_every_page() {
    let h = Harness::new();
    let s = h.open();
    let fx = Effects::new(EffectResult::Unknown("lost".into()));
    let mut c = consumer("work", Recorder::ok());
    c.effect = Some(fx.executor());
    for _ in 0..5 {
        write(&*s, |tx| {
            let op = intent(tx)?;
            enqueue_tx(tx, "work", None, Some(op), json!({}))
        })
        .unwrap();
    }
    let d = dispatcher_with(&h, s.clone(), vec![c], |c| c.recovery_batch = 2);
    let rt = runtime();
    rt.block_on(d.run_until_idle()).unwrap();
    assert_eq!(read(&*s, |r| r.queue().status("work")).reconciling, 5);
    *fx.observation.lock().unwrap() = EffectResult::Completed(json!({"seen": true}));
    assert_eq!(rt.block_on(d.observe_unknown()).unwrap(), 5);
    assert_eq!(fx.observed.load(Ordering::SeqCst), 5);
}

/// The root-control layer's view of which roots are held.
struct Roots(Mutex<HashSet<String>>);
impl devworks_engine::RootEligibility for Roots {
    fn may_resume(
        &self,
        _: &mut dyn WriteTransaction,
        e: &QueueEntry,
    ) -> Result<bool, StorageError> {
        let root = e.root_id.as_ref().expect("root-scoped").0.as_str();
        Ok(!self.0.lock().unwrap().contains(root))
    }
}

/// Root-scoped work claimed by a process that died is not continued automatically on the next
/// boot: it is held awaiting root eligibility while root-less work proceeds. Without an
/// eligibility port it stays held; with one, it resumes only once its root is no longer held.
#[test]
fn root_scoped_work_of_an_earlier_boot_waits_for_root_eligibility() {
    let h = Harness::new();
    let s = h.open();
    seed_root(&h, "root-a");
    let (rooted, free) = write(&*s, |tx| {
        let event_id = event(tx, "Changed")?;
        let rid = tx.new_id(IdKind::QueueEntry);
        let rooted = tx.queue().enqueue(NewQueueEntry {
            id: rid,
            consumer: "work".into(),
            event_id,
            root_id: Some(devworks_core::protocol::RootId(id("root-a"))),
            operation_id: None,
            partition: None,
            priority: 0,
            ready_at_ms: 0,
            payload: json!({}),
        })?;
        let free = enqueue_tx(tx, "work", None, None, json!({}))?;
        Ok((rooted, free))
    })
    .unwrap();
    let ghost = ClaimRequest {
        consumer: "work".into(),
        owner: id("ghost"),
        lease_ms: 3_600_000,
        limit: 10,
    };
    assert_eq!(
        write(&*s, move |tx| tx.queue().claim_ready(&ghost))
            .unwrap()
            .len(),
        2
    );
    drop(s);

    // Next boot, no eligibility port: the safe default holds the root-scoped work.
    let s = h.open();
    let recorder = Recorder::ok();
    let d = dispatcher(&h, s.clone(), vec![consumer("work", recorder.clone())]);
    let rt = runtime();
    let report = rt.block_on(d.recover()).unwrap();
    assert_eq!(report.held, std::slice::from_ref(&rooted.id));
    assert_eq!(report.requeued, std::slice::from_ref(&free.id));
    rt.block_on(d.run_until_idle()).unwrap();
    assert_eq!(entry(&*s, &free.id).state, QueueState::Completed);
    let held = entry(&*s, &rooted.id);
    assert_eq!(
        (held.state, held.awaiting_eligibility),
        (QueueState::Reconciling, true)
    );
    assert_eq!(recorder.seen().len(), 1);

    // With the root-control layer's port: not while the root is held, then once released.
    let roots = Arc::new(Roots(Mutex::new(HashSet::from(["root-a".to_owned()]))));
    let mut c = consumer("work", recorder.clone());
    c.eligibility = Some(roots.clone());
    let d = dispatcher(&h, s.clone(), vec![c]);
    rt.block_on(d.run_until_idle()).unwrap();
    assert_eq!(entry(&*s, &rooted.id).state, QueueState::Reconciling);
    assert_eq!(recorder.seen().len(), 1, "held root work was dispatched");
    roots.0.lock().unwrap().clear();
    let report = rt.block_on(d.tick()).unwrap();
    assert_eq!(report.resumed, 1);
    rt.block_on(async {
        d.idle().await;
        d.run_until_idle().await.unwrap();
    });
    assert_eq!(entry(&*s, &rooted.id).state, QueueState::Completed);
    assert_eq!(recorder.seen().len(), 2);
}

/// A sleep the dispatcher cannot see (a monotonic clock that kept counting through suspend)
/// falls back to expired-lease recovery: the live effect's delivery is held, the late result is
/// recorded against the operation, and reconciliation completes it without re-executing.
#[test]
fn an_undetected_sleep_holds_the_live_effect_and_never_re_executes_it() {
    let h = Harness::new();
    let s = h.open();
    let fx = Effects::new(EffectResult::Completed(json!({"ok": true})));
    let gate = fx.gated();
    let recorder = Recorder::ok();
    let mut c = consumer("work", recorder.clone());
    c.effect = Some(fx.executor());
    let (op, queued) = write(&*s, |tx| {
        let op = intent(tx)?;
        let e = enqueue_tx(tx, "work", None, Some(op.clone()), json!({}))?;
        Ok((op, e))
    })
    .unwrap();
    // No wall-clock jump is ever treated as a sleep.
    let d = dispatcher_with(&h, s.clone(), vec![c], |c| c.wake_jump_ms = i64::MAX);
    let rt = runtime();
    rt.block_on(within(
        "an_undetected_sleep_holds_the_live_effect_and_never_re_executes_it",
        async {
            assert_eq!(d.tick().await.unwrap().claimed, 1);
            fx.started.notified().await;
            h.advance(3_600_000);
            let report = d.tick().await.unwrap();
            assert!(!report.woke);
            assert_eq!(report.recovered.held, std::slice::from_ref(&queued.id));
            gate.send(true).unwrap();
            d.idle().await;
        },
    ));
    assert_eq!(entry(&*s, &queued.id).state, QueueState::Reconciling);
    assert_eq!(
        read(&*s, |r| r.operations().operation(&op)).unwrap().state,
        OperationState::ObservedComplete,
        "the late result is recorded"
    );
    let ctx = principal(&*s, "operator", PrincipalKind::Human);
    let held = entry(&*s, &queued.id);
    resolve_queue(&h, &*s, &ctx, "queue.reconcile", "r-1", &held, "Reconcile");
    rt.block_on(d.run_until_idle()).unwrap();
    assert_eq!(entry(&*s, &queued.id).state, QueueState::Completed);
    assert_eq!(fx.executed(), 1, "an effect was re-executed");
    assert_eq!(recorder.seen().len(), 1);
}

/// A claim whose cancellation was requested before its process died is settled `Cancelled` by
/// the next boot's recovery, with its undispatched operation, instead of being redelivered.
#[test]
fn a_cancel_requested_before_a_crash_is_honoured_by_recovery() {
    let h = Harness::new();
    let s = h.open();
    let queued = write(&*s, |tx| {
        let op = intent(tx)?;
        enqueue_tx(tx, "work", None, Some(op), json!({}))
    })
    .unwrap();
    let ghost = ClaimRequest {
        consumer: "work".into(),
        owner: id("ghost"),
        lease_ms: 3_600_000,
        limit: 1,
    };
    write(&*s, move |tx| tx.queue().claim_ready(&ghost)).unwrap();
    let qid = queued.id.clone();
    write(&*s, move |tx| tx.queue().cancel(&qid)).unwrap();
    drop(s);
    let s = h.open();
    let recorder = Recorder::ok();
    let fx = Effects::new(EffectResult::Completed(json!({})));
    let mut c = consumer("work", recorder.clone());
    c.effect = Some(fx.executor());
    let d = dispatcher(&h, s.clone(), vec![c]);
    let rt = runtime();
    let report = rt.block_on(d.recover()).unwrap();
    assert_eq!(report.cancelled, std::slice::from_ref(&queued.id));
    rt.block_on(d.run_until_idle()).unwrap();
    assert_eq!(entry(&*s, &queued.id).state, QueueState::Cancelled);
    let op = queued.operation_id.clone().unwrap();
    assert_eq!(
        read(&*s, |r| r.operations().operation(&op)).unwrap().state,
        OperationState::CancelledBeforeDispatch
    );
    assert_eq!((fx.executed(), recorder.seen().len()), (0, 0));
}
