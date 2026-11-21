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
