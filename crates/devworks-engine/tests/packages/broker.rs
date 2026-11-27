//! `broker.call`: a package calls the host operations its grant allows, as the principal that
//! invoked it, and nothing else.
use super::*;
use devworks_core::protocol::OperationState;
use devworks_core::registry::{Mode, SchemaRevision};
use devworks_engine::package_host::broker::APPLICATION_CAPABILITY;

/// The registered input schema reference of `operation` in `mode` at `revision`.
pub(super) fn input_schema(operation: &str, mode: Mode, revision: Option<u32>) -> Value {
    let descriptor = devworks_core::registry::find(operation).unwrap();
    let revision = revision.map_or(*descriptor.revisions.last().unwrap(), SchemaRevision);
    let schema = descriptor
        .binding(mode)
        .unwrap()
        .input_schema(revision)
        .to_value();
    let canonical = devworks_core::jcs::canonicalize(&schema).unwrap();
    let mode = match mode {
        Mode::Query => "query",
        Mode::Command => "command",
    };
    json!({"schemaId": format!("devworks/{operation}.{mode}.input"),
        "revision": revision.0.to_string(), "sha256": sha256_hex(canonical.as_bytes())})
}

pub(super) fn application(operation: &str, schema: Value, arguments: Value) -> Value {
    json!({"kind": "Application", "operation": format!("devworks/{operation}"),
        "inputSchema": schema, "arguments": arguments})
}

/// Runs the package's `call` contribution with `script` as `principal` on `input`.
pub(super) fn run_calls(
    a: &App,
    package: &str,
    script: Value,
    input: &Value,
    principal: Option<InvocationContext>,
    allowed: Vec<&str>,
) -> (Vec<String>, InvocationOutcome) {
    let path = std::env::temp_dir().join(format!("devworks-fixture-{package}-call.json"));
    std::fs::write(&path, script.to_string()).unwrap();
    let h = a
        .app()
        .packages()
        .unwrap()
        .invoke(InvokeRequest {
            package_id: package.into(),
            contribution_id: format!("{package}/call"),
            scope: InvocationScope::Workspace,
            input: serde_json::from_value(input.clone()).unwrap(),
            operation_id: id(&a.key("op")),
            deadline: Duration::from_secs(30),
            allowed_operations: allowed
                .into_iter()
                .map(|o| o.to_owned().try_into().unwrap())
                .collect(),
            kind: InvocationKind::Ordinary,
            principal,
            expected_digest: None,
        })
        .unwrap();
    let outcome = finished(h.wait(Duration::from_secs(20)));
    let _ = std::fs::remove_file(&path);
    (h.progress().0, outcome)
}

pub(super) fn read_artifact(
    a: &App,
    ctx: &InvocationContext,
    artifact: &wire::ArtifactRef,
) -> Value {
    let reference = serde_json::from_value(serde_json::to_value(artifact).unwrap()).unwrap();
    let (bytes, _) = a
        .app()
        .artifacts()
        .read_exact(&*a.store, ctx, &reference, 1 << 20)
        .unwrap_or_else(|e| panic!("{e:?}"));
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn a_human_invoked_package_cannot_make_a_protected_decision() {
    let _s = serial();
    let a = App::new();
    let mut spec = Spec::new("decision-pack", "1.0.0");
    spec.contributions = vec!["call"];
    spec.effect = "Mutation";
    package(a.install(&a.user, &spec));
    a.ready("decision-pack");
    let args = a.save(&a.user, b"{}");
    let script = json!({"calls": [
        {"actionKey": "decision", "call": application("approvals.record_human_decision",
            input_schema("approvals.record_human_decision", Mode::Command, None), json!("$input"))},
        {"actionKey": "continue", "call": application("runs.continue",
            input_schema("runs.continue", Mode::Command, None), json!("$input"))}
    ]});
    let (progress, _) = run_calls(
        &a,
        "decision-pack",
        script,
        &args,
        Some(a.user.clone()),
        vec![],
    );
    assert_eq!(
        progress,
        [
            "call:decision:PermissionDenied",
            "call:continue:PermissionDenied"
        ]
    );
}

#[test]
fn a_package_calls_only_what_its_grant_allows_and_as_its_invoking_principal() {
    let _s = serial();
    let a = App::new();
    let mut spec = Spec::new("call-pack", "1.0.0");
    spec.contributions = vec!["call"];
    package(a.install(&a.user, &spec));
    a.ready("call-pack");
    let args = a.save(&a.user, br#"{"page":{"limit":100}}"#);
    let other = a.save(&a.user, br#"{"page":{"limit":5}}"#);
    let catalog = input_schema("addons.catalog", Mode::Query, Some(2));
    let disable = input_schema("addons.disable", Mode::Command, Some(2));
    let script = json!({"calls": [
        {"actionKey": "read", "call": application("addons.catalog", catalog.clone(), json!("$input"))},
        // A read-only tool calls no command, whatever the principal could do.
        {"actionKey": "disable", "call": application("addons.disable", disable, json!("$input"))},
        // Only the invocation's own artifacts are arguments.
        {"actionKey": "foreign", "call": application("addons.catalog", catalog.clone(), other.clone())},
        // A schema other than the operation's registered one is a protocol violation.
        {"actionKey": "schema", "call": application("addons.catalog",
            json!({"schemaId": "devworks/addons.catalog.query.input", "revision": "2", "sha256": "0".repeat(64)}),
            json!("$input"))},
        // Network and inference need their own approved access.
        {"actionKey": "http", "call": {"kind": "Http", "connectionHandle": "c1", "endpointId": "e1",
            "httpMethod": "Get", "relativePath": "/status"}},
        {"actionKey": "infer", "call": {"kind": "Inference", "inputSchema": catalog, "request": "$input"}},
    ]});
    let (progress, outcome) = run_calls(
        &a,
        "call-pack",
        script.clone(),
        &args,
        Some(a.agent.clone()),
        vec![],
    );
    assert_eq!(
        progress,
        vec![
            "call:read:BrokerCompleted",
            "call:disable:PermissionDenied",
            "call:foreign:PermissionDenied",
            "call:schema:ProtocolViolation",
            "call:http:PermissionDenied",
            "call:infer:PermissionDenied",
        ]
    );
    // The package got the operation's typed reply, published for the invoking agent.
    let InvocationOutcome::Completed { artifacts } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(artifacts.len(), 1);
    let reply = read_artifact(&a, &a.agent, &artifacts[0]);
    assert_eq!(reply["kind"], "Result", "{reply}");
    assert!(reply.to_string().contains("call-pack"), "{reply}");
    // The call is journaled as an operation and its history names the package as the actor.
    let calls = read(&*a.store, |s| {
        s.operations().operations_for_capability(
            APPLICATION_CAPABILITY,
            &[OperationState::ObservedComplete],
            None,
            10,
        )
    });
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].payload["operation"], "addons.catalog");
    assert_eq!(calls[0].payload["principal"], "agent-a");
    let started = read(&*a.store, |s| {
        s.history().page(
            &EventFilter::Aggregate {
                kind: "package".into(),
                id: "call-pack".into(),
            },
            0,
            500,
        )
    })
    .events
    .into_iter()
    .find(|e| e.kind == "PackageBrokerCallStarted")
    .expect("a started call event");
    assert_eq!(started.actor.0.as_str(), "package-call-pack");
    assert_eq!(
        read(&*a.store, |s| s.actors().actor_kind(&started.actor)),
        Some(PrincipalKind::Package)
    );

    // Without an invoking principal nothing runs; a caller's list only narrows.
    let read_only = json!({"calls": [script["calls"][0].clone()]});
    let (progress, _) = run_calls(&a, "call-pack", read_only.clone(), &args, None, vec![]);
    assert_eq!(progress, vec!["call:read:PermissionDenied"]);
    let (progress, _) = run_calls(
        &a,
        "call-pack",
        read_only,
        &args,
        Some(a.agent.clone()),
        vec!["devworks/addons.logs"],
    );
    assert_eq!(progress, vec!["call:read:PermissionDenied"]);
}

#[test]
fn a_mutating_tool_runs_commands_as_its_invoking_principal() {
    let _s = serial();
    let a = App::new();
    let mut spec = Spec::new("mut-pack", "1.0.0");
    spec.contributions = vec!["call"];
    spec.effect = "Mutation";
    package(a.install(&a.user, &spec));
    package(a.install(&a.user, &Spec::new("other-pack", "1.0.0")));
    a.ready("mut-pack");
    let args = a.save(
        &a.user,
        json!({"command": {"kind": "Disable", "packageId": "other-pack",
            "expectedRegistryRevision": a.registry().to_string()}})
        .to_string()
        .as_bytes(),
    );
    let script = json!({"calls": [{"actionKey": "disable", "call": application(
        "addons.disable",
        input_schema("addons.disable", Mode::Command, Some(2)),
        json!("$input"),
    )}]});
    let (progress, outcome) =
        run_calls(&a, "mut-pack", script, &args, Some(a.agent.clone()), vec![]);
    assert_eq!(progress, vec!["call:disable:BrokerCompleted"]);
    let InvocationOutcome::Completed { artifacts } = outcome else {
        panic!("{outcome:?}");
    };
    let reply = read_artifact(&a, &a.agent, &artifacts[0]);
    assert_eq!(reply["receipt"]["decision"]["kind"], "Applied", "{reply}");
    assert!(!a.record("other-pack").enabled);
    // The gateway admitted and recorded it as the agent that invoked the package: the agent's
    // own authority decided it, never the package's.
    let actors: Vec<String> = read(&*a.store, |s| {
        s.history().page(
            &EventFilter::Aggregate {
                kind: "package".into(),
                id: "other-pack".into(),
            },
            0,
            500,
        )
    })
    .events
    .into_iter()
    .map(|e| e.actor.0.as_str().to_owned())
    .collect();
    assert!(actors.iter().any(|a| a == "agent-a"), "{actors:?}");
    assert!(
        !actors.iter().any(|a| a.starts_with("package-")),
        "{actors:?}"
    );
}

/// A booted app whose use cases include the agent inbox (`agents.receive`, `agents.post`), which
/// the engine registers with every composition.
fn with_inbox() -> App {
    App::new()
}

/// The journaled brokered calls in `states`.
fn broker_calls(a: &App, states: &[OperationState]) -> Vec<OperationRecord> {
    read(&*a.store, |s| {
        s.operations()
            .operations_for_capability(APPLICATION_CAPABILITY, states, None, 10)
    })
}

/// A mutating tool brokers a person's `agents.receive`, which waits on the empty inbox; `end`
/// ends the invocation (a cancellation or its deadline). The invocation must end well inside
/// the receive's 55 s wait, and a message posted after it ended is never delivered to it.
fn a_waiting_receive_ends_with_its_invocation(
    deadline: Duration,
    end: impl FnOnce(&devworks_engine::package_host::InvocationHandle) + Send,
) {
    /// Far below the receive's 55 s wait, far above a loaded machine's cancellation grace.
    const PROMPTLY: Duration = Duration::from_secs(10);
    let _s = serial();
    let a = with_inbox();
    // Per process: concurrent copies of this binary never share the fixture's script file.
    let name = format!("recv-pack-{}", std::process::id());
    let mut spec = Spec::new(&name, "1.0.0");
    spec.contributions = vec!["call"];
    spec.effect = "Mutation";
    package(a.install(&a.user, &spec));
    a.ready(&name);
    let args = a.save(&a.user, br#"{"projectId":"project-a","waitSeconds":55}"#);
    let script = json!({"calls": [{"actionKey": "receive", "call": application(
        "agents.receive",
        input_schema("agents.receive", Mode::Command, None),
        json!("$input"),
    )}]});
    let path = std::env::temp_dir().join(format!("devworks-fixture-{name}-call.json"));
    std::fs::write(&path, script.to_string()).unwrap();
    let h = a
        .app()
        .packages()
        .unwrap()
        .invoke(InvokeRequest {
            package_id: name.clone(),
            contribution_id: format!("{name}/call"),
            scope: InvocationScope::Workspace,
            input: serde_json::from_value(args).unwrap(),
            operation_id: id(&a.key("op")),
            deadline,
            allowed_operations: vec![],
            kind: InvocationKind::Ordinary,
            principal: Some(a.user.clone()),
            expected_digest: None,
        })
        .unwrap();
    // The call is journaled just before the gateway admits it; then its preparation waits.
    let until = Instant::now() + PATIENCE;
    while broker_calls(&a, &[OperationState::Dispatching]).is_empty() {
        assert!(Instant::now() < until, "the receive was never brokered");
        std::thread::sleep(Duration::from_millis(20));
    }
    // Lets the gateway enter the receive's wait (what is asserted below holds either way).
    std::thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    let ended = std::thread::scope(|scope| {
        let ending = scope.spawn(|| end(&h));
        while !ending.is_finished() && started.elapsed() < PROMPTLY {
            std::thread::sleep(Duration::from_millis(10));
        }
        let ended = ending.is_finished().then(|| started.elapsed());
        // A message that arrives once the invocation is over (or should have been).
        let post = json!({"protocol": "1.0-draft.1", "mode": "Command", "schemaRevision": 7,
            "requestKey": a.key("post"), "body": {"kind": "agents.post",
            "projectId": "project-a", "body": "After the invocation", "toHuman": true}});
        assert!(matches!(
            a.app()
                .command(&a.user, "agents.post", post.to_string().as_bytes())
                .unwrap(),
            CommandReply::Recorded { .. }
        ));
        ended
    });
    let _ = std::fs::remove_file(&path);
    // The brokered receive settles without delivering the message.
    let until = Instant::now() + PATIENCE;
    while !broker_calls(&a, &[OperationState::Dispatching]).is_empty() {
        assert!(Instant::now() < until, "the brokered receive never settled");
        std::thread::sleep(Duration::from_millis(20));
    }
    let settled = broker_calls(
        &a,
        &[OperationState::Failed, OperationState::ObservedComplete],
    );
    assert!(
        ended.is_some_and(|e| e < PROMPTLY)
            && settled.len() == 1
            && settled[0].state == OperationState::Failed,
        "ended after {ended:?} (bound {PROMPTLY:?}); brokered receive: {settled:?}"
    );
    // The message still waits for the person.
    let receive = json!({"protocol": "1.0-draft.1", "mode": "Command", "schemaRevision": 7,
        "requestKey": a.key("receive"), "body": {"kind": "agents.receive",
        "projectId": "project-a"}});
    let reply = a
        .app()
        .command(&a.user, "agents.receive", receive.to_string().as_bytes())
        .unwrap();
    assert!(
        matches!(&reply, CommandReply::Recorded { receipt, .. }
            if matches!(&receipt.decision, DomainDecision::Applied {
                result: OperationResult::AgentsReceive { value }
            } if value.items.0.len() == 1 && value.items.0[0].body.0 == "After the invocation")),
        "{reply:?}"
    );
}

#[test]
fn cancelling_an_invocation_stops_its_waiting_brokered_receive() {
    a_waiting_receive_ends_with_its_invocation(Duration::from_secs(30), |h| {
        h.cancel();
    });
}

#[test]
fn an_expired_invocation_stops_its_waiting_brokered_receive() {
    a_waiting_receive_ends_with_its_invocation(Duration::from_secs(2), |h| {
        // The host enforces the deadline; the invocation is over once it is terminal.
        let _ = h.wait(Duration::from_secs(60));
    });
}
