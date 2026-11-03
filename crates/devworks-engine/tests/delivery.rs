//! Durable delivery against the real SQLite adapter: claims, fences, ordering, retries, poison,
//! wakeups, sleep/wake and restart recovery (DW-003 AC1, CAP-QUEUE).
mod common;
use common::*;
use devworks_core::context::PrincipalKind;
use devworks_core::gateway::Gateway;
use devworks_core::ports::IdKind;
use devworks_core::ports::storage::*;
use devworks_core::protocol::results::DomainDecision;
use devworks_core::protocol::{CommandReply, OperationState};
use devworks_core::services::UseCases;
use devworks_engine::{Dispatcher, EffectResult, Outcome};
use serde_json::json;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
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

#[test]
fn a_missing_wakeup_is_repaired_by_the_bounded_sweep() {
    // Real time with short bounds: a delivery committed without any wakeup is found by the
    // sweep; with a long sweep it is found promptly only when woken.
    let h = Harness::new();
    let s = h.open();
    let swept = Recorder::ok();
    let woken = Recorder::ok();
    let sweeping = dispatcher_with(&h, s.clone(), vec![consumer("sweep", swept.clone())], |c| {
        c.owner = id("sweeper");
        c.sweep_interval = Duration::from_millis(100);
    });
    let sleeping = dispatcher_with(&h, s.clone(), vec![consumer("wake", woken.clone())], |c| {
        c.owner = id("sleeper");
        c.sweep_interval = Duration::from_secs(3_600);
    });
    runtime().block_on(async {
        let loops: Vec<_> = [sweeping.clone(), sleeping.clone()]
            .into_iter()
            .map(|d| tokio::spawn(async move { d.run().await }))
            .collect();
        // Both loops have finished a pass over an empty queue; the next pass of the sleeping
        // one needs a wakeup (its sweep is an hour away).
        for d in [&sweeping, &sleeping] {
            let d2 = d.clone();
            until(d, move || d2.passes() >= 1).await;
        }
        let start = std::time::Instant::now();
        enqueue(&*s, "sweep", None, json!({}));
        let r = swept.clone();
        until(&sweeping, move || r.seen().len() == 1).await;
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "bounded by the sweep"
        );

        enqueue(&*s, "wake", None, json!({}));
        let quiet = tokio::time::timeout(Duration::from_millis(300), async {
            let mut rx = sleeping.subscribe();
            let _ = rx.changed().await;
        })
        .await;
        assert!(
            quiet.is_err() && woken.seen().is_empty(),
            "no wakeup, long sweep: still waiting"
        );
        sleeping.wakeup().wake();
        let r = woken.clone();
        until(&sleeping, move || r.seen().len() == 1).await;
        for d in [&sweeping, &sleeping] {
            d.stop();
        }
        for l in loops {
            l.await.unwrap();
        }
    });
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
    // A bounded deterministic functional check (not the W08 measurement, owned by DW-061):
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
