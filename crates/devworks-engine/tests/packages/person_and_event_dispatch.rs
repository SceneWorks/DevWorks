//! A person runs a package tool through `addons.invoke`, and a package's event handler runs when
//! the host event it declares fires, as the person who approved that exact version. Every path
//! here is the production composition: the gateway operation, the dispatcher's consumers, the
//! package host and its brokered inference.
use super::*;
use devworks_core::ports::storage::UsageAttemptFilter;
use devworks_core::protocol::OperationState;
use devworks_core::protocol::records::PackageInvocation;
use devworks_core::services::packages::index::AVAILABILITY_CHANGED;
use devworks_engine::package_host::events::{CONSUMER, MAX_CASCADE_DEPTH, ROUTER};
use devworks_engine::package_host::inference::{
    REQUEST_SCHEMA_ID, person_session_id, request_schema,
};

pub(super) fn invoke_frame(a: &App, package: &str, contribution: &str, input: &Value) -> Vec<u8> {
    json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": a.key("invoke"),
        "schemaRevision": 8, "body": {"kind": "addons.invoke", "packageId": package,
            "contributionId": format!("{package}/{contribution}"), "input": input}})
    .to_string()
    .into_bytes()
}

pub(super) fn invocation_of(reply: DomainCommandReply) -> PackageInvocation {
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
pub(super) fn finished_invocation(
    a: &App,
    ctx: &InvocationContext,
    operation: &Id,
) -> PackageInvocation {
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
    let input = a.save(&a.user, br#"{"text":"hello"}"#);

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

#[derive(Default)]
struct CoordinatorModel {
    requests: Mutex<Vec<Value>>,
    replacement_ready: (Mutex<bool>, std::sync::Condvar),
}

impl devworks_core::ports::provider_http::StreamingHttp for CoordinatorModel {
    fn open(
        &self,
        request: &devworks_core::ports::http::HttpRequest,
        _: &devworks_core::ports::provider_http::StreamLimits,
        _: &Cancellation,
    ) -> Result<
        Box<dyn devworks_core::ports::provider_http::HttpStream>,
        devworks_core::ports::provider_http::TransportError,
    > {
        let request: Value = serde_json::from_slice(request.body.as_ref().unwrap()).unwrap();
        let index = {
            let mut requests = self.requests.lock().unwrap();
            let index = requests.len();
            requests.push(request.clone());
            index
        };
        if index == 3 {
            let (replacement, signal) = &self.replacement_ready;
            let (_guard, wait) = signal
                .wait_timeout_while(replacement.lock().unwrap(), PATIENCE, |ready| !*ready)
                .unwrap();
            assert!(!wait.timed_out(), "package update did not reach cutover");
        }
        let body = if matches!(index, 0 | 3) {
            let alias = package_alias(&request)
                .expect("selected package tool in the native provider request");
            let call = format!("native-package-call-{index}");
            let finish = json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":call,
                "type":"function","function":{"name":alias,"arguments":"{\"text\":\"selected package\"}"}}]},"finish_reason":"tool_calls"}]});
            format!("data: {finish}\n\ndata: [DONE]\n\n").into_bytes()
        } else {
            MODEL_WIRE.as_bytes().to_vec()
        };
        Ok(Box::new(ModelStream {
            head: devworks_core::ports::provider_http::ResponseHead {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
            },
            body: Some(body),
        }))
    }
}

fn package_alias(request: &Value) -> Option<&str> {
    request["tools"]
        .as_array()?
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .find(|name| name.starts_with("pkg_echo_"))
}

fn set_package_policy(a: &App, selected: bool, profile: &'static str) {
    use devworks_core::settings::catalog::TOOL_OVERRIDES_KEY;
    write(&*a.store, move |tx| {
        let scope = ConfigScope::Workspace;
        let current = model::current_layer(tx.configuration(), &scope)?;
        let key = descriptor(TOOL_OVERRIDES_KEY).unwrap();
        let assignment = serde_json::from_value(json!({"key": key.key,
            "value": {"schema": key.schema_ref(), "value": {"agent-pack/echo": selected}}}))
        .unwrap();
        let profile_key = descriptor("safety.accessProfile").unwrap();
        let profile = serde_json::from_value(json!({"key": profile_key.key,
            "value": {"schema": profile_key.schema_ref(), "value": profile}}))
        .unwrap();
        let plan = model::plan(&current, &[assignment, profile]);
        let body = model::body(
            &plan,
            &ActorId(id("user-a")),
            tx.now_ms(),
            json!({"kind":"test"}),
        );
        tx.configuration().append_revision(
            &scope,
            model::NAMESPACE,
            current.revision,
            model::BODY_SCHEMA_VERSION,
            body,
        )?;
        Ok(())
    })
    .unwrap();
}

#[test]
fn the_native_coordinator_offers_and_dispatches_only_selected_package_tools() {
    let _s = serial();
    let mut a = App::unbooted(Harness::new(), None, None);
    seed_model(&a.store);
    a.model = Some(Arc::new(ScriptedModel::default()));
    let wire = Arc::new(CoordinatorModel::default());
    let mut composition = a.composition();
    composition.use_cases.register(
        "help.generate_capability_docs",
        None,
        Some(Arc::new(UnusedHelpGeneration)),
    );
    let owner = ActorId(id("workspace-coordinator"));
    composition.application.sessions = Some(devworks_engine::SessionPorts {
        coordinator: Some(owner.clone()),
    });
    composition.config.providers =
        Some(devworks_engine::providers::driver::ProviderComposition::new(wire.clone()));
    a.app = Some(a.rt.block_on(boot(composition)).unwrap());

    let mut spec = Spec::new("agent-pack", "1.0.0");
    spec.contributions = vec!["echo", "unselected"];
    package(a.install(&a.user, &spec));
    a.ready("agent-pack");
    set_package_policy(&a, true, "full");
    let session = match applied(a.domain(
        &a.user,
        "sessions.start",
        json!({
            "kind":"sessions.start","scope":{"kind":"Workspace"},"title":"Native tool selection"
        }),
    )) {
        OperationResult::SessionsStart { value } => value.id,
        other => panic!("expected an ordinary workspace session, got {other:?}"),
    };
    let send = |key: &str, text: &str| {
        let revision = read(&*a.store, |r| r.sessions().session(&session))
            .unwrap()
            .revision;
        let frame = json!({"protocol":"1.0-draft.1","schemaRevision":3,"requestKey":key,"sessionId":session,
            "expectedSessionRevision":revision.to_string(),"content":[{"kind":"Text","text":text}],
            "replyRoute":{"intent":{"kind":"Model","modelId":"model"},"effort":"Default"}});
        applied(
            a.app()
                .command(
                    &a.user,
                    "sessions.append_message",
                    frame.to_string().as_bytes(),
                )
                .unwrap(),
        );
    };
    send("package-coordinator-first", "Use the selected package tool");
    settled(&a, devworks_engine::package_tools::CONSUMER, 1);
    settled(
        &a,
        devworks_core::services::sessions::workspace_reply::CONSUMER,
        1,
    );
    let operation = read(&*a.store, |r| {
        r.operations().operations_for_capability(
            "package.invoke",
            &[OperationState::ObservedComplete],
            None,
            10,
        )
    })
    .pop()
    .expect("durable package tool operation");
    assert_eq!(operation.payload["contributionId"], "agent-pack/echo");
    assert_eq!(operation.payload["principal"], json!(owner));
    assert_eq!(operation.payload["sessionId"], json!(session));

    set_package_policy(&a, false, "full");
    send(
        "package-coordinator-disabled",
        "The package tool is now disabled",
    );
    settled(
        &a,
        devworks_core::services::sessions::workspace_reply::CONSUMER,
        2,
    );
    let transcript = read(&*a.store, |r| r.sessions().entries(&session, 0, 40));
    assert_eq!(
        transcript
            .iter()
            .filter(|entry| entry.kind == "ToolResponseRecorded")
            .count(),
        1
    );
    assert!(
        transcript.iter().any(|entry| {
            entry.kind == "OperationUpdate" && entry.body["operationId"] == json!(operation.id)
        }),
        "the settled invocation appends its own update after the initial tool receipt"
    );
    {
        let requests = wire.requests.lock().unwrap();
        let [first, continuation, disabled] = requests.as_slice() else {
            panic!(
                "expected one tool call and two final requests, got {}",
                requests.len()
            );
        };
        assert_eq!(package_alias(first), package_alias(continuation));
        assert!(!first.to_string().contains("pkg_unselected_"));
        assert!(
            package_alias(disabled).is_none(),
            "the disabled tool must disappear on the next request"
        );
    }
    assert_eq!(
        queue_status(&a, devworks_engine::package_tools::CONSUMER).completed,
        1
    );

    // A compatible update can keep the same input schema and change the executable. The
    // streaming model saw the old implementation's alias and cannot dispatch the new one.
    set_package_policy(&a, true, "full");
    send(
        "package-coordinator-old-version",
        "Use the currently offered tool",
    );
    let until = Instant::now() + PATIENCE;
    while wire.requests.lock().unwrap().len() < 4 {
        assert!(
            Instant::now() < until,
            "provider did not capture the old alias"
        );
        super::event_intents::drive(&a, Duration::from_millis(25));
    }
    spec.version = "1.1.0".into();
    let changed = package(a.update_from(&a.user, &a.user, &spec));
    assert_eq!(availability(&changed), "Ready");
    a.ready("agent-pack");
    *wire.replacement_ready.0.lock().unwrap() = true;
    wire.replacement_ready.1.notify_all();
    settled(
        &a,
        devworks_core::services::sessions::workspace_reply::CONSUMER,
        3,
    );
    let transcript = read(&*a.store, |r| r.sessions().entries(&session, 0, 40));
    assert!(
        transcript
            .iter()
            .any(|entry| entry.kind == "ToolResponseRecorded"
                && entry.body["outcome"]["code"] == "package_tool_unavailable")
    );
    {
        let requests = wire.requests.lock().unwrap();
        assert_eq!(package_alias(&requests[0]), package_alias(&requests[3]));
        assert!(package_alias(&requests[4]).is_some());
        assert_ne!(package_alias(&requests[3]), package_alias(&requests[4]));
    }

    // Project Scope never offers this unsandboxed native tool despite the explicit selection.
    set_package_policy(&a, true, "projectScope");
    send(
        "package-coordinator-project-scope",
        "Respect the scoped execution profile",
    );
    settled(
        &a,
        devworks_core::services::sessions::workspace_reply::CONSUMER,
        4,
    );
    assert!(package_alias(wire.requests.lock().unwrap().last().unwrap()).is_none());
    assert_eq!(
        queue_status(&a, devworks_engine::package_tools::CONSUMER).completed,
        1
    );
}

#[test]
fn an_agent_cannot_start_a_person_invocation_nor_read_one() {
    let _s = serial();
    let a = App::new();
    let mut spec = Spec::new("guarded-pack", "1.0.0");
    spec.contributions = vec!["echo"];
    package(a.install(&a.user, &spec));
    a.ready("guarded-pack");
    let input = a.save(&a.user, br#"{"text":"hello"}"#);
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
    let input = a.save(&a.user, br#"{"text":"hello"}"#);
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

// ---------------------------------------------------------------------------------------------
// A person's invocation is verified before it is recorded.

fn recorded_invocations(a: &App) -> usize {
    read(&*a.store, |s| {
        s.operations().operations_for_capability(
            "package.invoke",
            &[
                OperationState::IntentRecorded,
                OperationState::Dispatching,
                OperationState::ObservedComplete,
                OperationState::Failed,
                OperationState::OutcomeUnknown,
            ],
            None,
            50,
        )
    })
    .len()
}

#[test]
fn a_person_invocation_with_a_bad_or_unreadable_input_records_nothing() {
    let _s = serial();
    let a = App::new();
    let mut spec = Spec::new("verified-pack", "1.0.0");
    spec.contributions = vec!["echo"];
    package(a.install(&a.user, &spec));
    a.ready("verified-pack");
    let run = |ctx: &InvocationContext, input: &Value| {
        a.app()
            .command(
                ctx,
                "addons.invoke",
                &invoke_frame(&a, "verified-pack", "echo", input),
            )
            .unwrap()
    };
    // Input that is not a JSON object, and input the tool's schema rejects.
    let not_json = a.save(&a.user, b"hello");
    assert_eq!(
        refused(run(&a.user, &not_json)).0,
        DomainErrorCode::InvalidArgument
    );
    let wrong_type = a.save(&a.user, br#"{"text": 5}"#);
    assert_eq!(
        refused(run(&a.user, &wrong_type)).0,
        DomainErrorCode::InvalidArgument
    );
    let extra = a.save(&a.user, br#"{"text": "x", "other": 1}"#);
    assert_eq!(
        refused(run(&a.user, &extra)).0,
        DomainErrorCode::InvalidArgument
    );
    // An artifact of project-a is not readable from another project's context.
    let good = a.save(&a.user, br#"{"text": "hello"}"#);
    let elsewhere = a.user.clone().with_lineage(
        Some(devworks_core::protocol::ProjectId(id("project-z"))),
        None,
        None,
    );
    assert_eq!(
        refused(run(&elsewhere, &good)).0,
        DomainErrorCode::InvalidArgument
    );
    assert_eq!(recorded_invocations(&a), 0, "nothing was recorded");
    // The same input from the person's own context is recorded.
    invocation_of(run(&a.user, &good));
    assert_eq!(recorded_invocations(&a), 1);
}

// ---------------------------------------------------------------------------------------------
// Event cascades are bounded.

/// Commits an announcement the router fans out, naming what caused it.
fn announce(a: &App, package: &str, origin: Option<&str>, depth: u64) {
    let payload = match origin {
        Some(origin) => json!({"packageId": package, "originPackageId": origin, "depth": depth}),
        None => json!({"packageId": package}),
    };
    let package = package.to_owned();
    write(&*a.store, move |tx| {
        let event = devworks_core::protocol::EventId(tx.new_id(IdKind::Event));
        let actor = tx.store().host_actor.clone();
        tx.history().append(NewEvent {
            id: event.clone(),
            actor,
            project_id: None,
            root_id: None,
            operation_id: None,
            aggregate_kind: "package_availability".into(),
            aggregate_id: package,
            aggregate_revision: 99,
            kind: AVAILABILITY_CHANGED.into(),
            payload: payload.clone(),
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
            payload,
        })?;
        Ok(())
    })
    .unwrap();
}

/// Waits until both consumers have nothing left to do (installs announce several changes), and
/// returns how many deliveries each has settled.
fn quiet(a: &App) -> (i64, i64) {
    let until = Instant::now() + PATIENCE;
    let mut last = (-1, -1);
    let mut steady = 0;
    loop {
        let (router, handlers) = (queue_status(a, ROUTER), queue_status(a, CONSUMER));
        let now = (router.completed, handlers.completed);
        let idle = router.ready + router.claimed + handlers.ready + handlers.claimed == 0;
        steady = if idle && now == last { steady + 1 } else { 0 };
        if steady >= 20 {
            return now;
        }
        last = now;
        assert!(Instant::now() < until, "never quiet: {now:?}");
        super::event_intents::drive(a, Duration::from_millis(50));
    }
}

#[test]
fn a_handler_never_hears_of_the_event_its_own_package_caused() {
    let _s = serial();
    let a = App::with_model();
    let scripts = [react_script(&a, "self-pack"), react_script(&a, "peer-pack")];
    package(a.install(&a.user, &handler_pack("self-pack", "1.0.0")));
    package(a.install(&a.user, &handler_pack("peer-pack", "1.0.0")));
    let (routed, handled) = quiet(&a);
    let ran = attempts(&a).len();

    announce(&a, "self-pack", Some("self-pack"), 1);
    settled(&a, ROUTER, routed + 1);
    settled(&a, CONSUMER, handled + 2);
    scripts.iter().for_each(|p| {
        let _ = std::fs::remove_file(p);
    });
    // Only the peer's handler ran: the originating package's delivery was skipped and logged.
    assert_eq!(attempts(&a).len(), ran + 1);
    assert_eq!(a.logged("self-pack", "PackageEventSkipped"), 1);
    assert_eq!(a.logged("peer-pack", "PackageEventSkipped"), 0);
}

#[test]
fn two_packages_whose_handlers_cause_each_others_events_stop_at_the_depth_bound() {
    let _s = serial();
    let a = App::with_model();
    let scripts = [react_script(&a, "ping-pack"), react_script(&a, "pong-pack")];
    package(a.install(&a.user, &handler_pack("ping-pack", "1.0.0")));
    package(a.install(&a.user, &handler_pack("pong-pack", "1.0.0")));
    let (routed, handled) = quiet(&a);
    let ran = attempts(&a).len();

    // Each handler's intent announces a change one deeper than the delivery that committed it,
    // naming its own package: ping at depth 1, pong at 2, and so on.
    let steps = MAX_CASCADE_DEPTH + 2;
    for depth in 1..=steps {
        let origin = if depth % 2 == 1 {
            "ping-pack"
        } else {
            "pong-pack"
        };
        announce(&a, origin, Some(origin), depth);
        settled(&a, ROUTER, routed + depth as i64);
        settled(&a, CONSUMER, handled + 2 * depth as i64);
    }
    scripts.iter().for_each(|p| {
        let _ = std::fs::remove_file(p);
    });
    // Within the bound the other package's handler ran once per step; beyond it, none ran.
    assert_eq!(attempts(&a).len() as u64, ran as u64 + MAX_CASCADE_DEPTH);
    let skipped =
        a.logged("ping-pack", "PackageEventSkipped") + a.logged("pong-pack", "PackageEventSkipped");
    // One skip per step within the bound (the origin's own), and both past it.
    assert_eq!(
        skipped as u64,
        MAX_CASCADE_DEPTH + 2 * (steps - MAX_CASCADE_DEPTH)
    );
}

// ---------------------------------------------------------------------------------------------
// Reruns of an event delivery.

#[test]
fn a_rerun_of_a_delivery_reuses_its_input_artifact() {
    let _s = serial();
    let a = App::new();
    let host = a.app().packages().unwrap();
    let delivery = id("delivery-one");
    let document = json!({"eventId": "e-1", "event": {"packageId": "x"}});
    let published = |a: &App| {
        read(&*a.store, |s| {
            s.history().page(
                &EventFilter::Aggregate {
                    kind: "package_event_input".into(),
                    id: "delivery-one".into(),
                },
                0,
                50,
            )
        })
        .events
        .len()
    };
    let first = host.stage_event_input(&document, &delivery).unwrap();
    let again = host.stage_event_input(&document, &delivery).unwrap();
    let third = host.stage_event_input(&document, &delivery).unwrap();
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(&again).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(&third).unwrap()
    );
    assert_eq!(published(&a), 1, "one artifact for three attempts");
    // Another delivery gets its own.
    let other = host
        .stage_event_input(&document, &id("delivery-two"))
        .unwrap();
    assert_ne!(first.artifact_id, other.artifact_id);
}

/// Runs the `react` handler of `package` on a fresh delivery under `operation`.
fn run_react(a: &App, package: &str, n: usize, operation: &Id) -> Vec<String> {
    let delivery = super::event_intents::claimed(a, package, n);
    let document = a.save(
        &a.user,
        json!({"subscriptionId": format!("sub-{n}"), "eventId": delivery.event_id.0.as_str(),
            "expectedCheckpoint": "0"})
        .to_string()
        .as_bytes(),
    );
    let mut invocation = super::event_intents::invocation(a, &delivery, n);
    invocation.package_id = package.into();
    let handle = a
        .app()
        .packages()
        .unwrap()
        .invoke(InvokeRequest {
            package_id: package.into(),
            contribution_id: format!("{package}/react"),
            scope: InvocationScope::Workspace,
            input: serde_json::from_value(document).unwrap(),
            operation_id: operation.clone(),
            deadline: Duration::from_secs(30),
            allowed_operations: vec![],
            kind: InvocationKind::Event(Box::new(invocation)),
            principal: None,
            expected_digest: None,
        })
        .unwrap();
    finished(handle.wait(Duration::from_secs(20)));
    handle.progress().0
}

#[test]
fn a_rerun_of_a_handler_under_its_operation_is_not_charged_again() {
    let _s = serial();
    let a = App::with_model();
    let script = react_script(&a, "rerun-pack");
    package(a.install(&a.user, &handler_pack("rerun-pack", "1.0.0")));
    settled(&a, ROUTER, 1);
    settled(&a, CONSUMER, 1);
    let charged = attempts(&a).len();
    // The same operation identity (one per delivery) run twice, as a crashed delivery is rerun.
    let operation = id("pkgevt-rerun");
    for n in 1..=2 {
        let progress = run_react(&a, "rerun-pack", n, &operation);
        assert!(
            progress.iter().any(|p| p.starts_with("infer:")),
            "{progress:?}"
        );
    }
    let _ = std::fs::remove_file(&script);
    assert_eq!(
        attempts(&a).len(),
        charged + 1,
        "the rerun replayed the recorded answer instead of inferring again"
    );
}

#[test]
fn an_approved_codec_encodes_and_decodes_through_the_owned_router_attempt() {
    let _s = serial();
    let a = App::with_model();
    let mut codec = Spec::new("codec-pack", "1.0.0");
    codec.contributions = vec!["provider-codec"];
    codec.provider_codec = true;
    package(a.install(&a.user, &codec));
    a.ready("codec-pack");
    // The router picks subscribers when it handles an announcement. Drain the codec's own
    // installation announcement before installing the subscriber for this test.
    settled(&a, ROUTER, 1);
    let script = react_script(&a, "codec-consumer");
    package(a.install(&a.user, &handler_pack("codec-consumer", "1.0.0")));
    settled(&a, ROUTER, 2);
    settled(&a, CONSUMER, 1);
    let _ = std::fs::remove_file(script);
    let deliveries = read(&*a.store, |snapshot| {
        snapshot.history().page(
            &EventFilter::Aggregate {
                kind: "package_event".into(),
                id: "codec-consumer".into(),
            },
            0,
            2,
        )
    });
    assert_eq!(deliveries.events.len(), 1, "{deliveries:?}");
    let event = &deliveries.events[0];
    assert_eq!(event.kind, "PackageEventDelivered");
    assert_eq!(event.payload["event"]["packageId"], "codec-consumer");
    let recorded = attempts(&a);
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0].state, AttemptState::Settled);
    let operation = read(&*a.store, |snapshot| {
        snapshot.operations().operation(&recorded[0].operation_id)
    })
    .expect("the router records the invocation outcome");
    assert_eq!(
        operation.state,
        OperationState::ObservedComplete,
        "{operation:?}"
    );
    let requests = a.model.as_ref().unwrap().0.lock().unwrap();
    assert_eq!(requests.len(), 1, "{recorded:?}");
    let body: Value = serde_json::from_slice(requests[0].body.as_deref().unwrap()).unwrap();
    assert_eq!(body["x-devworks-fixture"], true);
    assert_eq!(body["model"], "fixture-model");
}
