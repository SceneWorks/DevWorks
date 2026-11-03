//! `queue.*` through the shared application gateway on the real store: the same use
//! cases for a UI principal and an agent tool, receipts, revision/fence checks, refusals and the
//! retained CodeGraph `GetJobQueue`/`DismissJob` behavior.
mod common;
use common::*;
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::gateway::{Gateway, GatewayFailure};
use devworks_core::ports::IdKind;
use devworks_core::ports::storage::*;
use devworks_core::protocol::records::QueueDelivery;
use devworks_core::protocol::results::{DomainDecision, DomainQueryReply, OperationResult};
use devworks_core::protocol::{
    ActorId, CommandReply, DomainErrorCode, EventId, Id, OperationState, Receipt, Rejection,
};
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::replies::REPLY_CONSUMER;
use devworks_core::services::queue::REPLY_REQUIRED;
use devworks_engine::{EffectResult, Outcome};
use serde_json::json;
use std::sync::Arc;

struct Fixture {
    h: Harness,
    s: Arc<dyn ApplicationStore>,
    cases: UseCases,
    operator: InvocationContext,
    agent: InvocationContext,
}
impl Fixture {
    fn new() -> Self {
        let h = Harness::new();
        let s = h.open();
        let operator = principal(&*s, "operator", PrincipalKind::Human);
        let agent = principal(&*s, "agent-1", PrincipalKind::Agent);
        Self {
            h,
            s,
            cases: UseCases::new().with_queue(),
            operator,
            agent,
        }
    }
    fn command(
        &self,
        ctx: &InvocationContext,
        op: &str,
        key: &str,
        fields: &str,
    ) -> CommandReply<DomainDecision> {
        let body = format!(
            r#"{{"mode":"Command","protocol":"1.0-draft.1","requestKey":"{key}","body":{{"kind":"{op}",{fields}}}}}"#
        );
        let ids = self.h.ids.clone();
        let gateway = Gateway {
            authority: &Authority,
            store: &*self.s,
            ids: &*ids,
        };
        self.cases
            .command(&gateway, ctx, op, body.as_bytes())
            .expect("addressed reply")
    }
    fn query(
        &self,
        ctx: &InvocationContext,
        op: &str,
        key: &str,
        fields: &str,
    ) -> DomainQueryReply {
        let body = format!(
            r#"{{"mode":"Query","protocol":"1.0-draft.1","queryId":"{key}","body":{{"kind":"{op}"{}{fields}}}}}"#,
            if fields.is_empty() { "" } else { "," }
        );
        let ids = self.h.ids.clone();
        let gateway = Gateway {
            authority: &Authority,
            store: &*self.s,
            ids: &*ids,
        };
        self.cases
            .query(&gateway, ctx, op, body.as_bytes())
            .expect("addressed reply")
    }
    fn resolution(&self, e: &QueueEntry, resolution: &str) -> String {
        format!(
            r#""deliveryId":"{}","expectedRevision":"{}","expectedFence":"{}","resolution":"{resolution}","reason":"operator decision""#,
            e.id.as_str(),
            e.revision,
            e.generation
        )
    }
}

fn applied(reply: &CommandReply<DomainDecision>) -> (QueueDelivery, bool) {
    match reply {
        CommandReply::Recorded {
            receipt:
                Receipt {
                    decision: DomainDecision::Applied { result },
                    ..
                },
            replayed,
            ..
        } => {
            let value = match result {
                OperationResult::QueueRetry { value }
                | OperationResult::QueueResolve { value }
                | OperationResult::QueueReconcile { value } => value.clone(),
                other => panic!("unexpected result {other:?}"),
            };
            (value, *replayed)
        }
        other => panic!("not applied: {other:?}"),
    }
}
fn refused(reply: &CommandReply<DomainDecision>) -> DomainErrorCode {
    match reply {
        CommandReply::Recorded {
            receipt:
                Receipt {
                    decision: DomainDecision::Refused { fault },
                    ..
                },
            ..
        } => fault.code,
        other => panic!("not refused: {other:?}"),
    }
}
fn page(reply: &DomainQueryReply) -> devworks_core::protocol::support::Page<QueueDelivery> {
    match reply {
        DomainQueryReply::Result {
            result: OperationResult::QueueInspect { value },
            ..
        } => value.clone(),
        other => panic!("not a page: {other:?}"),
    }
}

/// Builds one failed (poison), one held (unknown effect outcome) and one queued delivery.
fn scenario(f: &Fixture) -> (QueueEntry, QueueEntry, QueueEntry, Effects) {
    let poison = enqueue(&*f.s, "jobs", None, json!({"bad": true}));
    let fx = Effects::new(EffectResult::Unknown("connection reset after send".into()));
    let held = write(&*f.s, |tx| {
        let op = intent(tx)?;
        enqueue_tx(tx, "effects", None, Some(op), json!({}))
    })
    .unwrap();
    let handler = Recorder::new(|d, _| {
        if d.entry.payload["bad"] == true {
            Outcome::Poison("unsupported payload".into())
        } else {
            Outcome::Completed
        }
    });
    let mut c = consumer("effects", handler.clone());
    c.effect = Some(fx.executor());
    let d = dispatcher(&f.h, f.s.clone(), vec![consumer("jobs", handler), c]);
    runtime().block_on(d.run_until_idle()).unwrap();
    let queued = enqueue(&*f.s, "jobs", None, json!({}));
    (
        entry(&*f.s, &poison.id),
        entry(&*f.s, &held.id),
        entry(&*f.s, &queued.id),
        fx,
    )
}

#[test]
fn inspection_and_status_are_shared_by_ui_and_agent_tools() {
    let f = Fixture::new();
    let (poison, held, queued, _) = scenario(&f);
    assert_eq!(poison.state, QueueState::Failed);
    assert_eq!(held.state, QueueState::Reconciling);
    let ui = page(&f.query(
        &f.operator,
        "queue.inspect",
        "q-ui",
        r#""page":{"limit":50}"#,
    ));
    let tool = page(&f.query(
        &f.agent,
        "queue.inspect",
        "q-tool",
        r#""page":{"limit":50}"#,
    ));
    let ids = |p: &devworks_core::protocol::support::Page<QueueDelivery>| {
        p.items.0.iter().map(|d| d.id.clone()).collect::<Vec<_>>()
    };
    assert_eq!(ids(&ui), ids(&tool));
    // Retained GetJobQueue order: failures/holds newest first, then queued work; successful
    // work is not listed.
    assert_eq!(
        ids(&ui),
        [held.id.clone(), poison.id.clone(), queued.id.clone()]
    );
    assert_eq!(ui.items.0[0].state, OperationState::OutcomeUnknown);
    assert_eq!(
        ui.items.0[0].error.as_ref().map(|e| e.0.as_str()),
        Some("connection reset after send")
    );
    assert_eq!(ui.items.0[1].state, OperationState::Failed);
    assert_eq!(ui.items.0[2].state, OperationState::IntentRecorded);

    // Keyset paging with a filter-bound cursor.
    let first = page(&f.query(
        &f.operator,
        "queue.inspect",
        "q-p1",
        r#""consumerId":"jobs","page":{"limit":1}"#,
    ));
    assert!(first.has_more);
    let cursor = first.next_cursor.clone().unwrap().0;
    let second = page(&f.query(
        &f.operator,
        "queue.inspect",
        "q-p2",
        &format!(r#""consumerId":"jobs","page":{{"limit":1,"cursor":"{cursor}"}}"#),
    ));
    assert_eq!(
        [first.items.0[0].id.clone(), second.items.0[0].id.clone()],
        [poison.id.clone(), queued.id.clone()]
    );
    assert!(!second.has_more);
    let other_filter = f.query(
        &f.operator,
        "queue.inspect",
        "q-p3",
        &format!(r#""consumerId":"effects","page":{{"limit":1,"cursor":"{cursor}"}}"#),
    );
    assert!(
        matches!(other_filter, DomainQueryReply::Rejected { fault, .. } if fault.code == DomainErrorCode::CursorExpired)
    );
    let garbage = f.query(
        &f.operator,
        "queue.inspect",
        "q-p4",
        r#""page":{"limit":1,"cursor":"nonsense"}"#,
    );
    assert!(
        matches!(garbage, DomainQueryReply::Rejected { fault, .. } if fault.code == DomainErrorCode::InvalidArgument)
    );

    // Retained running/queued/failed counters.
    let status = f.query(
        &f.agent,
        "queue.consumer_status",
        "q-s",
        r#""consumerId":"jobs""#,
    );
    let DomainQueryReply::Result {
        result: OperationResult::QueueConsumerStatus { value },
        ..
    } = status
    else {
        panic!("status")
    };
    assert_eq!(
        (
            value.pending_count.get(),
            value.leased_count.get(),
            value.poison_count.get()
        ),
        (1, 0, 1)
    );
    assert_eq!(
        value.cursor.get(),
        0,
        "the watermark stops before the failed delivery"
    );

    // An unregistered operation is not reachable through the registry.
    let ids2 = f.h.ids.clone();
    let gateway = Gateway {
        authority: &Authority,
        store: &*f.s,
        ids: &*ids2,
    };
    assert!(matches!(
        f.cases.query(&gateway, &f.operator, "projects.get", b"{}"),
        Err(GatewayFailure::NoUseCase(_))
    ));
}

#[test]
fn retry_checks_revision_fence_binding_authority_and_replays_its_receipt() {
    let f = Fixture::new();
    let (poison, held, _, _) = scenario(&f);
    let mut stale = poison.clone();
    stale.revision -= 1;
    assert!(matches!(
        refused(&f.command(
            &f.operator,
            "queue.retry",
            "r-stale",
            &f.resolution(&stale, "Retry")
        )),
        DomainErrorCode::RevisionConflict
    ));
    let mut fenced = poison.clone();
    fenced.generation += 7;
    assert_eq!(
        refused(&f.command(
            &f.operator,
            "queue.retry",
            "r-fence",
            &f.resolution(&fenced, "Retry")
        )),
        DomainErrorCode::LeaseLost
    );
    // The recorded selected-operation rule binds `queue.retry` to `Retry` at admission: no
    // receipt is minted for a mismatched resolution.
    assert!(matches!(
        f.command(
            &f.operator,
            "queue.retry",
            "r-bind",
            &f.resolution(&poison, "Reconcile")
        ),
        CommandReply::Rejected {
            code: Rejection::InvalidRequest,
            ..
        }
    ));
    // An agent tool cannot perform host maintenance.
    assert!(matches!(
        f.command(
            &f.agent,
            "queue.retry",
            "r-agent",
            &f.resolution(&poison, "Retry")
        ),
        CommandReply::Rejected {
            code: Rejection::PermissionDenied,
            ..
        }
    ));
    // Refusals persisted only their receipts.
    assert_eq!(entry(&*f.s, &poison.id), poison);
    // An unknown effect cannot be retried blindly.
    assert_eq!(
        refused(&f.command(
            &f.operator,
            "queue.retry",
            "r-held",
            &f.resolution(&held, "Retry")
        )),
        DomainErrorCode::OutcomeUnknown
    );
    assert_eq!(entry(&*f.s, &held.id).state, QueueState::Reconciling);

    let body = f.resolution(&poison, "Retry");
    let (delivery, replayed) = applied(&f.command(&f.operator, "queue.retry", "r-ok", &body));
    assert!(!replayed);
    assert_eq!(delivery.state, OperationState::IntentRecorded);
    assert_eq!(
        delivery.error.as_ref().map(|e| e.0.as_str()),
        Some("unsupported payload")
    );
    let (again, replayed) = applied(&f.command(&f.operator, "queue.retry", "r-ok", &body));
    assert!(replayed);
    assert_eq!(again.revision, delivery.revision);
    let resolved = read(&*f.s, |r| {
        r.history().page(
            &EventFilter::Aggregate {
                kind: "queue_entry".into(),
                id: poison.id.as_str().into(),
            },
            0,
            500,
        )
    })
    .events
    .into_iter()
    .filter(|e| e.kind == "QueueDeliveryResolved")
    .count();
    assert_eq!(resolved, 1, "a replay never repeats the resolution");
    assert_eq!(entry(&*f.s, &poison.id).state, QueueState::Ready);
}

#[test]
fn dismissal_preserves_the_source_semantics() {
    let f = Fixture::new();
    let (poison, held, queued, _) = scenario(&f);
    let (dismissed, _) = applied(&f.command(
        &f.operator,
        "queue.resolve",
        "d-1",
        &f.resolution(&poison, "AcknowledgePoison"),
    ));
    assert_eq!(dismissed.state, OperationState::Failed);
    assert_eq!(entry(&*f.s, &poison.id).state, QueueState::Dismissed);
    // Dismissing again, or anything not failed, is not-found (the source's false/404).
    let now = entry(&*f.s, &poison.id);
    for (key, e) in [("d-2", &now), ("d-3", &queued)] {
        assert_eq!(
            refused(&f.command(
                &f.operator,
                "queue.resolve",
                key,
                &f.resolution(e, "AcknowledgePoison")
            )),
            DomainErrorCode::NotFound
        );
    }
    // A held effect cannot be dismissed while its outcome is unknown.
    assert_eq!(
        refused(&f.command(
            &f.operator,
            "queue.resolve",
            "d-4",
            &f.resolution(&held, "AcknowledgePoison")
        )),
        DomainErrorCode::OutcomeUnknown
    );
    let unknown = r#""deliveryId":"no-such-delivery","expectedRevision":"1","expectedFence":"0","resolution":"AcknowledgePoison","reason":"x""#;
    assert_eq!(
        refused(&f.command(&f.operator, "queue.resolve", "d-5", unknown)),
        DomainErrorCode::NotFound
    );
    // The dismissed delivery leaves the queue listing.
    let listed = page(&f.query(
        &f.operator,
        "queue.inspect",
        "q",
        r#""consumerId":"jobs","page":{"limit":50}"#,
    ));
    assert_eq!(listed.items.0.len(), 1);
    assert_eq!(listed.items.0[0].id, queued.id);
}

#[test]
fn reconcile_requires_an_established_outcome_and_cancel_only_before_dispatch() {
    let f = Fixture::new();
    let (_, held, queued, fx) = scenario(&f);
    let reply = f.command(
        &f.operator,
        "queue.reconcile",
        "c-1",
        &f.resolution(&held, "Reconcile"),
    );
    assert_eq!(refused(&reply), DomainErrorCode::OutcomeUnknown);
    let CommandReply::Recorded { receipt, .. } = &reply else {
        panic!()
    };
    assert_eq!(
        receipt.operation_id, held.operation_id,
        "refusal names the uncertain operation"
    );
    // The executor observes the target; the outcome is then established and reconcile proceeds.
    *fx.observation.lock().unwrap() = EffectResult::Completed(json!({"found": true}));
    let handler = Recorder::ok();
    let mut c = consumer("effects", handler.clone());
    c.effect = Some(fx.executor());
    let d = dispatcher(&f.h, f.s.clone(), vec![c]);
    let rt = runtime();
    assert_eq!(rt.block_on(d.observe_unknown()).unwrap(), 1);
    let now = entry(&*f.s, &held.id);
    let (delivery, _) = applied(&f.command(
        &f.operator,
        "queue.reconcile",
        "c-2",
        &f.resolution(&now, "Reconcile"),
    ));
    assert_eq!(delivery.state, OperationState::RetryPending);
    rt.block_on(d.run_until_idle()).unwrap();
    assert_eq!(entry(&*f.s, &held.id).state, QueueState::Completed);
    assert_eq!(
        fx.executed(),
        1,
        "reconciliation never re-executes the effect"
    );
    assert_eq!(handler.seen().len(), 1);

    // Cancel before dispatch: a queued delivery and its undispatched operation.
    let pending = write(&*f.s, |tx| {
        let op = intent(tx)?;
        enqueue_tx(tx, "effects", None, Some(op), json!({}))
    })
    .unwrap();
    let (cancelled, _) = applied(&f.command(
        &f.operator,
        "queue.resolve",
        "x-1",
        &f.resolution(&pending, "CancelBeforeDispatch"),
    ));
    assert_eq!(cancelled.state, OperationState::CancelledBeforeDispatch);
    let op = pending.operation_id.clone().unwrap();
    assert_eq!(
        read(&*f.s, |r| r.operations().operation(&op))
            .unwrap()
            .state,
        OperationState::CancelledBeforeDispatch
    );
    // A completed delivery cannot be cancelled.
    let done = entry(&*f.s, &held.id);
    assert_eq!(
        refused(&f.command(
            &f.operator,
            "queue.resolve",
            "x-2",
            &f.resolution(&done, "CancelBeforeDispatch")
        )),
        DomainErrorCode::Conflict
    );
    let _ = queued;
}

/// `CancelBeforeDispatch` applies only before dispatch: a held delivery whose operation already
/// has an outcome (`ObservedComplete` or `Failed`) is a `Conflict` and keeps its outcome, while
/// a preflighted (started, not dispatched) operation is cancelled with its delivery.
#[test]
fn cancel_before_dispatch_is_refused_once_the_operation_has_an_outcome() {
    for (observed, op_state) in [
        (
            EffectResult::Completed(json!({"found": true})),
            OperationState::ObservedComplete,
        ),
        (
            EffectResult::Failed("rejected".into()),
            OperationState::Failed,
        ),
    ] {
        let f = Fixture::new();
        let (_, held, _, fx) = scenario(&f);
        *fx.observation.lock().unwrap() = observed;
        let mut c = consumer("effects", Recorder::ok());
        c.effect = Some(fx.executor());
        let d = dispatcher(&f.h, f.s.clone(), vec![c]);
        assert_eq!(runtime().block_on(d.observe_unknown()).unwrap(), 1);
        let op = held.operation_id.clone().unwrap();
        assert_eq!(
            read(&*f.s, |r| r.operations().operation(&op))
                .unwrap()
                .state,
            op_state
        );
        let now = entry(&*f.s, &held.id);
        assert_eq!(now.state, QueueState::Reconciling);
        assert_eq!(
            refused(&f.command(
                &f.operator,
                "queue.resolve",
                "c-1",
                &f.resolution(&now, "CancelBeforeDispatch")
            )),
            DomainErrorCode::Conflict
        );
        assert_eq!(entry(&*f.s, &held.id).state, QueueState::Reconciling);
        assert_eq!(
            read(&*f.s, |r| r.operations().operation(&op))
                .unwrap()
                .state,
            op_state
        );
    }

    let f = Fixture::new();
    let preflighted = write(&*f.s, |tx| {
        let op = intent(tx)?;
        let e = enqueue_tx(tx, "effects", None, Some(op.clone()), json!({}))?;
        let rec = tx.operations().operation(&op)?.unwrap();
        tx.operations()
            .begin_attempt(&op, rec.revision, AttemptBinding::default())?;
        Ok(e)
    })
    .unwrap();
    let (cancelled, _) = applied(&f.command(
        &f.operator,
        "queue.resolve",
        "c-2",
        &f.resolution(&preflighted, "CancelBeforeDispatch"),
    ));
    assert_eq!(cancelled.state, OperationState::CancelledBeforeDispatch);
    let op = preflighted.operation_id.clone().unwrap();
    assert_eq!(
        read(&*f.s, |r| r.operations().operation(&op))
            .unwrap()
            .state,
        OperationState::CancelledBeforeDispatch
    );
}

/// Failures and holds are listed newest first by when they failed (the retained source's
/// order), not by event sequence: a delivery for an older event that failed later comes first.
#[test]
fn failures_are_listed_by_failure_time_newest_first() {
    let f = Fixture::new();
    let older = enqueue(&*f.s, "jobs", None, json!({}));
    let newer = enqueue(&*f.s, "jobs", None, json!({}));
    let claims = claim_all(&f.s, "jobs");
    let fence = |e: &QueueEntry| {
        claims
            .iter()
            .find(|c| c.id == e.id)
            .unwrap()
            .fence()
            .unwrap()
    };
    let (id, fc) = (newer.id.clone(), fence(&newer));
    write(&*f.s, move |tx| {
        tx.queue()
            .fail(&id, &fc, "first failure", FailureDisposition::Terminal)
    })
    .unwrap();
    f.h.advance(1_000);
    let (id, fc) = (older.id.clone(), fence(&older));
    write(&*f.s, move |tx| {
        tx.queue()
            .fail(&id, &fc, "later failure", FailureDisposition::Terminal)
    })
    .unwrap();
    let listed = page(&f.query(
        &f.operator,
        "queue.inspect",
        "q-1",
        r#""consumerId":"jobs","page":{"limit":50}"#,
    ));
    let ids: Vec<_> = listed.items.0.iter().map(|d| d.id.clone()).collect();
    assert_eq!(ids, [older.id.clone(), newer.id.clone()]);
    assert!(older.event_sequence < newer.event_sequence);
    // Keyset paging follows the same order.
    let first = page(&f.query(
        &f.operator,
        "queue.inspect",
        "q-2",
        r#""consumerId":"jobs","page":{"limit":1}"#,
    ));
    let cursor = first.next_cursor.clone().unwrap().0;
    let second = page(&f.query(
        &f.operator,
        "queue.inspect",
        "q-3",
        &format!(r#""consumerId":"jobs","page":{{"limit":1,"cursor":"{cursor}"}}"#),
    ));
    assert_eq!(
        [first.items.0[0].id.clone(), second.items.0[0].id.clone()],
        [older.id.clone(), newer.id.clone()]
    );
}

fn claim_all(s: &Arc<dyn ApplicationStore>, consumer: &str) -> Vec<QueueEntry> {
    let request = ClaimRequest {
        consumer: consumer.into(),
        owner: id("worker"),
        lease_ms: 60_000,
        limit: 10,
    };
    write(&**s, move |tx| tx.queue().claim_ready(&request)).unwrap()
}

fn fault(reply: &CommandReply<DomainDecision>) -> devworks_core::protocol::DomainFault {
    match reply {
        CommandReply::Recorded {
            receipt:
                Receipt {
                    decision: DomainDecision::Refused { fault },
                    ..
                },
            ..
        } => fault.clone(),
        other => panic!("not refused: {other:?}"),
    }
}

/// Claims every ready reply delivery and fails `targets` terminally (as a poison outcome, or a
/// retry budget an earlier build applied, would), returning their current records.
fn fail_replies(f: &Fixture, targets: &[&QueueEntry]) -> Vec<QueueEntry> {
    let claimed = claim_all(&f.s, REPLY_CONSUMER);
    targets
        .iter()
        .map(|t| {
            let c = claimed.iter().find(|c| c.id == t.id).expect("claimed");
            let (id, fence) = (c.id.clone(), c.fence().unwrap());
            write(&*f.s, move |tx| {
                tx.queue()
                    .fail(&id, &fence, "failed", FailureDisposition::Terminal)
            })
            .unwrap()
        })
        .collect()
}

#[test]
fn a_receipt_reply_is_never_cancelled_and_dismissed_only_when_it_cannot_be_honoured() {
    let f = Fixture::new();
    let mandatory_refusal = |reply: &CommandReply<DomainDecision>, what: &str| {
        let fault = fault(reply);
        assert_eq!(
            (
                fault.code,
                fault.retry,
                fault.target_kind.map(|k| k.0).as_deref()
            ),
            (
                DomainErrorCode::Conflict,
                devworks_core::protocol::RetryAdvice::Never,
                Some(REPLY_REQUIRED)
            ),
            "{what}"
        );
    };
    // A genuine reply delivery: the recorded command commits one for its receipt.
    let unknown = r#""deliveryId":"no-such-delivery","expectedRevision":"1","expectedFence":"0","resolution":"Retry","reason":"x""#;
    assert_eq!(
        refused(&f.command(&f.operator, "queue.resolve", "r-0", unknown)),
        DomainErrorCode::NotFound
    );
    let genuine = read(&*f.s, |s| {
        s.queue().deliveries(
            &DeliveryFilter {
                consumer: Some(REPLY_CONSUMER.into()),
                operation_id: None,
            },
            None,
            10,
        )
    })
    .entries
    .remove(0);
    assert_eq!(genuine.state, QueueState::Ready);
    // An operator can never cancel it.
    mandatory_refusal(
        &f.command(
            &f.operator,
            "queue.resolve",
            "r-1",
            &f.resolution(&genuine, "CancelBeforeDispatch"),
        ),
        "cancel a ready reply",
    );
    assert_eq!(entry(&*f.s, &genuine.id).state, QueueState::Ready);

    // Unhonourable deliveries: a payload that is no publication intent, and a well-formed intent
    // whose event is not its receipt's decision.
    let unreadable = enqueue(&*f.s, REPLY_CONSUMER, None, json!({"bad": true}));
    let misdirected = enqueue(
        &*f.s,
        REPLY_CONSUMER,
        None,
        json!({"actorId": "operator", "scopeKey": "scope", "requestKey": "r-0",
            "commandId": "command-1"}),
    );
    mandatory_refusal(
        &f.command(
            &f.operator,
            "queue.resolve",
            "r-2",
            &f.resolution(&unreadable, "CancelBeforeDispatch"),
        ),
        "cancel even an unreadable reply delivery",
    );
    // A well-formed intent naming a present `CommandDecided` decision under a receipt identity
    // that holds no receipt of that command.
    let mut payload = genuine.payload.clone();
    payload["requestKey"] = json!("no-such-request");
    payload["commandId"] = json!("command-without-receipt");
    let actor = ActorId(Id::new(genuine.payload["actorId"].as_str().unwrap()).unwrap());
    let misidentified = write(&*f.s, move |tx| {
        let event_id = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: event_id.clone(),
            actor,
            project_id: None,
            root_id: None,
            operation_id: None,
            aggregate_kind: "command".into(),
            aggregate_id: "command-without-receipt".into(),
            aggregate_revision: 1,
            kind: "CommandDecided".into(),
            payload: json!({}),
        })?;
        let id = tx.new_id(IdKind::QueueEntry);
        tx.queue().enqueue(NewQueueEntry {
            id,
            consumer: REPLY_CONSUMER.into(),
            event_id,
            root_id: None,
            operation_id: None,
            partition: None,
            priority: 0,
            ready_at_ms: 0,
            payload,
        })
    })
    .unwrap();
    let failed = fail_replies(&f, &[&genuine, &unreadable, &misdirected, &misidentified]);
    let [genuine, unreadable, misdirected, misidentified] =
        [&failed[0], &failed[1], &failed[2], &failed[3]];
    assert!(failed.iter().all(|e| e.state == QueueState::Failed));

    // A failed reply that can still be published is not acknowledged away: it is retried.
    mandatory_refusal(
        &f.command(
            &f.operator,
            "queue.resolve",
            "r-3",
            &f.resolution(genuine, "AcknowledgePoison"),
        ),
        "dismiss a publishable reply",
    );
    assert_eq!(entry(&*f.s, &genuine.id).state, QueueState::Failed);
    mandatory_refusal(
        &f.command(
            &f.operator,
            "queue.resolve",
            "r-4",
            &f.resolution(genuine, "CancelBeforeDispatch"),
        ),
        "cancel a failed reply",
    );
    let (retried, _) = applied(&f.command(
        &f.operator,
        "queue.retry",
        "r-5",
        &f.resolution(genuine, "Retry"),
    ));
    assert!(matches!(
        retried.state,
        OperationState::IntentRecorded | OperationState::RetryPending
    ));
    assert_eq!(entry(&*f.s, &genuine.id).state, QueueState::Ready);

    // Genuinely poisoned deliveries are acknowledged.
    for (key, e) in [
        ("r-6", unreadable),
        ("r-7", misdirected),
        ("r-8", misidentified),
    ] {
        let (dismissed, _) = applied(&f.command(
            &f.operator,
            "queue.resolve",
            key,
            &f.resolution(e, "AcknowledgePoison"),
        ));
        assert_eq!(dismissed.state, OperationState::Failed);
        assert_eq!(entry(&*f.s, &e.id).state, QueueState::Dismissed);
    }
}
