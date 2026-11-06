//! Real paid query recovery uses a stable authority witness across native invocations/boots.
use super::*;
fn caller(app: &HelpApp, invocation: &str) -> InvocationContext {
    let user = app.user();
    HostIngress::new(app.store.as_ref()).context(
        user.principal().clone(),
        user.principal_kind(),
        id(invocation),
        user.ingress_namespace().clone(),
        Counter::new(app.store.info().restore_epoch).unwrap(),
        user.policy_revision().clone(),
    )
}
fn query(
    rt: &tokio::runtime::Runtime,
    app: &HelpApp,
    ctx: InvocationContext,
    frame: &[u8],
) -> Value {
    let runtime = app.runtime.clone();
    let frame = frame.to_vec();
    rt.block_on(async move {
        tokio::task::spawn_blocking(move || {
            json!(runtime.query(&ctx, "conventions.search", &frame).unwrap())
        })
        .await
        .unwrap()
    })
}
fn attempts(app: &HelpApp) -> Vec<StoredInferenceAttempt> {
    read(app.store.as_ref(), |s| {
        s.usage().inference_attempts(
            &UsageAttemptFilter {
                purpose: Some("Embedding".into()),
                ..Default::default()
            },
            None,
            20,
        )
    })
}
#[test]
fn exact_paid_query_reuses_custody_with_new_invocations_and_real_app_restart() {
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let rt = runtime();
    let wire = Arc::new(Wire::default());
    let app = boot_paid(&rt, &h, wire.clone());
    let owner = imported_body(&app, &rt);
    configure(&app, &rt);
    rt.block_on(within("actual qualified indexed body", async {
        loop {
            app.runtime.dispatcher().run_until_idle().await.unwrap();
            if read(app.store.as_ref(), |s| {
                s.conventions().convention_index_head(&owner)
            })
            .is_some_and(|i| i.status == "Ready")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    }));
    let frame=serde_json::to_vec(&json!({"mode":"Query","protocol":PROTOCOL,"queryId":"paid-query-stable-invocation","schemaRevision":3,"body":{"kind":"conventions.search","scope":{"kind":"Workspace"},"page":{"limit":10},"applicability":[],"text":"actual source","topK":"10"}})).unwrap();
    let first = query(&rt, &app, caller(&app, "query-call-one"), &frame);
    let first_page = &first["result"]["value"];
    assert_eq!(first_page["retrieval"]["mode"], "Hybrid", "{first}");
    let request = id(first_page["retrieval"]["embeddingRequestId"]
        .as_str()
        .unwrap());
    let preparation: OperationId = serde_json::from_value(
        first_page["retrieval"]["embeddingOperation"]["operationId"].clone(),
    )
    .unwrap();
    let original_attempts = attempts(&app);
    let native = original_attempts
        .iter()
        .find(|a| a.request_id == request)
        .unwrap();
    let balance = read(app.store.as_ref(), |s| {
        s.usage().attempt_balance(&native.attempt_id)
    })
    .unwrap();
    assert_eq!(balance.priced_micro_usd, Some(7));
    assert!(balance.local_quiescent && !balance.unknown_tail);
    let calls = wire.calls.load(Ordering::SeqCst);
    let second = query(&rt, &app, caller(&app, "query-call-two"), &frame);
    assert_eq!(
        second["result"]["value"], *first_page,
        "new invocation must reuse exact paid query: {second}"
    );
    assert_eq!(wire.calls.load(Ordering::SeqCst), calls);
    assert_eq!(attempts(&app), original_attempts);
    rt.block_on(app.runtime.shutdown());
    drop(app);
    let reopened = boot_paid(&rt, &h, wire.clone());
    let recovered = query(
        &rt,
        &reopened,
        caller(&reopened, "query-call-after-restart"),
        &frame,
    );
    assert_eq!(
        recovered["result"]["value"], *first_page,
        "actual app restart must recover original paid query: {recovered}"
    );
    assert_eq!(wire.calls.load(Ordering::SeqCst), calls);
    assert_eq!(attempts(&reopened), original_attempts);
    assert_eq!(
        read(reopened.store.as_ref(), |s| s
            .usage()
            .attempt_balance(&native.attempt_id))
        .unwrap(),
        balance
    );
    let original_preparation = read(reopened.store.as_ref(), |s| {
        s.operations().operation(&preparation)
    })
    .unwrap();
    // A genuine selection change must retain the old financial binding and refuse reuse.
    reopened.n.store(1000, Ordering::SeqCst);
    let selection = read(reopened.store.as_ref(), |s| {
        s.router().embedding_selection()
    });
    value(reopened.command("router.configure",json!({"action":{"kind":"ClearEmbeddingSpace","expectedSelectionRevision":selection.revision.to_string()}}),3));
    let refused = query(
        &rt,
        &reopened,
        caller(&reopened, "query-call-new-selection"),
        &frame,
    );
    assert_eq!(refused["fault"]["code"], "CursorExpired", "{refused}");
    assert_eq!(wire.calls.load(Ordering::SeqCst), calls);
    assert_eq!(attempts(&reopened), original_attempts);
    assert_eq!(
        read(reopened.store.as_ref(), |s| s
            .operations()
            .operation(&preparation))
        .unwrap(),
        original_preparation
    );
    assert_eq!(
        read(reopened.store.as_ref(), |s| s
            .usage()
            .attempt_balance(&native.attempt_id))
        .unwrap(),
        balance
    );
    rt.block_on(reopened.runtime.shutdown());
}
