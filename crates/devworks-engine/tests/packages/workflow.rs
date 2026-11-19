use super::*;

fn operation_for(
    a: &App,
    principal: &InvocationContext,
    input: &Value,
    digest: &str,
    gates: Value,
) -> DomainCommandReply {
    let raw = json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":9,
        "requestKey": a.key("workflow"), "body": {"kind":"addons.run_workflow",
            "workflowId":"compatibility","packageId":"workflow-pack","contributionId":"workflow-pack/workflow-step",
            "manifestDigest":digest,"input":input,"requiredGates":gates}});
    a.app()
        .command(principal, "addons.run_workflow", raw.to_string().as_bytes())
        .unwrap()
}

fn await_result(a: &App, reply: DomainCommandReply) -> OperationRecord {
    let CommandReply::Recorded { receipt, .. } = reply else {
        panic!("{reply:?}")
    };
    let DomainDecision::Accepted { operation_id } = receipt.decision else {
        panic!("{receipt:?}")
    };
    let until = Instant::now() + PATIENCE;
    loop {
        let id = operation_id.clone();
        let record = read(&*a.store, move |s| s.operations().operation(&id)).unwrap();
        if matches!(
            record.state,
            OperationState::ObservedComplete
                | OperationState::Failed
                | OperationState::OutcomeUnknown
        ) {
            return record;
        }
        assert!(Instant::now() < until, "{record:?}");
        super::event_intents::drive(a, Duration::from_millis(20));
    }
}

#[test]
fn a_workflow_waits_for_the_exact_selected_gates_and_retains_source_bound_evidence() {
    let _s = serial();
    let a = App::new();
    let mut spec = Spec::new("workflow-pack", "1.0.0");
    spec.workflow = true;
    spec.contributions = vec!["workflow-step", "workflow-gate"];
    let installed = package(a.install(&a.user, &spec));
    let digest = digest_of(&installed);
    a.ready("workflow-pack");
    let gates = json!([{"packageId":"workflow-pack","contributionId":"workflow-pack/workflow-gate",
        "manifestDigest":digest,"reason":"The selected workflow requires this check."}]);
    let ctx = a.agent.clone().with_session(id("workflow-agent-session"));
    let session = StoredSession {
        id: id("workflow-agent-session"),
        conversation_id: id("workflow-agent-conversation"),
        agent_id: ctx.principal().clone(),
        project_id: None,
        root_id: None,
        title: "Workflow selection".into(),
        context_epoch: 0,
        revision: 1,
        created_by: a.user.principal().clone(),
        created_at_ms: 0,
        closed_at_ms: None,
        closed_by: None,
        next_sequence: 1,
        checkpoint_sequence: 0,
    };
    write(&*a.store, move |tx| tx.sessions().insert_session(&session)).unwrap();
    let input = a.save(&ctx, br#"{"text":"valid"}"#);
    let pass = await_result(&a, operation_for(&a, &ctx, &input, &digest, gates.clone()));
    assert_eq!(pass.payload["principalKind"], "Agent");
    assert_eq!(pass.state, OperationState::ObservedComplete, "{pass:?}");
    let effect = &pass.attempts[0].observations.last().unwrap()["effect"]["evidence"];
    assert_eq!(effect["source"], input);
    assert_eq!(effect["requiredGates"][0]["binding"], gates[0]);
    assert_eq!(
        effect["requiredGates"][0]["evidence"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let changed = a.save(&ctx, br#"{"text":"invalid"}"#);
    let failure = await_result(
        &a,
        operation_for(&a, &ctx, &changed, &digest, gates.clone()),
    );
    assert_eq!(failure.state, OperationState::Failed, "{failure:?}");
    assert_eq!(failure.payload["workflow"]["requiredGates"], gates);

    assert_eq!(
        refused(operation_for(&a, &ctx, &input, &digest, json!([]))).0,
        DomainErrorCode::Conflict
    );
    let stale = json!([{"packageId":"workflow-pack","contributionId":"workflow-pack/workflow-gate",
        "manifestDigest":"f".repeat(64),"reason":"Test a stale selection."}]);
    assert_eq!(
        refused(operation_for(&a, &ctx, &input, &digest, stale)).0,
        DomainErrorCode::NotFound
    );
}

#[test]
fn a_scheduled_workflow_observes_gate_failure_instead_of_the_queue_receipt() {
    let _s = serial();
    let a = App::new();
    let mut spec = Spec::new("workflow-pack", "1.0.0");
    spec.workflow = true;
    spec.contributions = vec!["workflow-step", "workflow-gate"];
    let digest = digest_of(&package(a.install(&a.user, &spec)));
    a.ready("workflow-pack");
    let input = a.save(&a.user, br#"{"text":"invalid"}"#);
    let operation = "addons.run_workflow";
    let schema = devworks_core::services::jobs::argument_schema(
        operation,
        devworks_core::registry::SchemaRevision(9),
    )
    .expect("native workflow arguments");
    let definition = json!({
        "kind":"Native","capabilityId":devworks_core::services::jobs::capability_id(operation),
        "arguments":{"schema":schema,"value":{
            "workflowId":"scheduled-check","packageId":"workflow-pack",
            "contributionId":"workflow-pack/workflow-step","manifestDigest":digest,"input":input,
            "requiredGates":[{"packageId":"workflow-pack","contributionId":"workflow-pack/workflow-gate",
                "manifestDigest":digest,"reason":"Require the fixture's compatibility check."}]
        }}
    });
    let result = applied(a.domain(
        &a.user,
        "jobs.create",
        json!({
            "kind":"jobs.create","scope":{"kind":"Workspace"},"title":"Package workflow",
            "schedule":{"kind":"Manual"},"definition":definition,"enabled":true
        }),
    ));
    let OperationResult::JobsCreate { value: job } = result else {
        panic!("{result:?}")
    };
    let result = applied(a.domain(
        &a.user,
        "jobs.run_now",
        json!({
            "kind":"jobs.run_now","jobId":job.id,"expectedRevision":job.revision
        }),
    ));
    let OperationResult::JobsRunNow { value: run } = result else {
        panic!("{result:?}")
    };
    let until = Instant::now() + PATIENCE;
    loop {
        let id = run.id.clone();
        let record = read(&*a.store, move |s| s.jobs().job_run(&id)).unwrap();
        if matches!(
            record.status.as_str(),
            "Succeeded" | "Failed" | "OutcomeUnknown"
        ) {
            assert_eq!(record.status, "Failed", "{record:?}");
            assert!(record.inner_operation_id.is_some(), "{record:?}");
            break;
        }
        assert!(Instant::now() < until, "{record:?}");
        super::event_intents::drive(&a, Duration::from_millis(25));
    }
}
