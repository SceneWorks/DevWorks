//! Native Home first-turn admission with deterministic SystemOne and conversational transports.
use super::workspace_reply::{open_home, wait_for_replies, workspace_ports};
use super::*;
use devworks_core::ports::secrets::{Secret, SecretKey};
use devworks_core::services::router::auto_routing::Assignment;

#[derive(Default)]
struct AutoWire {
    requests: Mutex<Vec<Value>>,
}
impl StreamingHttp for AutoWire {
    fn open(
        &self,
        request: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        let value: Value = serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
        let model = value["model"].as_str().unwrap();
        let systemone = model.starts_with("decision-");
        let body = if systemone {
            let tier = if model == "decision-b-upstream" && !value.to_string().contains("fallback")
            {
                "High"
            } else {
                "invalid-tier"
            };
            json!({
                "model":model, "answers":{"tier":{
                    "type":"choice","choice":tier,
                    "probabilities":{"Low":0.05,"Mid":0.05,"High":0.85,"Max":0.05},
                    "confidence":0.85,
                }}, "usage":{"input_tokens":10,"output_tokens":0},
            })
            .to_string()
        } else {
            let reply = match model {
                // A completed but oversized Low answer must leave the next candidate eligible.
                "low-a-upstream" => "x".repeat(1025),
                "gpt-6-luna" => "High".into(),
                _ => "completed turn".into(),
            };
            let body = json!({
                "choices":[{"index":0,"delta":{"content":reply},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":15,"completion_tokens":2},
            });
            format!("data: {body}\n\ndata: [DONE]\n\n")
        };
        let rejects_model = model == "decision-a-upstream"
            && value.to_string().contains("model-specific rejection");
        self.requests.lock().unwrap().push(value);
        Ok(Box::new(AgentStream {
            head: ResponseHead {
                status: if rejects_model { 422 } else { 200 },
                headers: vec![(
                    "content-type".into(),
                    if systemone {
                        "application/json".into()
                    } else {
                        "text/event-stream".into()
                    },
                )],
            },
            body: Some(body.into_bytes()),
        }))
    }
}

fn seed_auto(f: &Fx, managed: bool) {
    seed_local_route(f);
    f.tx(move |_, tx| {
        let template = tx.router().route_model(&id("watch-model"))?.unwrap();
        for (name, position) in [("low-a", 2), ("low-b", 3), ("same-tier-backup", 4)] {
            let mut model = template.clone();
            model.id = id(name);
            model.display_name = name.into();
            model.upstream_model_id = if name == "low-b" {
                "gpt-6-luna".into()
            } else {
                format!("{name}-upstream")
            };
            model.position = position;
            if name == "same-tier-backup" {
                model.image_input = true;
            }
            tx.router().put_route_model(&model, None)?;
        }
        let mut connection = tx.router().connection(&id("watch-connection"))?.unwrap();
        connection.discovered = Some(
            [
                "fixture-model",
                "low-a-upstream",
                "gpt-6-luna",
                "same-tier-backup-upstream",
            ]
            .iter()
            .map(|name| DiscoveredModel {
                id: (*name).into(),
                context_tokens: Some(16_000),
            })
            .collect(),
        );
        tx.router()
            .put_connection(&connection, Some(connection.revision))?;
        let values: serde_json::Map<String, Value> = [
            ("routing.tierLow.members", json!(["low-a","luna-latest"])),
            ("routing.tierHigh.members", json!(["watch-model","same-tier-backup"])),
            ("routing.managedAuto", json!(managed)),
        ].into_iter().map(|(key, value)| (key.into(), json!({
            "schema": devworks_core::settings::catalog::descriptor(key).unwrap().schema_ref(),
            "value": value,
        }))).collect();
        tx.configuration().append_revision(
            &ConfigScope::Workspace,
            "settings",
            1,
            1,
            json!({"values":values}),
        )?;
        Ok(())
    });
}

fn seed_systemone(f: &Fx) {
    f.tx(|_, tx| {
        let mut con = tx.router().connection(&id("watch-connection"))?.unwrap();
        con.id = id("decision-connection");
        con.name = "Decision fixture".into();
        con.profile = "SystemOne".into();
        con.endpoint = "https://systemone.example.test/v1".into();
        con.local_free = false;
        con.credential_ref = Some(id("decision-credential"));
        con.credential_revision = 1;
        con.credential_epoch = Some(tx.store().restore_epoch);
        con.discovered = Some(vec![
            DiscoveredModel { id: "decision-a-upstream".into(), context_tokens: None },
            DiscoveredModel { id: "decision-b-upstream".into(), context_tokens: None },
        ]);
        tx.router().put_connection(&con, None)?;
        let result_schema = serde_json::to_string(&devworks_core::providers::systemone::result_reference()).unwrap();
        for name in ["decision-a", "decision-b"] {
            tx.router().put_decision_model(&StoredDecisionModel {
                id: id(name), display_name: name.into(), connection_id: con.id.clone(),
                upstream_model_id: format!("{name}-upstream"), question_types: vec!["choice".into()],
                accepts_images: false, accepts_videos: false, enabled: true,
                extension_schemas: "[]".into(), result_schema: result_schema.clone(),
                decision_policy: Some(json!({"poolMember":true,"maxInputBytes":"8192","maxQuestionCount":"1","maxBatchBytes":"16384"}).to_string()),
                revision: 1, updated_at_ms: 0,
            }, None)?;
        }
        Ok(())
    });
}

fn auto_ports(f: &Fx, wire: Arc<AutoWire>, systemone: bool) -> Composition {
    let mut ports = workspace_ports(f, wire);
    if systemone {
        ports.config.providers = Some(
            ports
                .config
                .providers
                .take()
                .unwrap()
                .with_systemone_validator(Arc::new(
                    devworks_adapters::packages::JsonSchemaValidator,
                )),
        );
        let credential = devworks_core::services::router::connections::auth::Credential::ApiKey(
            Secret::new("systemone-test-key"),
        );
        ports
            .application
            .connections
            .as_ref()
            .unwrap()
            .secrets
            .put(
                &SecretKey::new(
                    f.store.info().store_id.0.as_str(),
                    id("decision-credential"),
                ),
                &credential.to_secret(),
            )
            .unwrap();
    }
    ports
}

fn send_auto(
    f: &Fx,
    app: &devworks_engine::Runtime,
    session: &Id,
    key: &str,
    text: &str,
) -> Vec<u8> {
    let current = read(&*f.store, |read| read.sessions().session(session)).unwrap();
    let frame = json!({
        "protocol":PROTOCOL,"schemaRevision":3,"requestKey":key,"sessionId":session,
        "expectedSessionRevision":current.revision.to_string(),"content":[{"kind":"Text","text":text}],
        "replyRoute":{"intent":{"kind":"DevWorksAuto"},"effort":"Default"},
    });
    let frame = serde_json::to_vec(&frame).unwrap();
    applied(
        &app.command(&f.user, "sessions.append_message", &frame)
            .unwrap(),
    );
    frame
}

fn assignment(f: &Fx, session: &Id) -> Assignment {
    let record = read(&*f.store, |read| read.router().auto_route(session)).unwrap();
    let assignment: Assignment = serde_json::from_value(record.decision.unwrap()).unwrap();
    assert_eq!(assignment.input_digest, record.source["material"]["sha256"]);
    assert_eq!(assignment.rubric_version, 1);
    assignment
}

#[test]
fn systemone_checks_later_candidates_before_direct_low_and_reuses_the_chosen_tier() {
    let _native_case = native_runtime_case();
    let f = Fx::new();
    seed_auto(&f, false);
    seed_systemone(&f);
    let wire = Arc::new(AutoWire::default());
    runtime().block_on(async {
        let app = boot(auto_ports(&f, wire.clone(), true)).await.unwrap();
        let first = open_home(&f, &app);
        send_auto(
            &f,
            &app,
            &first,
            "systemone-first",
            "first authorized prompt",
        );
        send_auto(
            &f,
            &app,
            &first,
            "systemone-later",
            "later prompt excluded from classification",
        );
        wait_for_replies(&f, 2).await;
        assert_eq!(assignment(&f, &first).model_id, id("decision-b"));
        let request_models = |requests: &[Value]| -> Vec<String> {
            requests
                .iter()
                .map(|r| r["model"].as_str().unwrap().to_owned())
                .collect()
        };
        {
            let requests = wire.requests.lock().unwrap();
            assert_eq!(
                request_models(&requests),
                [
                    "decision-a-upstream",
                    "decision-b-upstream",
                    "fixture-model",
                    "fixture-model",
                ]
            );
            for request in &requests[..2] {
                assert!(request.to_string().contains("first authorized prompt"));
                assert!(!request.to_string().contains("later prompt excluded"));
            }
        }
        assert_eq!(read(&*f.store, |r| r.router().auto_low_cursor()), 0);
        let initial = assignment(&f, &first).request_id;
        app.shutdown().await;

        let app = boot(auto_ports(&f, wire.clone(), true)).await.unwrap();
        let next = send_auto(
            &f,
            &app,
            &first,
            "systemone-reopened",
            "another prompt after reopening",
        );
        wait_for_replies(&f, 3).await;
        assert!(replayed(
            &app.command(&f.user, "sessions.append_message", &next)
                .unwrap()
        ));
        assert_eq!(assignment(&f, &first).request_id, initial);
        assert_eq!(wire.requests.lock().unwrap().len(), 5);

        let start = json!({"mode":"Command","protocol":PROTOCOL,"requestKey":"fallback-home",
            "body":{"kind":"sessions.start","scope":{"kind":"Workspace"},"title":"Fallback"}});
        let second = applied(
            &app.command(
                &f.user,
                "sessions.start",
                &serde_json::to_vec(&start).unwrap(),
            )
            .unwrap(),
        );
        let second = id(second["id"].as_str().unwrap());
        send_auto(
            &f,
            &app,
            &second,
            "systemone-fallback",
            "please use the fallback case",
        );
        wait_for_replies(&f, 4).await;
        assert_eq!(assignment(&f, &second).model_id, id("low-b"));
        {
            let requests = wire.requests.lock().unwrap();
            assert_eq!(
                request_models(&requests[5..]),
                [
                    "decision-b-upstream",
                    "decision-a-upstream",
                    "low-a-upstream",
                    "gpt-6-luna",
                    "same-tier-backup-upstream",
                ]
            );
        }
        assert_eq!(read(&*f.store, |r| r.router().tier_cursor("Low")), 0);
        assert_eq!(read(&*f.store, |r| r.router().auto_low_cursor()), 1);

        let start = json!({"mode":"Command","protocol":PROTOCOL,"requestKey":"rejected-model-home",
            "body":{"kind":"sessions.start","scope":{"kind":"Workspace"},"title":"Rejection"}});
        let third = applied(
            &app.command(
                &f.user,
                "sessions.start",
                &serde_json::to_vec(&start).unwrap(),
            )
            .unwrap(),
        );
        let third = id(third["id"].as_str().unwrap());
        send_auto(
            &f,
            &app,
            &third,
            "rejected-candidate",
            "Test a model-specific rejection",
        );
        wait_for_replies(&f, 5).await;
        assert_eq!(assignment(&f, &third).model_id, id("decision-b"));
        assert_eq!(read(&*f.store, |r| r.router().auto_low_cursor()), 1);
        assert_eq!(
            request_models(&wire.requests.lock().unwrap()[10..]),
            [
                "decision-a-upstream",
                "decision-b-upstream",
                "fixture-model",
            ]
        );
        let start = json!({"mode":"Command","protocol":PROTOCOL,"requestKey":"low-rotation-home",
            "body":{"kind":"sessions.start","scope":{"kind":"Workspace"},"title":"Rotate Low"}});
        let fourth = applied(
            &app.command(
                &f.user,
                "sessions.start",
                &serde_json::to_vec(&start).unwrap(),
            )
            .unwrap(),
        );
        let fourth = id(fourth["id"].as_str().unwrap());
        send_auto(
            &f,
            &app,
            &fourth,
            "low-rotation",
            "Use the fallback case to check Low rotation",
        );
        wait_for_replies(&f, 6).await;
        assert_eq!(assignment(&f, &fourth).model_id, id("low-b"));
        assert_eq!(read(&*f.store, |r| r.router().auto_low_cursor()), 2);
        assert_eq!(read(&*f.store, |r| r.router().tier_cursor("Low")), 0);
        assert_eq!(
            request_models(&wire.requests.lock().unwrap()[13..]),
            [
                "decision-b-upstream",
                "decision-a-upstream",
                "gpt-6-luna",
                "same-tier-backup-upstream",
            ]
        );
        app.shutdown().await;
    });
}

#[test]
fn a_saved_automatic_tier_handles_capability_failover_and_explicit_restoration() {
    let _native_case = native_runtime_case();
    let f = Fx::new();
    seed_auto(&f, false);
    let wire = Arc::new(AutoWire::default());
    runtime().block_on(async {
        let app = boot(auto_ports(&f, wire.clone(), false)).await.unwrap();
        let session = open_home(&f, &app);
        send_auto(&f, &app, &session, "auto-before-failover", "A localized model routing check");
        wait_for_replies(&f, 1).await;
        let saved = assignment(&f, &session);
        let pin = read(&*f.store, |read| read.router().session_pin(&session)).unwrap();
        assert_eq!(pin.model_id, id("watch-model"));

        let route = |key: &str, need: &str| {
            let raw = json!({"mode":"Command","protocol":PROTOCOL,"schemaRevision":3,"requestKey":key,
                "body":{"kind":"router.route","sessionId":session,"expectedContextEpoch":"0",
                "intent":{"kind":"DevWorksAuto"},"effort":"Default","requiredCapabilities":[need]}});
            app.command(&f.user, "router.route", &serde_json::to_vec(&raw).unwrap()).unwrap()
        };
        let vision = applied(&route("automatic-vision-replacement", "vision"));
        assert_eq!(vision["modelId"], "same-tier-backup");
        assert_eq!(vision["replacedPinId"], pin.pin_id.as_str());
        let unavailable = route("automatic-no-capable-member", "context:20000");
        assert!(matches!(unavailable, CommandReply::Recorded { receipt, .. }
            if matches!(&receipt.decision, DomainDecision::Refused { fault }
                if fault.code == DomainErrorCode::UnsupportedCapability)));

        // Manual override must still satisfy the saved capability, so clear it with an
        // ordinary re-admission first. The replacement remains while it stays eligible.
        let previous = read(&*f.store, |read| read.router().session_pin(&session)).unwrap();
        let clear = json!({"mode":"Command","protocol":PROTOCOL,"schemaRevision":3,"requestKey":"auto-clear-vision",
            "body":{"kind":"router.route","sessionId":session,"expectedContextEpoch":"0",
            "intent":{"kind":"DevWorksAuto"},"effort":"Default","requiredCapabilities":[]}});
        applied(&app.command(&f.user, "router.route", &serde_json::to_vec(&clear).unwrap()).unwrap());
        let manual = |key: &str, intent: Value| {
            let pin = read(&*f.store, |read| read.router().session_pin(&session)).unwrap();
            let raw = json!({"mode":"Command","protocol":PROTOCOL,"schemaRevision":3,"requestKey":key,
                "body":{"kind":"router.pin","sessionId":session,"expectedPinRevision":pin.revision.to_string(),
                "intent":intent,"effort":"Default"}});
            applied(&app.command(&f.user, "router.pin", &serde_json::to_vec(&raw).unwrap()).unwrap())
        };
        assert_eq!(manual("auto-manual-override", json!({"kind":"Model","modelId":"low-a"}))["modelId"], "low-a");
        let stale = json!({"mode":"Command","protocol":PROTOCOL,"schemaRevision":3,"requestKey":"stale-automatic-route",
            "body":{"kind":"router.route","sessionId":session,"expectedContextEpoch":"0",
            "expectedPinRevision":previous.revision.to_string(),"requiredCapabilities":[],
            "intent":{"kind":"DevWorksAuto"},"effort":"Default"}});
        let stale = app.command(&f.user, "router.route", &serde_json::to_vec(&stale).unwrap()).unwrap();
        assert!(matches!(stale, CommandReply::Recorded { receipt, .. }
            if matches!(&receipt.decision, DomainDecision::Refused { fault }
                if fault.code == DomainErrorCode::SourceChanged)));
        assert_eq!(read(&*f.store, |r| r.router().session_pin(&session)).unwrap().model_id, id("low-a"));
        let restored = manual("auto-restored", json!({"kind":"DevWorksAuto"}));
        assert_ne!(restored["modelId"], "low-a");
        assert_eq!(assignment(&f, &session).request_id, saved.request_id);
        assert_eq!(assignment(&f, &session).input_digest, saved.input_digest);
        assert_eq!(previous.reason, "Failover");
        assert_eq!(wire.requests.lock().unwrap().len(), 3);
        assert_eq!(read(&*f.store, |r| r.router().tier_cursor("Low")), 0);
        app.shutdown().await;
    });
}

#[test]
fn a_delegated_worker_classifies_its_own_first_assignment_with_project_attribution() {
    let _native_case = native_runtime_case();
    let f = Fx::new();
    seed_auto(&f, true);
    let wire = Arc::new(AutoWire::default());
    runtime().block_on(async {
        let app = Arc::new(boot(auto_ports(&f, wire.clone(), false)).await.unwrap());
        applied(&f.start(&f.user, "root-i", 1, 1));
        let activation = f.launch("auto-worker-runner");
        let host = principal(&*f.store, "auto-worker-host", PrincipalKind::Host);
        let epoch = activation.owner_epoch;
        let execution = activation.id.clone();
        let agents = Arc::new(AgentService::new(f.h.ids.clone(), f.service.clone()));
        let primary = {
            let agents = agents.clone();
            f.tx(move |_, tx| {
                agents
                    .materialize_primary(tx, &host, &execution, epoch)
                    .map_err(|error| StorageError::InvalidRecord(format!("primary: {error:?}")))
            })
        };
        let parent = read(&*f.store, |snapshot| {
            let assignment = snapshot.runs().assignment(&primary.assignment_id)?.unwrap();
            snapshot.sessions().session(&assignment.session_id)
        })
        .unwrap();
        let coordinator = devworks_core::context::HostIngress::new(&*f.store)
            .context(
                parent.agent_id,
                PrincipalKind::Agent,
                id("delegate-auto-worker"),
                id("auto-worker-ingress"),
                devworks_core::protocol::Counter::new(f.store.info().restore_epoch).unwrap(),
                Revision::new(1).unwrap(),
            )
            .with_session(parent.id.clone())
            .with_lineage(
                Some(ProjectId(id("project-a"))),
                Some(rid("root-i")),
                Some(primary.assignment_id),
            )
            .with_activation(activation.id.clone(), epoch);
        let child = {
            let agents = agents.clone();
            let input = devworks_core::operations::agents::DelegateInput {
                root_id: rid("root-i"),
                expected_control_revision: Revision::new(f.control("root-i").revision).unwrap(),
                task_ids: devworks_core::protocol::support::BoundedList(Vec::new()),
                role_id: id("worker"),
                instruction: devworks_core::protocol::support::BoundedText(
                    "Investigate the worker's own assignment.".into(),
                ),
                source_checkpoint_id: None,
            };
            f.tx(move |_, tx| {
                agents
                    .delegate(tx, &coordinator, &input)
                    .map_err(|error| StorageError::InvalidRecord(format!("delegate: {error:?}")))
            })
        };
        let launcher = AgentLauncher::new(
            f.store.clone(),
            f.h.ids.clone(),
            agents,
            Arc::new(NativeAgentTurnExecutor::new(&app)),
        );
        let (_stop_tx, stop) = tokio::sync::watch::channel(false);
        let end = launcher
            .launch_child(child.assignment_id.clone(), activation.id, epoch, stop)
            .await;
        assert!(
            matches!(
                end,
                ChildLaunch::End(LaunchEnd::Finished {
                    outcome: ActivationOutcome::Completed,
                    ..
                })
            ),
            "{end:?}"
        );
        let child = read(&*f.store, |r| r.runs().assignment(&child.assignment_id)).unwrap();
        let assigned = assignment(&f, &child.session_id);
        assert_eq!(assigned.model_id, id("low-b"));
        let route = read(&*f.store, |r| {
            r.router().route_request(&assigned.request_id)
        })
        .unwrap();
        assert_eq!(route.project_id, Some(ProjectId(id("project-a"))));
        let operation = read(&*f.store, |r| r.operations().operation(&route.operation_id)).unwrap();
        assert_eq!(operation.root_id, Some(rid("root-i")));
        assert_eq!(route.purpose, "TierClassification");
        assert!(route.pin_id.is_none());
        assert!(read(&*f.store, |r| r.router().auto_route(&parent.id)).is_none());
        {
            let requests = wire.requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            for request in &requests[..2] {
                let input = request.to_string();
                assert!(input.contains("Investigate the worker's own assignment."));
                assert!(input.contains("Assigned role: Worker"));
                assert!(!input.contains("root-g"));
            }
        }
        app.shutdown().await;
    });
}

struct StopDuringOpen {
    inner: AutoWire,
    hold_model: &'static str,
    entered: tokio::sync::Notify,
}

impl StreamingHttp for StopDuringOpen {
    fn open(
        &self,
        request: &HttpRequest,
        limits: &StreamLimits,
        cancel: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        use devworks_core::ports::provider_http::{SendPhase, TransportFailure};

        let value: Value = serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
        if value["model"] == self.hold_model {
            self.entered.notify_one();
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            while !cancel.is_cancelled() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            return Err(TransportError {
                phase: SendPhase::NotSent,
                kind: if cancel.is_cancelled() {
                    TransportFailure::Cancelled
                } else {
                    TransportFailure::Deadline
                },
            });
        }
        self.inner.open(request, limits, cancel)
    }
}

#[test]
fn cancellation_before_and_after_the_tier_commit_never_sends_a_stale_generation() {
    let _native_case = native_runtime_case();
    for (hold_model, commits_tier) in [("gpt-6-luna", false), ("fixture-model", true)] {
        let f = Fx::new();
        seed_auto(&f, false);
        let wire = Arc::new(StopDuringOpen {
            inner: AutoWire::default(),
            hold_model,
            entered: tokio::sync::Notify::new(),
        });
        runtime().block_on(async {
            let app = boot(workspace_ports(&f, wire.clone())).await.unwrap();
            let session = open_home(&f, &app);
            let frame = send_auto(
                &f,
                &app,
                &session,
                "cancel-auto",
                "Use the initial tier once",
            );
            within(
                "classifier or generation reached stoppable transport",
                wire.entered.notified(),
            )
            .await;
            let state =
                |session: &Id| read(&*f.store, |read| read.router().auto_route(session)).unwrap();
            let before = state(&session);
            assert_eq!(before.decision.is_some(), commits_tier);
            app.shutdown().await;
            let wire_after = Arc::new(AutoWire::default());
            let app = boot(auto_ports(&f, wire_after, false)).await.unwrap();
            assert!(replayed(
                &app.command(&f.user, "sessions.append_message", &frame)
                    .unwrap()
            ));
            let after = state(&session);
            assert_eq!(before.source, after.source);
            assert_eq!(before.decision, after.decision);
            assert!(
                wire.inner
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|request| request["model"] != "fixture-model")
            );
            app.shutdown().await;
        });
    }
}

#[test]
fn the_tier_survives_an_unavailable_generation_pool_and_reopen_without_reclassifying() {
    let _native_case = native_runtime_case();
    let f = Fx::new();
    seed_auto(&f, false);
    let set_enabled = |enabled| {
        f.tx(move |_, tx| {
            for name in ["watch-model", "same-tier-backup"] {
                let mut model = tx.router().route_model(&id(name))?.unwrap();
                model.enabled = enabled;
                tx.router().put_route_model(&model, Some(model.revision))?;
            }
            Ok(())
        })
    };
    set_enabled(false);
    let wire = Arc::new(AutoWire::default());
    runtime().block_on(async {
        let app = boot(auto_ports(&f, wire.clone(), false)).await.unwrap();
        let session = open_home(&f, &app);
        send_auto(
            &f,
            &app,
            &session,
            "auto-selected-pool-unavailable",
            "Preserve this tier",
        );
        within("a saved tier with an unavailable generation pool", async {
            loop {
                let settled = read(&*f.store, |r| {
                    r.operations().operations_for_capability(
                        devworks_core::services::sessions::workspace_reply::CAPABILITY,
                        &[OperationState::ObservedComplete],
                        None,
                        10,
                    )
                });
                if let Some(op) = settled.first() {
                    assert!(
                        op.latest_attempt()
                            .unwrap()
                            .observations
                            .iter()
                            .any(|note| note["replySettlement"]["kind"]
                                == "WorkspaceReplyUnavailable"),
                        "{op:?}"
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        let prior = assignment(&f, &session);
        assert!(read(&*f.store, |r| r.router().session_pin(&session)).is_none());
        assert_eq!(wire.requests.lock().unwrap().len(), 2);
        app.shutdown().await;
        set_enabled(true);

        let app = boot(auto_ports(&f, wire.clone(), false)).await.unwrap();
        let frame = send_auto(
            &f,
            &app,
            &session,
            "resume-chosen-tier",
            "Continue with the stored tier",
        );
        within("generation on the previously chosen tier", async {
            loop {
                let entries = read(&*f.store, |r| r.sessions().entries(&session, 0, 30));
                if entries.iter().any(|entry| {
                    entry.kind == "AssistantAttempt"
                        && entry.body["visibleText"] == "completed turn"
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(replayed(
            &app.command(&f.user, "sessions.append_message", &frame)
                .unwrap()
        ));
        assert_eq!(assignment(&f, &session).request_id, prior.request_id);
        assert_eq!(wire.requests.lock().unwrap().len(), 3);
        app.shutdown().await;
    });
}
