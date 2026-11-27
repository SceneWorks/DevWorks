//! Drive native Dreaming admission from the gateway and observe the SQLite owner and transport.
use super::workspace_reply::workspace_ports;
use super::*;
use devworks_core::ports::storage::DreamRunRow;

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
    let frame = json!({"mode":"Command","protocol":PROTOCOL,"schemaRevision":24,"requestKey":fx.key(),
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
        if round > 0 {
            assert!(String::from_utf8_lossy(raw).contains("Use the revised sweep instruction."));
        }
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
fn scheduled_and_manual_resume_keep_scoped_receipts_and_get_fresh_budgets() {
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
            max_turns: 1,
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
    let scope = KnowledgeScope::Project(ProjectId(id("project-a")));
    runtime().block_on(async {
        let app = boot(ports).await.unwrap();
        let result = serde_json::to_value(run(&app, &fx, "project-a", 2)).unwrap();
        assert!(result.to_string().contains("Accepted"), "{result}");
        let first = settled(&fx, &scope, 1).await;
        assert_eq!(first.phase, DreamPhase::BudgetExhausted, "{first:?}");
        assert_eq!(first.report.as_ref().unwrap()["inputTokens"], "20");
        let artifacts = app.artifacts().clone();
        let staged = artifacts.stage_host_bytes(fx.store.as_ref(), b"Use the revised sweep instruction.",
            json!({"fixture":"dreaming continuation instruction"})).unwrap().unwrap();
        let new_text = write(fx.store.as_ref(), {
            let artifacts = artifacts.clone();
            let staged = staged.clone();
            move |tx| {
                let version = artifacts.commit_host_bytes(tx, &staged, "Revised Dreaming instruction",
                    "text/plain", json!({"fixture":"dreaming continuation instruction"}))?;
                Ok(devworks_core::services::artifacts::version_ref(&version))
            }
        }).unwrap();
        artifacts.settle_host_bytes(fx.store.as_ref(), &staged, true);
        let save = json!({"mode":"Command","protocol":PROTOCOL,"schemaRevision":24,"requestKey":fx.key(),
            "body":{"kind":"dreaming.instructions","scope":{"kind":"Workspace"},"expectedRevision":"1",
                    "content":new_text}});
        applied(&app.command(&fx.user, "dreaming.instructions", save.to_string().as_bytes()).unwrap());
        fx.h.clock.advance(1000);
        let job = read(&*fx.store, |r| r.jobs().job(&id("dream-projects"))).unwrap();
        let nightly = json!({"mode":"Command","protocol":PROTOCOL,"schemaRevision":1,"requestKey":fx.key(),
            "body":{"kind":"jobs.run_now","jobId":job.id,"expectedRevision":job.revision.to_string()}});
        applied(&app.command(&fx.user, "jobs.run_now", nightly.to_string().as_bytes()).unwrap());
        let second = settled(&fx, &scope, 2).await;
        assert_eq!(second.cutoffs.base_instruction_revision, 2);
        assert_eq!(second.sweep_started_at_ms(), first.started_at_ms);
        assert!(second.started_at_ms > first.started_at_ms);
        assert_eq!(second.continued_from.as_ref(), Some(&first.id));
        assert_eq!(second.phase, DreamPhase::BudgetExhausted, "{second:?}");
        assert_eq!(second.report.as_ref().unwrap()["inputTokens"], "20");
        let result = serde_json::to_value(run(&app, &fx, "project-a", 2)).unwrap();
        assert!(result.to_string().contains("Accepted"), "{result}");
        let third = settled(&fx, &scope, 3).await;
        assert_eq!(third.continued_from.as_ref(), Some(&second.id));
        assert_eq!(third.phase, DreamPhase::Completed, "{third:?}");
        assert_eq!(third.report.as_ref().unwrap()["inputTokens"], "20");
        let receipts = |row: &DreamRunRow| read(&*fx.store, |r| r.dreaming()?.dream_actions(&row.id, None, 10));
        let first_actions = receipts(&first);
        let second_actions = receipts(&second);
        assert_eq!(first_actions.len(), 1);
        assert_eq!(first_actions[0].action["tool"], "dream_work_trail");
        assert!(!first_actions[0].action.to_string().contains("foreign-dream-review"));
        assert_eq!(second_actions.len(), 1);
        assert_eq!(second_actions[0].action["tool"], "dream_read_prior");
        assert!(receipts(&third).is_empty());
        let stale = json!({"mode":"Command","protocol":PROTOCOL,"schemaRevision":24,"requestKey":fx.key(),
            "body":{"kind":"dreaming.run","scope":{"kind":"Project","projectId":"project-a"},
                "expectedSettingsRevision":"2","resumeRunId":first.id}});
        let stale = app.command(&fx.user, "dreaming.run", stale.to_string().as_bytes()).unwrap();
        assert!(serde_json::to_value(stale).unwrap().to_string().contains("dreaming_resume_consumed"));
        assert_eq!(wire.0.load(Ordering::SeqCst), 3);
        app.shutdown().await;
    });
}

async fn settled(fx: &Fx, scope: &KnowledgeScope, attempts: usize) -> DreamRunRow {
    within("native Dreaming settlement", async {
        let start = std::time::Instant::now();
        loop {
            let rows = read(&*fx.store, |r| r.dreaming()?.dream_runs(scope, None, 10));
            if rows.len() >= attempts && rows.iter().all(|row| row.phase.ended()) {
                let tip = rows
                    .iter()
                    .find(|row| {
                        !rows
                            .iter()
                            .any(|child| child.continued_from.as_ref() == Some(&row.id))
                    })
                    .expect("each scoped continuation has a terminal leaf");
                break tip.clone();
            }
            if start.elapsed() > Duration::from_secs(8) {
                let journals = rows
                    .iter()
                    .map(|row| {
                        let operation = row
                            .operation_id
                            .as_ref()
                            .and_then(|id| read(&*fx.store, |r| r.operations().operation(id)));
                        let queue = read(&*fx.store, |r| r.queue().entry(&row.id));
                        (
                            row.id.clone(),
                            row.phase,
                            row.continued_from.clone(),
                            row.checkpoint
                                .as_ref()
                                .and_then(|cp| cp.request.as_ref())
                                .map(|request| request.stage.clone()),
                            operation.map(|op| (op.state, op.attempts.len())),
                            queue.map(|q| (q.state, q.cancel_requested)),
                        )
                    })
                    .collect::<Vec<_>>();
                panic!("wanted {attempts} terminal attempts, observed {journals:?}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
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
            if round != 3 {
                assert!(receipt["result"].get("error").is_none(), "{receipt}");
            }
            if round == 2 {
                assert_eq!(receipt["result"]["changed"], true, "{receipt}");
            }
        }
        let new_claim = (round == 2).then(|| {
            let messages = body["messages"].as_array().unwrap();
            messages
                .iter()
                .rev()
                .filter(|row| row["role"] == "tool")
                .map(|row| serde_json::from_str::<Value>(row["content"].as_str().unwrap()).unwrap())
                .find_map(|row| {
                    row["result"]["receipt"]["claimIds"][0]
                        .as_str()
                        .map(str::to_owned)
                })
                .expect("the first direct write returned an exact claim")
        });
        let target = self.target.lock().unwrap();
        let (claim, entity) = target.as_ref().unwrap();
        let (delta, stop) = match round {
            0 => {
                let hash = devworks_core::fingerprint::sha256_hex(
                    br#"{"review":"Qualified fixture review"}"#,
                );
                (
                    json!({"tool_calls":[
                        {"index":0,"id":"read-existing","type":"function",
                         "function":{"name":"dream_read","arguments":json!({"id":claim,"detail":"full"}).to_string()}},
                        {"index":1,"id":"read-review","type":"function",
                         "function":{"name":"dream_work_trail","arguments":json!({"kind":"review",
                             "id":"proposal-review-witness","expected_hash":hash}).to_string()}}
                    ]}),
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
                let mut primary = json!({"action":"retract","target":claim,"expected_version":1,
                    "rationale":"The original fixture description needs manual review.",
                    "prevalence":"One reviewed fixture","evidence":[
                        {"kind":"entry","id":claim},{"kind":"review","id":"proposal-review-witness"}]});
                let qualified = primary.to_string();
                primary["evidence"] = json!([{"kind":"entry","id":claim}]);
                let second = primary.to_string();
                primary["action"] = json!("hook_draft");
                primary["draft"] = json!("A fact alone cannot justify a hook.");
                (
                    json!({"tool_calls":[
                        {"index":0,"id":"propose-retract","type":"function","function":{"name":"dream_propose","arguments":qualified}},
                        {"index":1,"id":"propose-duplicate","type":"function","function":{"name":"dream_propose","arguments":second}},
                        {"index":2,"id":"propose-nonrule-hook","type":"function","function":{"name":"dream_propose","arguments":primary.to_string()}},
                        {"index":3,"id":"read-created","type":"function","function":{"name":"dream_read",
                            "arguments":json!({"id":new_claim.unwrap(),"detail":"full"}).to_string()}}
                    ]}),
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
            tx.history().append(NewEvent {
                id: devworks_core::protocol::EventId(id("proposal-review-witness")),
                actor: ActorId(id("user-a")),
                project_id: scope.project().cloned(),
                root_id: None,
                operation_id: None,
                aggregate_kind: "feature".into(),
                aggregate_id: "root-f".into(),
                aggregate_revision: 1,
                kind: "PlanReviewSettled".into(),
                payload: json!({"review":"Qualified fixture review"}),
            })?;

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
        assert!(actions.iter().any(|action| action.action["tool"] == "dream_propose"
            && action.action["result"]["error"] == "DreamingProposalPolicyRefused"),
            "native tool receipts: {:?}", actions.iter().map(|row| (
                &row.action["tool"], &row.action["result"]["error"]
            )).collect::<Vec<_>>());
        assert!(actions.iter().any(|action| action.action["tool"] == "dream_read"
            && action.action["arguments"]["id"] != seed["claimIds"][0]
            && action.action["result"]["proofs"][0]["revision"] == 1));
        let staged = actions.iter().filter(|row| row.action["tool"] == "dream_propose"
            && row.action["result"]["proposalId"].is_string()).collect::<Vec<_>>();
        assert_eq!(staged.len(), 2);
        let proposal = &staged[0].action["result"]["proposal"];
        assert_eq!(staged[0].action["result"]["proposalId"], staged[1].action["result"]["proposalId"]);
        let approval_id = id(proposal["id"].as_str().unwrap());
        let approval = read(&*fx.store, |r| r.decisions().approval(&approval_id)).unwrap();
        assert!(approval.payload["reviewed"]["evidence"].as_array().unwrap().iter()
            .any(|evidence| evidence["kind"] == "Review" && evidence["reference"] == "proposal-review-witness"));
        for revision in [1, 24] {
            let stage = json!({"mode":"Command","protocol":PROTOCOL,"schemaRevision":revision,"requestKey":fx.key(),
                "body":{"kind":"dreaming.propose","runId":completed.id,"expectedRevision":completed.revision.to_string(),
                    "claimIds":[{"id":proposal["targetId"],"revision":proposal["targetRevision"]}],"content":proposal["artifact"]}});
            let refused = app.command(&fx.user, "dreaming.propose", stage.to_string().as_bytes()).unwrap();
            let refused = serde_json::to_value(refused).unwrap();
            assert!(refused.to_string().contains("PermissionDenied"), "{refused}");
        }
        assert_eq!(proposal["state"], "Pending");
        assert_eq!(completed.report.as_ref().unwrap()["proposalIds"][0], proposal["id"]);
        let mut review = json!({"mode":"Command","protocol":PROTOCOL,"schemaRevision":24,"requestKey":fx.key(),
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
        let report = json!({"mode":"Query","protocol":PROTOCOL,"schemaRevision":24,"queryId":"read-revert-witness",
            "body":{"kind":"dreaming.report","runId":completed.id}});
        let report = serde_json::to_value(app.query(&fx.user, "dreaming.report", report.to_string().as_bytes()).unwrap()).unwrap();
        assert!(report["result"]["value"]["report"].is_object(), "{report}");
        let digest = &report["result"]["value"]["detail"]["journalDigest"];
        let undo = json!({"mode":"Command","protocol":PROTOCOL,"schemaRevision":24,"requestKey":fx.key(),
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

struct ControlledWire {
    calls: AtomicUsize,
    entered: tokio::sync::Notify,
    released: (Mutex<bool>, std::sync::Condvar),
    status: u16,
}

impl ControlledWire {
    fn release(&self) {
        *self.released.0.lock().unwrap() = true;
        self.released.1.notify_all();
    }
}

impl StreamingHttp for ControlledWire {
    fn open(
        &self,
        _: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        let (wait, wake) = &self.released;
        let (ready, _) = wake
            .wait_timeout_while(wait.lock().unwrap(), Duration::from_secs(15), |ready| {
                !*ready
            })
            .unwrap();
        assert!(*ready, "test must settle the original physical send");
        let (content_type, body) = if self.status == 200 {
            ("text/event-stream", b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"No changes warranted.\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":4}}\n\ndata: [DONE]\n\n".as_slice())
        } else {
            ("application/json", b"{\"error\":{\"message\":\"fixture request rejected\",\"type\":\"invalid_request_error\"}}".as_slice())
        };
        Ok(Box::new(AgentStream {
            head: ResponseHead {
                status: self.status,
                headers: vec![("content-type".into(), content_type.into())],
            },
            body: Some(body.to_vec()),
        }))
    }
}

fn dream_ports(fx: &Fx, wire: Arc<dyn StreamingHttp>) -> Composition {
    let mut ports = workspace_ports(fx, wire);
    ports.config.providers = ports.config.providers.take().map(|mut providers| {
        providers.usage_semantics.insert(
            "watch-model".into(),
            devworks_core::services::usage::meter::InputSemantics::Inclusive,
        );
        providers.with_schema_validator(Arc::new(devworks_adapters::packages::JsonSchemaValidator))
    });
    ports
}

#[test]
fn settlement_releases_the_scope_after_provider_failure_settings_change_or_cancel() {
    let _native_case = native_runtime_case();
    runtime().block_on(async {
        for (status, change_settings, cancel, phase) in [
            (400, false, false, DreamPhase::Failed),
            (200, true, false, DreamPhase::Blocked),
            (400, false, true, DreamPhase::OutcomeUnknown),
        ] {
            let fx = Fx::new();
            let scope = KnowledgeScope::Project(ProjectId(id("project-a")));
            seed_local_route(&fx);
            fx.tx(|_, tx| {
                let scope = KnowledgeScope::Project(ProjectId(id("project-a")));
                let policy = DreamPolicy {
                    model_id: Some(id("watch-model")),
                    ..Default::default()
                };
                tx.dreaming()?.set_dream_policy(&scope, 1, &policy)?;
                tx.history().append(NewEvent {
                    id: devworks_core::protocol::EventId(id("on-the-wire-review")),
                    actor: ActorId(id("user-a")),
                    project_id: scope.project().cloned(),
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
            let wire = Arc::new(ControlledWire {
                calls: AtomicUsize::new(0),
                entered: tokio::sync::Notify::new(),
                released: (Mutex::new(false), std::sync::Condvar::new()),
                status,
            });
            let app = boot(dream_ports(&fx, wire.clone())).await.unwrap();
            assert!(serde_json::to_value(run(&app, &fx, "project-a", 2)).unwrap().to_string().contains("Accepted"));
            within("in-flight Dreaming provider", wire.entered.notified()).await;
            if change_settings {
                fx.tx(|_, tx| {
                    let scope = KnowledgeScope::Project(ProjectId(id("project-a")));
                    tx.dreaming()?.set_dream_policy(&scope, 2, &DreamPolicy {
                        enabled: false,
                        ..Default::default()
                    })?;
                    Ok(())
                });
            }
            if cancel {
                let live = read(&*fx.store, |r| r.dreaming()?.dream_live_run(&scope)).unwrap();
                let request = json!({"mode":"Command","protocol":PROTOCOL,"schemaRevision":24,"requestKey":fx.key(),
                    "body":{"kind":"dreaming.run","scope":{"kind":"Project","projectId":"project-a"},
                        "expectedSettingsRevision":"2","cancelRunId":live.id}});
                applied(&app.command(&fx.user, "dreaming.run", request.to_string().as_bytes()).unwrap());
            }
            wire.release();
            let ended = settled(&fx, &scope, 1).await;
            assert_eq!(ended.phase, phase, "{ended:?}");
            if change_settings {
                assert_eq!(ended.report.as_ref().unwrap()["inputTokens"], "20");
            }
            let checkpoint = ended.checkpoint.as_ref().unwrap();
            let request = checkpoint.request.as_ref().unwrap();
            let stage = if cancel {
                devworks_core::ports::storage::DreamRequestStage::PossiblySent
            } else {
                devworks_core::ports::storage::DreamRequestStage::Observed
            };
            assert_eq!(request.stage, stage);
            assert_eq!(read(&*fx.store, |r| r.dreaming()?.dream_live_run(&scope)).is_some(), cancel);
            assert_eq!(wire.calls.load(Ordering::SeqCst), 1);
            app.shutdown().await;
            drop(app);
            let restarted = boot(dream_ports(&fx, wire.clone())).await.unwrap();
            assert_eq!(read(&*fx.store, |r| r.dreaming()?.dream_live_run(&scope)).is_some(), cancel);
            assert_eq!(read(&*fx.store, |r| r.dreaming()?.dream_run(&ended.id)).unwrap().phase, phase);
            assert_eq!(wire.calls.load(Ordering::SeqCst), 1);
            restarted.shutdown().await;
        }
    });
}

#[derive(Default)]
struct SourceScript {
    repository: String,
    bytes: Vec<u8>,
    previous_page: String,
    fragment: String,
    pages: usize,
}

struct SourceWire {
    script: Mutex<SourceScript>,
}

impl StreamingHttp for SourceWire {
    fn open(
        &self,
        request: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        let body: Value = serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
        let mut script = self.script.lock().unwrap();
        let prior = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|row| row["role"] == "tool")
            .map(|message| {
                serde_json::from_str::<Value>(message["content"].as_str().unwrap()).unwrap()
            });
        let (delta, finish) = if let Some(reply) = prior {
            assert!(reply["result"].get("error").is_none(), "{reply}");
            let result = if let Some(fragment) = reply["content"]
                .as_str()
                .or_else(|| reply["result"]["content"].as_str())
            {
                script.fragment.push_str(fragment);
                let offset = reply["next_offset_chars"]
                    .as_u64()
                    .or_else(|| reply["result"]["next_offset_chars"].as_u64());
                if let Some(offset) = offset {
                    let (action, hash) = if reply["content"].is_string() {
                        let action = reply["action_index"].to_string();
                        let hash = reply["expected_hash"].as_str().unwrap().to_owned();
                        script.previous_page = format!("{action}:{hash}");
                        (action, hash)
                    } else {
                        let (action, hash) = script.previous_page.split_once(':').unwrap();
                        (action.to_owned(), hash.to_owned())
                    };
                    return Ok(source_stream(
                        json!({"tool_calls":[source_call(
                        "dream_read_prior", &json!({"action_index":action.parse::<u64>().unwrap(),
                        "offset_chars":offset,"expected_hash":hash}), &body)]}),
                        "tool_calls",
                    ));
                }
                serde_json::from_str::<Value>(&std::mem::take(&mut script.fragment)).unwrap()
            } else {
                reply["result"].clone()
            };
            let source = &result["source"];
            let start = source["startByte"]
                .as_str()
                .unwrap()
                .parse::<usize>()
                .unwrap();
            assert_eq!(start, script.bytes.len());
            script
                .bytes
                .extend_from_slice(source["text"].as_str().unwrap().as_bytes());
            script.pages += 1;
            if let Some(cursor) = result["sourceNextCursor"].as_str() {
                assert_eq!(result["verifiedIndexMatch"], false);
                assert_eq!(result["proofs"], json!([]));
                let call = source_call("dream_read_source", &json!({"cursor":cursor}), &body);
                (json!({"tool_calls":[call]}), "tool_calls")
            } else {
                assert!(script.pages > 1);
                assert_eq!(result["verifiedIndexMatch"], true, "{result}");
                assert_eq!(result["proofs"].as_array().unwrap().len(), 1);
                (
                    json!({"content":"The entire captured source was read."}),
                    "stop",
                )
            }
        } else {
            let call = source_call(
                "dream_read_source",
                &json!({
                    "repository":script.repository,"path":"guide.md","detail":"full"
                }),
                &body,
            );
            (json!({"tool_calls":[call]}), "tool_calls")
        };
        Ok(source_stream(delta, finish))
    }
}

fn source_call(tool: &str, args: &Value, request: &Value) -> Value {
    let round = request["messages"].as_array().unwrap().len();
    json!({"index":0,"id":format!("source-{round}"),"type":"function",
        "function":{"name":tool,"arguments":args.to_string()}})
}

fn source_stream(delta: Value, finish: &str) -> Box<dyn HttpStream> {
    let reply = json!({"choices":[{"index":0,"delta":delta,"finish_reason":finish}],
        "usage":{"prompt_tokens":20,"completion_tokens":4}});
    Box::new(AgentStream {
        head: ResponseHead {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
        },
        body: Some(format!("data: {reply}\n\ndata: [DONE]\n\n").into_bytes()),
    })
}

#[test]
fn source_proof_requires_the_final_native_page_of_the_selected_capture() {
    use devworks_adapters::index::{NativeSourceTree, treesitter::TreeSitterParser};
    use devworks_core::protocol::results::OperationResult;
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    seed_local_route(&fx);
    let checkout = devworks_test_support::TempRoot::under(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")),
        "dreaming-source-test",
    );
    std::fs::create_dir_all(checkout.path()).unwrap();
    let parent = checkout.path().canonicalize().unwrap();
    let repo = parent.join("source-proof");
    std::fs::create_dir_all(&repo).unwrap();
    let large = format!(
        "{}Final evidence 世界.\n",
        "Tracked project evidence.\n".repeat(2700)
    );
    std::fs::write(repo.join("guide.md"), &large).unwrap();
    for args in [
        &["init", "-q"][..],
        &["add", "guide.md"],
        &["commit", "-q", "-m", "captured source"],
    ] {
        let result = std::process::Command::new("git")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
    }
    let wire = Arc::new(SourceWire {
        script: Mutex::new(SourceScript::default()),
    });
    let mut ports = dream_ports(&fx, wire.clone());
    ports.process_host = Arc::new(devworks_adapters::process::NativeProcessHost);
    ports.indexing = Some(devworks_engine::IndexComposition {
        parser: Arc::new(TreeSitterParser::new()),
        source: Arc::new(NativeSourceTree::new()),
        fs: Arc::new(NativeFilesystem::new()),
        settings: Default::default(),
        config: devworks_engine::IndexingConfig {
            max_runs: 1,
            tick: Duration::from_millis(50),
            housekeeping_interval: Duration::from_secs(3600),
            discovery_interval: Duration::from_secs(3600),
            discover_at_boot: false,
        },
        enrichers: Vec::new(),
    });
    let project = ProjectId(id("project-a"));
    let scope = KnowledgeScope::Project(project.clone());
    let settings_actor = fx.user.clone();
    fx.tx(move |_, tx| {
        let descriptor =
            devworks_core::settings::catalog::descriptor("repositories.baseDirectory").unwrap();
        let setting = serde_json::from_value(json!({"key":"repositories.baseDirectory",
            "value":{"schema":descriptor.schema_ref(),"value":parent.display().to_string()}}))
        .unwrap();
        devworks_core::settings::service::apply_human_assignments(
            tx,
            &settings_actor,
            &ConfigScope::Workspace,
            &[setting],
            json!({}),
        )
        .map_err(|failure| StorageError::InvalidRecord(format!("{failure:?}")))?;
        let scope = KnowledgeScope::Project(ProjectId(id("project-a")));
        tx.dreaming()?.set_dream_policy(
            &scope,
            1,
            &DreamPolicy {
                model_id: Some(id("watch-model")),
                ..Default::default()
            },
        )?;
        Ok(())
    });
    runtime().block_on(async {
        let app = boot(ports).await.unwrap();
        let discovery = json!({"mode":"Query","protocol":PROTOCOL,"queryId":"discover-scoped-source",
            "body":{"kind":"indexing.discover","scope":{"kind":"Workspace"},"roots":[],"page":{"limit":50}}});
        let reply = app.query(&fx.user, "indexing.discover", discovery.to_string().as_bytes()).unwrap();
        let DomainQueryReply::Result { result: OperationResult::IndexingDiscover { value }, .. } = reply
            else { panic!("native source discovery: {reply:?}") };
        let repository = value.items.0.into_iter().find(|repository| repository.primary)
            .expect("native source repository").id;
        wire.script.lock().unwrap().repository = repository.as_str().to_owned();
        fx.tx({
            let project = project.clone();
            let repository = repository.clone();
            move |_, tx| tx.projects().set_members(&project, &[repository])
        });
        let frame = json!({"mode":"Command","protocol":PROTOCOL,"requestKey":fx.key(),
            "body":{"kind":"indexing.index","scope":{"kind":"Workspace"},"repositoryIds":[repository],
                "expectedSourceRevision":"1","views":{"kind":"Published","repositoryIds":[repository]}}});
        applied(&app.command(&fx.user, "indexing.index", frame.to_string().as_bytes()).unwrap());
        within("native source publication", async {
            while read(&*fx.store, |snapshot| snapshot.indexing().published(&repository)).is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await;
        assert!(read(&*fx.store, |snapshot| snapshot.dreaming()?.dream_cutoffs(&scope)).sources > 0);
        let admitted = serde_json::to_value(run(&app, &fx, "project-a", 2)).unwrap();
        assert!(admitted.to_string().contains("Accepted"), "{admitted}");
        let ended = settled(&fx, &scope, 1).await;
        assert_eq!(ended.phase, DreamPhase::Completed, "{ended:?}");
        assert_eq!(wire.script.lock().unwrap().bytes, large.as_bytes());
        app.shutdown().await;
    });
}
