//! Native signed-package registry resolution, including legacy exact-bundle reinspection.
use super::*;
use devworks_core::protocol::provider::{SchemaRef, ToolSchema};
use devworks_core::providers::materialize::SchemaDocuments;
use devworks_engine::providers::schemas::RegisteredSchemas;
fn schema_bundle() -> (Vec<u8>, String, ToolSchema, Value) {
    let document = json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false});
    let schema_bytes = devworks_core::jcs::canonicalize(&document)
        .unwrap()
        .into_bytes();
    let reference = schema("schematool/input", &sha256_hex(&schema_bytes));
    let entry = if cfg!(windows) {
        "bin/helper.exe"
    } else {
        "bin/helper"
    };
    let files = [
        (entry.to_string(), helper_bytes()),
        (
            "fixture.json".into(),
            json!({"mode":"normal","inventory":["schematool/echo","schematool/call"]})
                .to_string()
                .into_bytes(),
        ),
        (
            "help.md".into(),
            b"# echo\nReturns the provided text.\n".to_vec(),
        ),
        ("schemas/input.json".into(), schema_bytes),
    ];
    let manifest=json!({"packageId":"schematool","version":"1.0.0","publisherId":"acme","protocolMin":{"major":1,"minor":0},"protocolMax":{"major":1,"minor":0},"minimumHostVersion":"0.1.0","dependencies":[],"runtimes":[],
        "schemaAssets":[{"reference":reference,"path":"schemas/input.json"}],"requestedLimits":{"maxConcurrent":4,"queueCapacity":16,"maxMemoryBytes":"1073741824","streamCreditBytes":65536},
        "targets":[{"targetTriple":host_target_triple(),"entrypoint":entry}],"files":files.iter().map(|(p,b)|json!({"path":p,"sha256":sha256_hex(b),"byteLength":b.len().to_string()})).collect::<Vec<_>>(),"stateSchema":reference,"configurationSchema":reference,"helpFile":"help.md","requestedPermissions":[{"kind":"StateWrite"}],
        "contributions":[{"kind":"Tool","meta":{"id":"schematool/echo","inputSchema":reference,"outputSchema":reference,"helpTopic":"echo","required":false,"cancellation":"Cooperative"},"requestedEffect":"ReadCandidate"},
            {"kind":"Tool","meta":{"id":"schematool/call","inputSchema":reference,"outputSchema":reference,"helpTopic":"call","required":false,"cancellation":"Cooperative"},"requestedEffect":"ReadCandidate"}]}).to_string().into_bytes();
    let refs = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect::<Vec<_>>();
    let tool = ToolSchema {
        capability_id: "schematool/echo".to_string().try_into().unwrap(),
        wire_name: "package_echo".into(),
        input_schema: serde_json::from_value::<SchemaRef>(reference).unwrap(),
    };
    (
        write_bundle(&manifest, &refs, Some(&key(1))),
        sha256_hex(&manifest),
        tool,
        document,
    )
}
#[test]
fn registered_provider_tools_use_verified_active_package_schema_metadata_and_legacy_source() {
    let _serial = serial();
    let app = App::new();
    let (bundle, digest, tool, document) = schema_bundle();
    let source = app.save(&app.user, &bundle);
    applied(app.lifecycle(&app.user,"addons.install",json!({"kind":"Install","bundle":source,"expectedRegistryRevision":app.registry().to_string()})));
    let service = app.app().packages().unwrap().service().clone();
    let registry = RegisteredSchemas::new(
        app.store.clone(),
        UseCases::new().with_packages(service.clone()),
        Some(service.clone()),
        app.app().artifacts().clone(),
    );
    // An installed package does not select its tools for agents. The user's workspace
    // choice is required before its schema can be presented to the provider.
    assert!(registry.tool(&app.agent, &tool).is_err());
    select_for_agents(&app, tool.capability_id.as_str());
    let resolved = registry.tool(&app.agent, &tool).unwrap();
    assert_eq!(resolved.schema, document);
    assert!(
        resolved
            .description
            .unwrap()
            .contains("Returns the provided text")
    );
    let mut wrong = tool.clone();
    wrong.capability_id = "schematool/not-registered".to_string().try_into().unwrap();
    assert!(registry.tool(&app.agent, &wrong).is_err());
    // A pre-DW19 compact inspection is recovered only from this exact retained signed source.
    let raw = rusqlite::Connection::open(app.h.root.join("workspace.sqlite")).unwrap();
    let value:String=raw.query_row("SELECT inspection_json FROM package_versions WHERE package_id='schematool' AND manifest_digest=?1",[&digest],|r|r.get(0)).unwrap();
    let mut legacy: Value = serde_json::from_str(&value).unwrap();
    legacy.as_object_mut().unwrap().remove("schemaAssets");
    for c in legacy["contributions"].as_array_mut().unwrap() {
        c.as_object_mut().unwrap().remove("descriptor");
    }
    raw.execute("UPDATE package_versions SET inspection_json=?1 WHERE package_id='schematool' AND manifest_digest=?2",rusqlite::params![legacy.to_string(),digest]).unwrap();
    assert_eq!(registry.tool(&app.agent, &tool).unwrap().schema, document);
    // Even a correctly declared digest cannot qualify bytes changed in the installed version.
    let path = service
        .files()
        .path("schematool", &digest, "schemas/input.json");
    let original = std::fs::read(&path).unwrap();
    std::fs::write(&path, b"{}").unwrap();
    assert!(registry.tool(&app.agent, &tool).is_err());
    std::fs::write(&path, original).unwrap();
    applied(app.simple(&app.user, "addons.disable", "Disable", "schematool"));
    assert!(registry.tool(&app.agent, &tool).is_err());
}

/// The user's workspace choice that offers `capability` to agents.
fn select_for_agents(app: &App, capability: &str) {
    let selected_tool = capability.to_owned();
    write(app.store.as_ref(), move |tx| {
        let current = devworks_core::settings::model::current_layer(
            tx.configuration(),
            &ConfigScope::Workspace,
        )?;
        let descriptor = devworks_core::settings::catalog::descriptor(
            devworks_core::settings::catalog::TOOL_OVERRIDES_KEY,
        )
        .unwrap();
        let assignment = serde_json::from_value(json!({
            "key": descriptor.key,
            "value": {"schema": descriptor.schema_ref(),
                "value": {(selected_tool): true}}
        }))
        .unwrap();
        let plan = devworks_core::settings::model::plan(&current, &[assignment]);
        let body = devworks_core::settings::model::body(
            &plan,
            &ActorId(id("user-a")),
            tx.now_ms(),
            json!({"kind": "test"}),
        );
        tx.configuration().append_revision(
            &ConfigScope::Workspace,
            devworks_core::settings::model::NAMESPACE,
            current.revision,
            devworks_core::settings::model::BODY_SCHEMA_VERSION,
            body,
        )?;
        Ok(())
    })
    .unwrap();
}

#[test]
fn an_agents_package_tool_call_runs_as_a_durable_invocation_of_the_selected_package() {
    use devworks_core::protocol::provider::{ArtifactRef, CompleteToolCall, ToolCallKey};
    use devworks_core::protocol::{AttemptId, Counter, OperationState};
    let _serial = serial();
    let app = App::new();
    let (bundle, _, tool, _) = schema_bundle();
    let source = app.save(&app.user, &bundle);
    applied(app.lifecycle(&app.user,"addons.install",json!({"kind":"Install","bundle":source,"expectedRegistryRevision":app.registry().to_string()})));
    app.ready("schematool");
    let service = app.app().packages().unwrap().service().clone();
    let registry = RegisteredSchemas::new(
        app.store.clone(),
        UseCases::new().with_packages(service.clone()),
        Some(service),
        app.app().artifacts().clone(),
    );
    // The agent's own open session recorded the call.
    let session = id("package-session");
    let agent = app.agent.clone().with_session(session.clone());
    write(app.store.as_ref(), |tx| {
        tx.sessions().insert_session(&StoredSession {
            id: id("package-session"),
            conversation_id: id("package-conversation"),
            agent_id: ActorId(id("agent-a")),
            project_id: None,
            root_id: None,
            title: "Package tool agent".into(),
            context_epoch: 0,
            revision: 1,
            created_by: ActorId(id("user-a")),
            created_at_ms: 0,
            closed_at_ms: None,
            closed_by: None,
            next_sequence: 1,
            checkpoint_sequence: 0,
        })
    })
    .unwrap();
    let arguments: ArtifactRef =
        serde_json::from_value(app.save(&app.user, br#"{"text":"hello"}"#)).unwrap();
    let call = CompleteToolCall {
        call: ToolCallKey {
            attempt_id: AttemptId(id("provider-attempt")),
            native_call_id: "call-1".into(),
            ordinal: Counter::new(0).unwrap(),
        },
        capability_id: tool.capability_id.clone(),
        input_schema: tool.input_schema.clone(),
        arguments: arguments.clone(),
        native_item: arguments.clone(),
    };
    let queue =
        || devworks_engine::package_tools::queue(&*app.store, &registry, &agent, &session, &call);
    // A tool the person did not offer to agents is never invoked.
    assert!(matches!(queue(), Err(StorageError::ScopeDenied)));
    select_for_agents(&app, tool.capability_id.as_str());
    let operation = queue().unwrap();
    // One durable invocation per call.
    assert_eq!(queue().unwrap(), operation);
    // The dispatcher runs on the application's runtime, which this thread drives while waiting.
    let until = Instant::now() + PATIENCE;
    let settled = app.rt.block_on(async {
        loop {
            let current =
                read(app.store.as_ref(), |s| s.operations().operation(&operation)).unwrap();
            if !matches!(
                current.state,
                OperationState::IntentRecorded
                    | OperationState::Preflighted
                    | OperationState::Dispatching
            ) {
                break current;
            }
            assert!(Instant::now() < until, "{current:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    assert_eq!(
        settled.state,
        OperationState::ObservedComplete,
        "{settled:?}"
    );
    assert_eq!(
        settled.capability,
        devworks_engine::package_tools::CAPABILITY
    );
    // The package echoed its input: the call's own arguments.
    let evidence = settled
        .latest_attempt()
        .unwrap()
        .observations
        .iter()
        .find_map(|o| o["effect"].get("evidence").cloned())
        .unwrap();
    assert_eq!(evidence["artifacts"], json!([arguments]));
    // The invocation is attributed to the package version under that operation.
    let invoked = read(app.store.as_ref(), |s| {
        s.history().page(
            &EventFilter::Aggregate {
                kind: "package".into(),
                id: "schematool".into(),
            },
            0,
            500,
        )
    })
    .events
    .into_iter()
    .filter(|e| e.kind == "PackageInvoked")
    .collect::<Vec<_>>();
    assert_eq!(invoked.len(), 1);
    assert_eq!(invoked[0].payload["operationId"], operation.0.as_str());

    // An artifact that is not the invocation's own is never accepted as its output.
    let forging = tool.capability_id.as_str().replace("/echo", "/call");
    select_for_agents(&app, &forging);
    let script = std::env::temp_dir().join("devworks-fixture-schematool-call.json");
    std::fs::write(&script, json!({"calls": [], "forge": true}).to_string()).unwrap();
    let mut forged = call.clone();
    forged.call.native_call_id = "call-2".into();
    forged.capability_id = forging.try_into().unwrap();
    let operation =
        devworks_engine::package_tools::queue(&*app.store, &registry, &agent, &session, &forged)
            .unwrap();
    let until = Instant::now() + PATIENCE;
    let settled = app.rt.block_on(async {
        loop {
            let current =
                read(app.store.as_ref(), |s| s.operations().operation(&operation)).unwrap();
            if !matches!(
                current.state,
                OperationState::IntentRecorded
                    | OperationState::Preflighted
                    | OperationState::Dispatching
            ) {
                break current;
            }
            assert!(Instant::now() < until, "{current:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    let _ = std::fs::remove_file(&script);
    assert_eq!(settled.state, OperationState::Failed, "{settled:?}");
    assert!(
        format!("{settled:?}").contains("not its invocation's"),
        "{settled:?}"
    );
}
