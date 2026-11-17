//! A person runs a package tool through `addons.invoke`, and a package's event handler runs when
//! the host event it declares fires, as the person who approved that exact version. Every path
//! here is the production composition: the gateway operation, the dispatcher's consumers, the
//! package host and its brokered inference.
use super::*;
use devworks_core::ports::storage::UsageAttemptFilter;
use devworks_core::protocol::OperationState;
use devworks_core::protocol::records::PackageInvocation;
use devworks_core::services::packages::index::AVAILABILITY_CHANGED;
use devworks_engine::package_host::events::{CONSUMER, ROUTER};
use devworks_engine::package_host::inference::{
    REQUEST_SCHEMA_ID, person_session_id, request_schema,
};

fn invoke_frame(a: &App, package: &str, contribution: &str, input: &Value) -> Vec<u8> {
    json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": a.key("invoke"),
        "schemaRevision": 8, "body": {"kind": "addons.invoke", "packageId": package,
            "contributionId": format!("{package}/{contribution}"), "input": input}})
    .to_string()
    .into_bytes()
}

fn invocation_of(reply: DomainCommandReply) -> PackageInvocation {
    match applied(reply) {
        OperationResult::AddonsInvoke { value } => value,
        other => panic!("{other:?}"),
    }
}

fn status(
    a: &App,
    ctx: &InvocationContext,
    operation: &Id,
) -> Result<PackageInvocation, DomainErrorCode> {
    let frame = json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": "q-invocation",
        "schemaRevision": 8, "body": {"kind": "addons.invocation", "operationId": operation.as_str()}});
    match a
        .app()
        .query(ctx, "addons.invocation", frame.to_string().as_bytes())
        .unwrap()
    {
        DomainQueryReply::Result {
            result: OperationResult::AddonsInvocation { value },
            ..
        } => Ok(value),
        DomainQueryReply::Rejected { fault, .. } => Err(fault.code),
        other => panic!("{other:?}"),
    }
}

/// Waits until the person's invocation `operation` is no longer queued or running.
fn finished_invocation(a: &App, ctx: &InvocationContext, operation: &Id) -> PackageInvocation {
    let until = Instant::now() + PATIENCE;
    loop {
        let now = status(a, ctx, operation).expect("the person's own invocation");
        if matches!(
            now.state,
            OperationState::ObservedComplete
                | OperationState::Failed
                | OperationState::OutcomeUnknown
        ) {
            return now;
        }
        assert!(
            Instant::now() < until,
            "the invocation never settled: {now:?} {:?} {:?}",
            read(&*a.store, |s| s.queue().status("package-tools")),
            read(&*a.store, |s| s.queue().deliveries(
                &DeliveryFilter::default(),
                None,
                10
            ))
        );
        super::event_intents::drive(a, Duration::from_millis(25));
    }
}

#[test]
fn a_person_runs_a_package_tool_through_the_gateway_and_reads_its_result() {
    let _s = serial();
    let a = App::new();
    let mut spec = Spec::new("run-pack", "1.0.0");
    spec.contributions = vec!["echo"];
    package(a.install(&a.user, &spec));
    a.ready("run-pack");
    let input = a.save(&a.user, b"hello");

    let started = invocation_of(
        a.app()
            .command(
                &a.user,
                "addons.invoke",
                &invoke_frame(&a, "run-pack", "echo", &input),
            )
            .unwrap(),
    );
    assert!(
        started.artifacts.0.is_empty(),
        "nothing has run when it is recorded"
    );
    let done = finished_invocation(&a, &a.user, &started.operation_id.0);
    assert_eq!(done.state, OperationState::ObservedComplete, "{done:?}");
    // The echo tool returns its input, which is the one artifact of the invocation.
    assert_eq!(done.artifacts.0.len(), 1);
    assert_eq!(
        done.artifacts.0[0].artifact_id.as_str(),
        input["artifactId"].as_str().unwrap()
    );
    assert!(done.failure.is_none());
}

#[test]
fn an_agent_cannot_start_a_person_invocation_nor_read_one() {
    let _s = serial();
    let a = App::new();
    let mut spec = Spec::new("guarded-pack", "1.0.0");
    spec.contributions = vec!["echo"];
    package(a.install(&a.user, &spec));
    a.ready("guarded-pack");
    let input = a.save(&a.user, b"hello");
    let agent = a.app().command(
        &a.agent,
        "addons.invoke",
        &invoke_frame(&a, "guarded-pack", "echo", &input),
    );
    assert_eq!(refused(agent.unwrap()).0, DomainErrorCode::PermissionDenied);

    // Another person cannot read the first person's invocation.
    let started = invocation_of(
        a.app()
            .command(
                &a.user,
                "addons.invoke",
                &invoke_frame(&a, "guarded-pack", "echo", &input),
            )
            .unwrap(),
    );
    let other = principal(&*a.store, "user-b", PrincipalKind::Human);
    assert_eq!(
        status(&a, &other, &started.operation_id.0).unwrap_err(),
        DomainErrorCode::NotFound
    );
    assert_eq!(
        status(&a, &a.agent, &started.operation_id.0).unwrap_err(),
        DomainErrorCode::NotFound
    );
    finished_invocation(&a, &a.user, &started.operation_id.0);
}

#[test]
fn a_person_invocation_names_an_installed_tool_of_the_active_version() {
    let _s = serial();
    let a = App::new();
    let mut spec = Spec::new("named-pack", "1.0.0");
    spec.contributions = vec!["echo", "react"];
    spec.handlers = vec!["react"];
    package(a.install(&a.user, &spec));
    a.ready("named-pack");
    let input = a.save(&a.user, b"hello");
    let refusal = |package: &str, contribution: &str| {
        refused(
            a.app()
                .command(
                    &a.user,
                    "addons.invoke",
                    &invoke_frame(&a, package, contribution, &input),
                )
                .unwrap(),
        )
        .0
    };
    assert_eq!(refusal("named-pack", "missing"), DomainErrorCode::NotFound);
    // An event handler is not a tool a person runs.
    assert_eq!(refusal("named-pack", "react"), DomainErrorCode::NotFound);
    assert_eq!(refusal("absent-pack", "echo"), DomainErrorCode::NotFound);
    // The caller's resolved version must be the active one.
    let mut frame: Value =
        serde_json::from_slice(&invoke_frame(&a, "named-pack", "echo", &input)).unwrap();
    frame["body"]["expectedDigest"] = json!("f".repeat(64));
    assert_eq!(
        refused(
            a.app()
                .command(&a.user, "addons.invoke", frame.to_string().as_bytes())
                .unwrap()
        )
        .0,
        DomainErrorCode::Conflict
    );
}

// ---------------------------------------------------------------------------------------------

/// The request the handler sends: the host's inference request schema.
fn react_script(a: &App, package: &str) -> std::path::PathBuf {
    let digest = devworks_core::fingerprint::canonical_digest(&request_schema()).unwrap();
    let script = json!({
        "request": {"messages": [{"role": "user", "text": "Summarize the event"}],
            "systemPrompt": "Answer in one line.", "maxTokens": 64},
        "inputSchema": {"schemaId": REQUEST_SCHEMA_ID, "revision": "1", "sha256": digest},
    });
    let path = std::env::temp_dir().join(format!("devworks-fixture-{package}-react.json"));
    std::fs::write(&path, script.to_string()).unwrap();
    let _ = a;
    path
}

fn handler_pack(name: &str, version: &str) -> Spec {
    let mut spec = Spec::new(name, version);
    spec.contributions = vec!["react"];
    spec.handlers = vec!["react"];
    spec.permissions = vec![json!({"kind": "StateWrite"}), json!({"kind": "Inference"})];
    spec
}

fn queue_status(a: &App, consumer: &'static str) -> QueueStatus {
    read(&*a.store, move |s| s.queue().status(consumer))
}

/// Waits until `consumer` settled `completed` deliveries.
fn settled(a: &App, consumer: &'static str, completed: i64) {
    let until = Instant::now() + PATIENCE;
    loop {
        let now = queue_status(a, consumer);
        assert_eq!(now.failed + now.reconciling, 0, "{consumer}: {now:?}");
        if now.completed >= completed {
            return;
        }
        assert!(Instant::now() < until, "{consumer} never settled: {now:?}");
        super::event_intents::drive(a, Duration::from_millis(25));
    }
}

fn attempts(a: &App) -> Vec<StoredInferenceAttempt> {
    read(&*a.store, |s| {
        s.usage()
            .inference_attempts(&UsageAttemptFilter::default(), None, 64)
    })
}

#[test]
fn an_installed_person_approved_handler_runs_when_its_event_fires_and_infers_as_the_approver() {
    let _s = serial();
    let a = App::with_model();
    let script = react_script(&a, "react-pack");
    // The person installs a package whose handler declares the availability announcement; the
    // install itself announces the package, which fires the event.
    let installed = package(a.install(&a.user, &handler_pack("react-pack", "1.0.0")));
    assert_eq!(availability(&installed), "Ready");
    settled(&a, ROUTER, 1);
    settled(&a, CONSUMER, 1);
    let _ = std::fs::remove_file(&script);

    // The handler ran as the approving person: its inference is hers, in her package session.
    let recorded = attempts(&a);
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0].actor_id, a.user.principal().clone());
    assert_eq!(
        recorded[0].session_id,
        Some(person_session_id(a.user.principal(), None, None))
    );
    assert_eq!(
        a.app()
            .packages()
            .unwrap()
            .instance_status("react-pack")
            .len(),
        1
    );
    // Its event was committed once: one processing receipt for the handler.
    let handled = read(&*a.store, |s| {
        s.queue().deliveries(
            &DeliveryFilter {
                consumer: Some(CONSUMER.into()),
                operation_id: None,
            },
            None,
            10,
        )
    });
    assert!(
        handled.entries.is_empty(),
        "nothing is left unsettled: {handled:?}"
    );
    assert_eq!(a.app().packages().unwrap().failures("react-pack"), 0);
}

#[test]
fn a_version_no_person_approved_runs_no_handler() {
    let _s = serial();
    let a = App::with_model();
    let script = react_script(&a, "trusted-react");
    // Version 1.0.0 is approved by the person. A trusted publisher's compatible update then
    // activates 1.1.0 without any person approving that version.
    package(a.install(&a.user, &handler_pack("trusted-react", "1.0.0")));
    settled(&a, CONSUMER, 1);
    assert_eq!(
        attempts(&a).len(),
        1,
        "the approved version's handler infers"
    );
    a.ready("trusted-react");
    let policy = json!({"kind": "addons.configure", "packageId": "trusted-react",
        "expectedRevision": a.record("trusted-react").revision.get().to_string(),
        "config": {"schema": host_schema_ref(UPDATE_POLICY_SCHEMA).unwrap(),
            "value": {"trustPublisherUpdates": true}}});
    package(a.domain(&a.user, "addons.configure", policy));
    let rec = package(a.update_from(&a.user, &a.agent, &handler_pack("trusted-react", "1.1.0")));
    assert_eq!(rec.trust_state.0, "TrustedPublisher", "{rec:?}");
    let until = Instant::now() + PATIENCE;
    while a.logged("trusted-react", "PackageEventSkipped") == 0 {
        assert!(Instant::now() < until, "{:?}", a.logs("trusted-react"));
        super::event_intents::drive(&a, Duration::from_millis(25));
    }
    settled(&a, CONSUMER, 2);
    let _ = std::fs::remove_file(&script);
    // The unapproved version's delivery settled without running its handler: no second
    // inference, and the package's log says why.
    assert_eq!(
        attempts(&a).len(),
        1,
        "no inference ran for the new version"
    );
    assert_eq!(a.logged("trusted-react", "PackageEventSkipped"), 1);
}

#[test]
fn an_announcement_committed_while_the_host_was_down_is_delivered_once_after_restart() {
    let _s = serial();
    let mut a = App::with_model();
    let script = react_script(&a, "restart-pack");
    package(a.install(&a.user, &handler_pack("restart-pack", "1.0.0")));
    settled(&a, CONSUMER, 1);
    assert_eq!(attempts(&a).len(), 1);

    // The host stops. An announcement is committed while it is down.
    a.shutdown();
    write(&*a.store, |tx| {
        let event = devworks_core::protocol::EventId(tx.new_id(IdKind::Event));
        let actor = tx.store().host_actor.clone();
        tx.history().append(NewEvent {
            id: event.clone(),
            actor,
            project_id: None,
            root_id: None,
            operation_id: None,
            aggregate_kind: "package_availability".into(),
            aggregate_id: "restart-pack".into(),
            aggregate_revision: 99,
            kind: AVAILABILITY_CHANGED.into(),
            payload: json!({"packageId": "restart-pack", "reason": "Offline"}),
        })?;
        let entry = tx.new_id(IdKind::QueueEntry);
        tx.queue().enqueue(NewQueueEntry {
            id: entry,
            consumer: ROUTER.into(),
            event_id: event,
            root_id: None,
            operation_id: None,
            partition: None,
            priority: 0,
            ready_at_ms: 0,
            payload: json!({"packageId": "restart-pack"}),
        })?;
        Ok(())
    })
    .unwrap();
    a.boot();
    settled(&a, ROUTER, 2);
    settled(&a, CONSUMER, 2);
    let _ = std::fs::remove_file(&script);
    // Each announcement ran the handler once.
    assert_eq!(attempts(&a).len(), 2);
    assert_eq!(queue_status(&a, CONSUMER).completed, 2);
}
