//! `broker.call`: a package calls the host operations its grant allows, as the principal that
//! invoked it, and nothing else.
use super::*;
use devworks_core::protocol::OperationState;
use devworks_core::registry::{Mode, SchemaRevision};
use devworks_engine::package_host::broker::APPLICATION_CAPABILITY;

/// The registered input schema reference of `operation` in `mode` at `revision`.
fn input_schema(operation: &str, mode: Mode, revision: Option<u32>) -> Value {
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

fn application(operation: &str, schema: Value, arguments: Value) -> Value {
    json!({"kind": "Application", "operation": format!("devworks/{operation}"),
        "inputSchema": schema, "arguments": arguments})
}

/// Runs the package's `call` contribution with `script` as `principal` on `input`.
fn run_calls(
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

fn read_artifact(a: &App, ctx: &InvocationContext, artifact: &wire::ArtifactRef) -> Value {
    let reference = serde_json::from_value(serde_json::to_value(artifact).unwrap()).unwrap();
    let (bytes, _) = a
        .app()
        .artifacts()
        .read_exact(&*a.store, ctx, &reference, 1 << 20)
        .unwrap_or_else(|e| panic!("{e:?}"));
    serde_json::from_slice(&bytes).unwrap()
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
