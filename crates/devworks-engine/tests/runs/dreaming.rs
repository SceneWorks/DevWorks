//! Drive native Dreaming admission from the gateway and observe the SQLite owner and transport.
use super::workspace_reply::workspace_ports;
use super::*;

struct DreamWire(Arc<AtomicUsize>);

impl StreamingHttp for DreamWire {
    fn open(
        &self,
        request: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        let body: Value = serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["model"], "fixture-model");
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 15);
        assert!(tools.iter().all(|tool| {
            tool["function"]["name"]
                .as_str()
                .unwrap()
                .starts_with("dream_")
        }));
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(AgentStream {
            head: ResponseHead { status: 200, headers: vec![("content-type".into(), "text/event-stream".into())] },
            body: Some(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"No changes warranted.\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":4}}\n\ndata: [DONE]\n\n".to_vec()),
        }))
    }
}

fn run(
    app: &devworks_engine::Runtime,
    fx: &Fx,
    project: &str,
    expected: i64,
) -> DomainCommandReply {
    let frame = json!({"mode":"Command","protocol":PROTOCOL,"schemaRevision":9,"requestKey":fx.key(),
        "body":{"kind":"dreaming.run","scope":{"kind":"Project","projectId":project},
                "expectedSettingsRevision":expected.to_string()}});
    app.command(&fx.user, "dreaming.run", frame.to_string().as_bytes())
        .unwrap()
}

#[test]
fn empty_or_disabled_projects_send_no_inference_and_scoped_signal_runs_once() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    seed_local_route(&fx);
    let calls = Arc::new(AtomicUsize::new(0));
    let mut ports = workspace_ports(&fx, Arc::new(DreamWire(calls.clone())));
    ports.config.providers = ports.config.providers.take().map(|mut providers| {
        providers.usage_semantics.insert(
            "watch-model".into(),
            devworks_core::services::usage::meter::InputSemantics::Inclusive,
        );
        providers.with_schema_validator(Arc::new(devworks_adapters::packages::JsonSchemaValidator))
    });
    let scope = KnowledgeScope::Project(ProjectId(id("project-a")));
    let other = KnowledgeScope::Project(ProjectId(id("project-b")));
    fx.tx({
        let scope = scope.clone();
        let other = other.clone();
        move |_, tx| {
            let policy = DreamPolicy {
                model_id: Some(id("watch-model")),
                ..Default::default()
            };
            tx.dreaming()?.set_dream_policy(&scope, 1, &policy)?;
            let disabled = DreamPolicy {
                enabled: false,
                ..Default::default()
            };
            tx.dreaming()?.set_dream_policy(&other, 1, &disabled)?;
            Ok(())
        }
    });
    runtime().block_on(async {
        let app = boot(ports).await.unwrap();
        assert_eq!(
            applied(&run(&app, &fx, "project-a", 2))["detail"]["status"],
            "NoSignal"
        );
        assert_eq!(
            applied(&run(&app, &fx, "project-b", 2))["detail"]["status"],
            "Disabled"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let base = read(&*fx.store, |read| {
            read.dreaming()?
                .dream_instructions(&KnowledgeScope::Workspace, None, 1)
        });
        assert_eq!(base[0].revision, 1);

        fx.tx(|_, tx| {
            tx.history().append(NewEvent {
                id: devworks_core::protocol::EventId(id("dream-fixture-review")),
                actor: ActorId(id("user-a")),
                project_id: Some(ProjectId(id("project-a"))),
                root_id: None,
                operation_id: None,
                aggregate_kind: "feature".into(),
                aggregate_id: "root-f".into(),
                aggregate_revision: 1,
                kind: "PlanReviewSettled".into(),
                payload: json!({"fixture":"review evidence"}),
            })?;
            Ok(())
        });
        let response = run(&app, &fx, "project-a", 2);
        let encoded = serde_json::to_value(&response).unwrap();
        assert!(encoded.to_string().contains("Accepted"), "{encoded}");
        within("the native Dreaming sweep", async {
            loop {
                let rows = read(&*fx.store, |read| {
                    read.dreaming()?.dream_runs(&scope, None, 8)
                });
                let sweep = rows.iter().find(|row| row.operation_id.is_some()).unwrap();
                if sweep.phase.ended() {
                    let operation = read(&*fx.store, |r| {
                        r.operations()
                            .operation(sweep.operation_id.as_ref().unwrap())
                    });
                    assert_eq!(
                        sweep.phase,
                        DreamPhase::Completed,
                        "{sweep:?} operation {operation:?}"
                    );
                    assert_eq!(sweep.report.as_ref().unwrap()["inputTokens"], "20");
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            applied(&run(&app, &fx, "project-a", 2))["detail"]["status"],
            "TooSoon"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let job = read(&*fx.store, |r| r.jobs().job(&id("dream-projects"))).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&job.schedule).unwrap(),
            json!({"kind":"Calendar","expression":"0 2 * * *","timezone":"UTC"}));
        fx.tx(|_, tx| {
            let scope = KnowledgeScope::Project(ProjectId(id("project-b")));
            let policy = DreamPolicy { model_id: Some(id("watch-model")), ..Default::default() };
            tx.dreaming()?.set_dream_policy(&scope, 2, &policy)?;
            tx.history().append(NewEvent {
                id: devworks_core::protocol::EventId(id("nightly-fixture-review")),
                actor: ActorId(id("user-a")), project_id: Some(ProjectId(id("project-b"))),
                root_id: None, operation_id: None, aggregate_kind: "feature".into(), aggregate_id: "root-b".into(),
                aggregate_revision: 1, kind: "PlanReviewSettled".into(), payload: json!({"fixture":"review evidence"}),
            })?;
            Ok(())
        });
        let nightly = json!({"mode":"Command","protocol":PROTOCOL,"schemaRevision":1,"requestKey":fx.key(),
            "body":{"kind":"jobs.run_now","jobId":job.id,"expectedRevision":job.revision.to_string()}});
        applied(&app.command(&fx.user, "jobs.run_now", nightly.to_string().as_bytes()).unwrap());
        let scheduled = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let workspace = read(&*fx.store, |r| r.dreaming()?.dream_runs(&KnowledgeScope::Workspace, None, 10));
                let jobs = read(&*fx.store, |r| r.jobs().job_runs(&id("dream-projects"), None, 5));
                assert!(!jobs.iter().any(|job| matches!(job.status.as_str(), "Failed" | "Blocked" | "Cancelled")), "{jobs:?}");
                let scheduled = read(&*fx.store, |r| r.dreaming()?.dream_runs(&other, None, 10));
                if let Some(sweep) = scheduled.iter().find(|row| row.trigger == DreamTrigger::Scheduled && row.phase.ended()) {
                    assert_eq!(sweep.phase, DreamPhase::Completed, "{sweep:?}");
                    let fanout = workspace.iter().find(|row| row.phase == DreamPhase::ScheduledFanout).unwrap();
                    let projects = fanout.report.as_ref().unwrap()["projects"].as_array().unwrap();
                    assert_eq!(projects.len(), 1);
                    assert_eq!(projects[0]["projectId"], "project-b");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await;
        assert!(scheduled.is_ok(), "job state {:?}, workspace {:?}, project {:?}",
            read(&*fx.store, |r| r.jobs().job_runs(&id("dream-projects"), None, 5)),
            read(&*fx.store, |r| r.dreaming()?.dream_runs(&KnowledgeScope::Workspace, None, 10)),
            read(&*fx.store, |r| r.dreaming()?.dream_runs(&other, None, 10)));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        app.shutdown().await;
    });
}

struct TrailWire(AtomicUsize);
impl StreamingHttp for TrailWire {
    fn open(
        &self,
        request: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        let raw = request.body.as_deref().unwrap();
        let body: Value = serde_json::from_slice(raw).unwrap();
        let round = self.0.fetch_add(1, Ordering::SeqCst);
        let tools = body["tools"].as_array().unwrap();
        if round > 0 {
            let messages = body["messages"].as_array().unwrap();
            let tool = messages
                .iter()
                .rev()
                .find(|row| row["role"] == "tool")
                .unwrap();
            let output: Value = serde_json::from_str(tool["content"].as_str().unwrap()).unwrap();
            if round == 1 {
                assert_eq!(output["result"]["items"][0]["id"], "dream-trail-review");
                assert_eq!(output["result"]["items"].as_array().unwrap().len(), 1);
                assert_eq!(output["action_index"], 1);
            } else {
                assert_eq!(output["result"]["historyOnly"], true);
            }
        }
        let reply = match round {
            0 | 1 => {
                let (name, args) = if round == 0 {
                    ("dream_work_trail", json!({"kind":"review","limit":1}))
                } else {
                    ("dream_read_prior", json!({"action_index":1}))
                };
                assert!(tools.iter().any(|tool| tool["function"]["name"] == name));
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":format!("private-{round}"),
                    "type":"function","function":{"name":name,"arguments":args.to_string()}}]},"finish_reason":"tool_calls"}],
                    "usage":{"prompt_tokens":20,"completion_tokens":4}})
            }
            2 => {
                json!({"choices":[{"index":0,"delta":{"content":"The project review was inspected. No changes warranted."},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":20,"completion_tokens":4}})
            }
            _ => panic!("unexpected Dreaming round {round}"),
        };
        Ok(Box::new(AgentStream {
            head: ResponseHead {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
            },
            body: Some(format!("data: {reply}\n\ndata: [DONE]\n\n").into_bytes()),
        }))
    }
}

#[test]
fn private_work_trail_and_prior_action_use_scoped_durable_receipts() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    seed_local_route(&fx);
    let wire = Arc::new(TrailWire(AtomicUsize::new(0)));
    let mut ports = workspace_ports(&fx, wire.clone());
    ports.config.providers = ports.config.providers.take().map(|mut providers| {
        providers.usage_semantics.insert(
            "watch-model".into(),
            devworks_core::services::usage::meter::InputSemantics::Inclusive,
        );
        providers.with_schema_validator(Arc::new(devworks_adapters::packages::JsonSchemaValidator))
    });
    fx.tx(|_, tx| {
        let scope = KnowledgeScope::Project(ProjectId(id("project-a")));
        let policy = DreamPolicy {
            model_id: Some(id("watch-model")),
            ..Default::default()
        };
        tx.dreaming()?.set_dream_policy(&scope, 1, &policy)?;
        for (id_value, project, feature) in [
            ("dream-trail-review", "project-a", "root-f"),
            ("foreign-dream-review", "project-b", "root-b"),
        ] {
            tx.history().append(NewEvent {
                id: devworks_core::protocol::EventId(id(id_value)),
                actor: ActorId(id("user-a")),
                project_id: Some(ProjectId(id(project))),
                root_id: None,
                operation_id: None,
                aggregate_kind: "feature".into(),
                aggregate_id: feature.into(),
                aggregate_revision: 1,
                kind: "PlanReviewSettled".into(),
                payload: json!({"review":"scoped evidence"}),
            })?;
        }
        Ok(())
    });
    runtime().block_on(async {
        let app = boot(ports).await.unwrap();
        let result = serde_json::to_value(run(&app, &fx, "project-a", 2)).unwrap();
        assert!(result.to_string().contains("Accepted"), "{result}");
        within("the private Dreaming tool rounds", async {
            loop {
                let rows = read(&*fx.store, |r| {
                    r.dreaming()?.dream_runs(
                        &KnowledgeScope::Project(ProjectId(id("project-a"))),
                        None,
                        5,
                    )
                });
                let row = &rows[0];
                if row.phase.ended() {
                    assert_eq!(row.phase, DreamPhase::Completed, "{row:?}");
                    assert_eq!(row.report.as_ref().unwrap()["inputTokens"], "60");
                    let receipts = read(&*fx.store, |r| {
                        r.dreaming()?.dream_actions(&row.id, None, 10)
                    });
                    assert_eq!(receipts.len(), 2);
                    assert_eq!(receipts[0].action["tool"], "dream_read_prior");
                    assert_eq!(receipts[1].action["tool"], "dream_work_trail");
                    assert!(
                        !serde_json::to_string(&receipts[1].action)
                            .unwrap()
                            .contains("foreign-dream-review")
                    );
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert_eq!(wire.0.load(Ordering::SeqCst), 3);
        app.shutdown().await;
    });
}

struct ActionWire {
    round: AtomicUsize,
    target: Mutex<Option<(String, String)>>,
}
impl StreamingHttp for ActionWire {
    fn open(
        &self,
        request: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        let body: Value = serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
        let round = self.round.fetch_add(1, Ordering::SeqCst);
        if round > 0 {
            let result = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .find(|row| row["role"] == "tool")
                .unwrap();
            let receipt: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
            assert!(receipt["result"].get("error").is_none(), "{receipt}");
            if round == 2 {
                assert_eq!(receipt["result"]["changed"], true, "{receipt}");
            }
        }
        let target = self.target.lock().unwrap();
        let (claim, entity) = target.as_ref().unwrap();
        let (delta, stop) = match round {
            0 => {
                let args = json!({"id":claim,"detail":"full"}).to_string();
                (
                    json!({"tool_calls":[{"index":0,"id":"read-existing","type":"function","function":{"name":"dream_read","arguments":args}}]}),
                    "tool_calls",
                )
            }
            1 => {
                let calls = [("uses", "The validation runs on the test host."), ("has", "The fixture has a replayable test case.")]
                    .into_iter().enumerate().map(|(index, (predicate, text))| {
                    let args = json!({"entry":{"kind":"Fact","entryKind":"Fact","subjectId":entity,
                        "predicate":predicate,"confidence":0.7,"tags":[],"evidenceIds":[],"value":text},
                        "prevalence":"One scoped existing entry", "evidence":[{"kind":"entry","reference":claim,"summary":"Existing fixture fact"}]});
                    json!({"index":index,"id":format!("record-fact-{index}"),"type":"function",
                        "function":{"name":"dream_record","arguments":args.to_string()}})
                }).collect::<Vec<_>>();
                (json!({"tool_calls":calls}), "tool_calls")
            }
            2 => {
                let args = json!({"action":"retract","target":claim,"expected_version":1,
                    "rationale":"The original fixture description needs manual review.",
                    "prevalence":"One reviewed fixture", "evidence":[{"kind":"entry","id":claim}]})
                .to_string();
                (
                    json!({"tool_calls":[{"index":0,"id":"propose-retract","type":"function","function":{"name":"dream_propose","arguments":args}}]}),
                    "tool_calls",
                )
            }
            3 => (
                json!({"content":"Recorded one scoped observation tied to the fixture evidence."}),
                "stop",
            ),
            _ => panic!("unexpected Dreaming action round {round}"),
        };
        let reply = json!({"choices":[{"index":0,"delta":delta,"finish_reason":stop}],"usage":{"prompt_tokens":20,"completion_tokens":4}});
        Ok(Box::new(AgentStream {
            head: ResponseHead {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
            },
            body: Some(format!("data: {reply}\n\ndata: [DONE]\n\n").into_bytes()),
        }))
    }
}

#[test]
fn direct_consolidation_records_exact_citation_and_human_revert_keeps_the_audit() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    seed_local_route(&fx);
    let wire = Arc::new(ActionWire {
        round: AtomicUsize::new(0),
        target: Mutex::new(None),
    });
    let mut ports = workspace_ports(&fx, wire.clone());
    ports.config.providers = ports.config.providers.take().map(|mut providers| {
        providers.usage_semantics.insert(
            "watch-model".into(),
            devworks_core::services::usage::meter::InputSemantics::Inclusive,
        );
        providers.with_schema_validator(Arc::new(devworks_adapters::packages::JsonSchemaValidator))
    });
    let scope = KnowledgeScope::Project(ProjectId(id("project-a")));
    fx.tx({
        let scope = scope.clone();
        move |_, tx| {
            let policy = DreamPolicy {
                model_id: Some(id("watch-model")),
                ..Default::default()
            };
            tx.dreaming()?.set_dream_policy(&scope, 1, &policy)?;
            Ok(())
        }
    });
    runtime().block_on(async {
        let app = boot(ports).await.unwrap();
        let seed = json!({"mode":"Command","protocol":PROTOCOL,"schemaRevision":2,"requestKey":fx.key(),
            "body":{"kind":"knowledge.record","scope":{"kind":"Project","projectId":"project-a"},"batch":{
                "entities":[{"clientId":"fixture-host","label":"Fixture host","entityType":"Machine","aliases":[]}],
                "claims":[{"kind":"Fact","entryKind":"Fact","subjectId":"fixture-host","predicate":"supports","confidence":0.9,
                    "evidenceIds":[],"tags":[],"value":"The test host runs the validation fixture."}],"evidence":[]}}});
        let seed = applied(&app.command(&fx.user, "knowledge.record", seed.to_string().as_bytes()).unwrap());
        *wire.target.lock().unwrap() = Some((seed["claimIds"][0].as_str().unwrap().into(), seed["entityIds"][0].as_str().unwrap().into()));
        let admitted = serde_json::to_value(run(&app, &fx, "project-a", 2)).unwrap();
        assert!(admitted.to_string().contains("Accepted"), "{admitted}");
        let completed = within("direct knowledge consolidation", async {
            loop {
                let rows = read(&*fx.store, |r| r.dreaming()?.dream_runs(&scope, None, 5));
                let row = &rows[0];
                if row.phase.ended() {
                    assert_eq!(row.phase, DreamPhase::Completed, "{row:?}");
                    break row.clone();
                }
                tokio::task::yield_now().await;
            }
        }).await;
        let actions = read(&*fx.store, |r| r.dreaming()?.dream_actions(&completed.id, None, 10));
        let action = actions.iter().find(|row| row.action["arguments"]["entry"]["predicate"] == "uses").unwrap();
        let edited_action = actions.iter().find(|row| row.action["arguments"]["entry"]["predicate"] == "has").unwrap();
        let edited_id = id(edited_action.action["result"]["receipt"]["claimIds"][0].as_str().unwrap());
        let proposal_action = actions.iter().find(|row| row.action["tool"] == "dream_propose").unwrap();
        let proposal = &proposal_action.action["result"]["proposal"];
        assert_eq!(proposal["state"], "Pending");
        assert_eq!(completed.report.as_ref().unwrap()["proposalIds"][0], proposal["id"]);
        let mut review = json!({"mode":"Command","protocol":PROTOCOL,"schemaRevision":9,"requestKey":fx.key(),
            "body":{"kind":"dreaming.review","proposalId":proposal["id"],"expectedRevision":proposal["revision"],
                "decision":"Approve","rationale":"Approve the scoped fixture retraction."}});
        let unauthorized = app.command(&fx.user, "dreaming.review", review.to_string().as_bytes()).unwrap();
        assert!(serde_json::to_value(unauthorized).unwrap().to_string().contains("Refused"));
        let mut body = review["body"].clone();
        body.as_object_mut().unwrap().remove("kind");
        let input: devworks_core::operations::dreaming::ReviewInput = serde_json::from_value(body).unwrap();
        let confirmed = devworks_core::context::HostIngress::new(fx.store.as_ref()).dreaming_review_context(&fx.user, &input).unwrap();
        review["requestKey"] = json!(fx.key());
        let approved = applied(&app.command(&confirmed, "dreaming.review", review.to_string().as_bytes()).unwrap());
        assert_eq!(approved["state"], "Approved");
        let target_id = id(proposal["targetId"].as_str().unwrap());
        let target = read(&*fx.store, |r| r.knowledge().knowledge_claim(&target_id)).unwrap();
        assert_eq!(target.lifecycle, "Retracted");
        let claim = id(action.action["result"]["receipt"]["claimIds"][0].as_str().unwrap());
        let saved = read(&*fx.store, |r| r.knowledge().knowledge_claim(&claim)).unwrap();
        assert_eq!(saved.lifecycle, "Active");
        assert_eq!(saved.body.evidence_ids.len(), 1);
        let edit = json!({"mode":"Command","protocol":PROTOCOL,"schemaRevision":2,"requestKey":fx.key(),
            "body":{"kind":"knowledge.tag","scope":{"kind":"Project","projectId":"project-a"},
                "claimIds":[edited_id],"expectedRevisions":[{"id":edited_id,"revision":"1"}],"tags":["human-reviewed"]}});
        applied(&app.command(&fx.user, "knowledge.tag", edit.to_string().as_bytes()).unwrap());
        let edited = read(&*fx.store, |r| r.knowledge().knowledge_claim(&edited_id)).unwrap();
        assert_eq!(edited.revision, 2);
        let report = json!({"mode":"Query","protocol":PROTOCOL,"schemaRevision":9,"queryId":"read-revert-witness",
            "body":{"kind":"dreaming.report","runId":completed.id}});
        let report = serde_json::to_value(app.query(&fx.user, "dreaming.report", report.to_string().as_bytes()).unwrap()).unwrap();
        assert!(report["result"]["value"]["report"].is_object(), "{report}");
        let digest = &report["result"]["value"]["detail"]["journalDigest"];
        let undo = json!({"mode":"Command","protocol":PROTOCOL,"schemaRevision":9,"requestKey":fx.key(),
            "body":{"kind":"dreaming.revert","runId":completed.id,"expectedRevision":completed.revision.to_string(),
                "expectedChangeDigest":digest,"allChanges":true}});
        let undo = applied(&app.command(&fx.user, "dreaming.revert", undo.to_string().as_bytes()).unwrap());
        assert_eq!(undo["applied"], json!([action.id]));
        assert_eq!(undo["conflicts"][0]["targetId"], json!(edited_id));
        assert_eq!(undo["remaining"], "1");
        assert_eq!(read(&*fx.store, |r| r.knowledge().knowledge_claim(&edited_id)), Some(edited));
        let after = read(&*fx.store, |r| r.knowledge().knowledge_claim(&claim)).unwrap();
        assert_eq!(after.lifecycle, "Retracted");
        assert_eq!(after.body.evidence_ids, saved.body.evidence_ids);
        assert_eq!(after.revision, saved.revision + 1);
        assert_eq!(wire.round.load(Ordering::SeqCst), 4);
        app.shutdown().await;
    });
}
