//! Readiness review must run while the only ordinary worker slot is occupied.
use super::*;
use devworks_core::operations::agents::DelegateInput;
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::AgentRecord;
use devworks_core::protocol::support::{BoundedList, BoundedText};
use devworks_core::services::artifacts::evidence::Evidence;
use devworks_core::services::work::{
    model::{ActivePlan, plan_digest, plan_units},
    plan_review::{self, PlanReview, ReviewOutcome},
    readiness::Assessment,
};
use devworks_engine::dispatch::RootEligibility;
use tokio::sync::{mpsc, watch};

struct ParkedTurns {
    entered: mpsc::UnboundedSender<(InvocationContext, AgentRecord)>,
    finish: watch::Receiver<bool>,
}

impl AgentTurnExecutor for ParkedTurns {
    fn run(
        &self,
        context: InvocationContext,
        agent: AgentRecord,
        mut stop: watch::Receiver<bool>,
    ) -> LaunchFuture {
        let mut finish = self.finish.clone();
        let sent = self.entered.send((context, agent));
        Box::pin(async move {
            assert!(sent.is_ok());
            let outcome = tokio::select! {
                _ = finish.wait_for(|done| *done) => ActivationOutcome::Completed,
                _ = stop.wait_for(|stopped| *stopped) => ActivationOutcome::Cancelled,
            };
            LaunchEnd::Finished {
                outcome,
                detail: None,
            }
        })
    }
}

fn seed_reviewed_plan(fx: &Fx, app: &devworks_engine::Runtime) -> ArtifactRef {
    let document = app
        .artifacts()
        .publish_evidence(
            &*fx.store,
            Evidence {
                scope: ArtifactScope::Project(ProjectId(id("project-a"))),
                producer: fx.user.principal().clone(),
                principal_kind: "Human".into(),
                agent_session: None,
                root: None,
                assignment: None,
                title: "Plan for the rebuild".into(),
                media_type: "text/plain".into(),
                tags: vec![],
                source: json!({"kind": "TestPlan"}),
            },
            b"Keep the published index readable across a rebuild.",
            |_, _| Ok(()),
        )
        .unwrap()
        .0;
    let draft = serde_json::from_value(json!({
        "content": document, "repositoryIds": ["review-repository"], "acceptance": [],
        "goals": [], "acceptanceTests": [], "requirements": [],
    }))
    .unwrap();
    let digest = plan_digest(&draft);
    let units = plan_units(&draft);
    let report = draft.content.clone();
    let prior = PlanReview {
        plan_revision: 1,
        plan_digest: digest.clone(),
        assignment: AssignmentId(id("prior-review-assignment")),
        session: id("prior-review-session"),
        actor: ActorId(id("prior-reviewer")),
        started_at_ms: T0,
        activation: id("prior-activation"),
        owner_epoch: 1,
        units: units.clone(),
        affected_units: units.keys().cloned().collect(),
        source_views: vec![id("review-view-1")],
        source_phase: true,
        outcome: ReviewOutcome::Complete {
            report: report.clone(),
            findings: 0,
        },
    };
    let approved = ActivePlan {
        draft,
        digest: digest.clone(),
        plan_revision: 1,
        proposal_id: id("capacity-plan-approval"),
        review_required: true,
    };
    let assessment = Assessment {
        id: id("capacity-assessment"),
        plan_revision: 1,
        plan_digest: digest,
        reviewed_pending: false,
        units,
        summary: "Ready".into(),
        findings: vec![],
        assessed_by: fx.user.principal().clone(),
        assessed_at_ms: T0,
    };
    fx.tx(move |_, tx| {
        tx.work().put_feature_plan(
            &StoredFeaturePlan {
                feature_id: id("root-f"),
                revision: 1,
                approved: Some(json!(approved)),
                approved_by: Some(assessment.assessed_by.clone()),
                approved_at_ms: Some(T0),
                pending: None,
                pending_revision: None,
            },
            None,
        )?;
        tx.work().put_readiness(
            &StoredReadiness {
                feature_id: id("root-f"),
                revision: 1,
                body: json!({"active": assessment, "planReviews": [prior]}),
            },
            None,
        )
    });
    report
}

fn one_worker_slot(app: &devworks_engine::Runtime, fx: &Fx) {
    let key = "agents.maxSubagents";
    let schema = devworks_core::settings::catalog::descriptor(key)
        .unwrap()
        .schema_ref();
    let frame = json!({"mode": "Command", "protocol": PROTOCOL, "requestKey": fx.key(),
        "body": {"kind": "settings.save_revision", "scope": {"kind": "Workspace"},
            "expectedRevision": "1",
            "entries": [{"key": key, "value": {"schema": schema, "value": 1}}]}});
    let reply = app
        .command(
            &fx.user,
            "settings.save_revision",
            frame.to_string().as_bytes(),
        )
        .unwrap();
    assert!(
        matches!(decision(&reply), DomainDecision::Applied { .. }),
        "{reply:?}"
    );
}

#[test]
fn source_review_launches_while_the_only_worker_waits_for_its_queued_effect() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    runtime().block_on(async {
        let (entered, mut turns) = mpsc::unbounded_channel();
        let (finish, finish_rx) = watch::channel(false);
        let executor = Arc::new(ParkedTurns {
            entered,
            finish: finish_rx,
        });
        let mut composed = composition(&fx, None, Arc::new(FakePower::default()));
        composed.runs.scope = None;
        composed.application.settings = Some(devworks_engine::SettingsPorts {
            roles: Arc::new(devworks_core::authority::HostRoles {
                store: fx.store.clone(),
            }),
            paths: Arc::new(devworks_adapters::paths::FsPathProbe),
        });
        let app = boot(composed).await.unwrap();
        one_worker_slot(&app, &fx);
        super::plan_review_tests::seed_source(&fx, &app);
        let report = seed_reviewed_plan(&fx, &app);
        app.runs()
            .install_launcher(Arc::new(AgentLauncher::new(
                fx.store.clone(),
                fx.h.ids.clone(),
                app.agents().clone(),
                executor,
            )))
            .unwrap();
        applied(&fx.start(&fx.user, "root-f", 1, 1));
        let (coordinator, primary) = within("feature coordinator starts", turns.recv())
            .await
            .unwrap();
        assert_eq!(
            primary.role_id.as_str(),
            AgentRole::FeatureCoordinator.as_str()
        );

        let delegate = |role: AgentRole| {
            let agents = app.agents().clone();
            let coordinator = coordinator.clone();
            let request = DelegateInput {
                root_id: rid("root-f"),
                expected_control_revision: Revision::new(fx.control("root-f").revision).unwrap(),
                task_ids: BoundedList(vec![]),
                role_id: id(role.as_str()),
                instruction: BoundedText("Implement the feature".into()),
                source_checkpoint_id: None,
            };
            fx.tx(move |_, tx| Ok(agents.delegate(tx, &coordinator, &request)))
        };
        let worker = delegate(AgentRole::Worker).unwrap();
        let (_, worker_turn) = within(
            "the first worker occupies the only ordinary slot",
            turns.recv(),
        )
        .await
        .unwrap();
        assert_eq!(worker_turn.assignment_id, worker.assignment_id);
        let queued_worker = delegate(AgentRole::Worker).unwrap();
        assert!(
            delegate(AgentRole::PlanReviewer).is_err(),
            "only the host can reserve a review slot"
        );

        let gate = Arc::new(RunGate::new(app.runs().service().clone(), None));
        let entry = fx.tx({
            let gate = gate.clone();
            let worker = worker.assignment_id.clone();
            move |_, tx| {
                let operation = OperationId(id("worker-queued-effect"));
                tx.operations().record_intent(NewOperation {
                    id: operation.clone(),
                    root_id: Some(rid("root-f")),
                    assignment_id: Some(worker),
                    capability: "process.execute".into(),
                    schema_version: 1,
                    args_digest: "b".repeat(64),
                    payload: json!({"kind": "Agent"}),
                })?;
                let entry_id = id("worker-queued-delivery");
                let event_id = event(tx, "Changed")?;
                tx.queue().enqueue(NewQueueEntry {
                    id: entry_id.clone(),
                    consumer: devworks_core::services::process::tools::EFFECT_CONSUMER.into(),
                    event_id,
                    root_id: Some(rid("root-f")),
                    operation_id: Some(operation),
                    partition: None,
                    priority: 0,
                    ready_at_ms: T0 + 60_000,
                    payload: json!({}),
                })?;
                let entry = tx.queue().entry(&entry_id)?.unwrap();
                assert!(gate.may_dispatch(tx, &entry)?);
                tx.indexing()
                    .publish(&id("review-repository"), &id("review-view-2"), 1)?;
                assert!(!gate.may_dispatch(tx, &entry)?);
                assert!(!gate.may_resume(tx, &entry)?);
                Ok(entry)
            }
        });

        let review = fx.tx({
            let agents = app.agents().clone();
            let coordinator = coordinator.clone();
            move |_, tx| {
                let (activation, epoch) = coordinator.activation().unwrap();
                let (target, existing) =
                    plan_review::next_target(tx, &id("root-f"), activation, epoch)?.unwrap();
                assert!(existing.is_none());
                agents
                    .delegate_plan_review(tx, &coordinator, &target)
                    .map_err(|error| {
                        StorageError::InvalidRecord(format!("delegate review: {error:?}"))
                    })
            }
        });
        let (reviewer, turn) = within(
            "review starts while the queued worker effect is held",
            turns.recv(),
        )
        .await
        .unwrap();
        assert_eq!(turn.assignment_id, review.assignment_id);
        assert_eq!(turn.role_id.as_str(), AgentRole::PlanReviewer.as_str());
        assert!(
            read(&*fx.store, |s| s.runs().assignment(&worker.assignment_id))
                .unwrap()
                .slot_held
        );
        assert!(
            !read(&*fx.store, |s| s
                .runs()
                .assignment(&queued_worker.assignment_id))
            .unwrap()
            .slot_held
        );

        fx.tx(move |_, tx| {
            assert!(!gate.may_dispatch(tx, &entry)?);
            assert!(plan_review::begin_source_phase(tx, &reviewer)?);
            assert!(plan_review::settle(
                tx,
                &reviewer,
                ReviewOutcome::Complete {
                    report,
                    findings: 0,
                }
            )?);
            assert!(gate.may_dispatch(tx, &entry)?);
            assert!(gate.may_resume(tx, &entry)?);
            Ok(())
        });
        finish.send_replace(true);
        let (_, last) = within(
            "next worker starts once the ordinary slot releases",
            turns.recv(),
        )
        .await
        .unwrap();
        assert_eq!(last.assignment_id, queued_worker.assignment_id);
        within("all assignments complete", async {
            while fx.live("root-f").is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        app.shutdown().await;
    });
}

#[cfg(unix)]
mod coordinator_wait {
    use super::*;
    use devworks_core::services::process::tools::{
        RegisteredExecutable, RegisteredWorkingRoot, ToolBindings,
    };
    use devworks_engine::runtime::ProcessToolComposition;

    struct Entropy;
    impl devworks_core::ports::http::Entropy for Entropy {
        fn fill(&self, bytes: &mut [u8]) {
            bytes.fill(7);
        }
    }

    struct Wire {
        coordinator_requests: AtomicUsize,
        reviewer_requests: AtomicUsize,
        final_request: mpsc::UnboundedSender<String>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl StreamingHttp for Wire {
        fn open(
            &self,
            request: &HttpRequest,
            _: &StreamLimits,
            _: &Cancellation,
        ) -> Result<Box<dyn HttpStream>, TransportError> {
            let request: Value = serde_json::from_slice(request.body.as_ref().unwrap()).unwrap();
            let body = request.to_string();
            let (delta, reason) = if body.contains("reviewedPlanDigest") {
                self.reviewer_requests.fetch_add(1, Ordering::SeqCst);
                let report = json!({"needsSource": false, "findings": [{
                    "concern": "The plan omits interrupted rebuilds.",
                    "consequence": "The published index may become unreadable.",
                    "acceptanceCase": "Stop halfway through rebuild and confirm the previous index remains readable.",
                }]});
                (json!({"content": report.to_string()}), "stop")
            } else {
                match self.coordinator_requests.fetch_add(1, Ordering::SeqCst) {
                    0 => {
                        let name = "process_execute_command";
                        assert!(
                            request["tools"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|tool| tool["function"]["name"] == name)
                        );
                        let args = json!({"spec": {
                            "executableRef": "shell",
                            "args": ["-c", "exit 0"],
                            "workingDirectory": {"rootId": "work", "relative": "capacity-work"},
                            "environment": [], "profile": "Full",
                            "timeoutMs": "10000", "outputLimitBytes": "4096",
                        }});
                        (
                            json!({"tool_calls": [{"index": 0, "id": "queued-process", "type": "function",
                            "function": {"name": name, "arguments": args.to_string()}}]}),
                            "tool_calls",
                        )
                    }
                    1 => {
                        self.final_request.send(body).unwrap();
                        // Source changes while this provider request is in flight, after the
                        // coordinator's last readiness check and before its linked-effect wait.
                        self.release
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(30))
                            .unwrap();
                        (json!({"content": "The process has been queued."}), "stop")
                    }
                    _ => {
                        self.final_request.send(body).unwrap();
                        (
                            json!({"content": "The review finding needs a disposition."}),
                            "stop",
                        )
                    }
                }
            };
            let frame = json!({"choices": [{"index": 0, "delta": delta, "finish_reason": reason}]});
            Ok(Box::new(AgentStream {
                head: ResponseHead {
                    status: 200,
                    headers: vec![("content-type".into(), "text/event-stream".into())],
                },
                body: Some(format!("data: {frame}\n\ndata: [DONE]\n\n").into_bytes()),
            }))
        }
    }

    #[test]
    fn native_coordinator_reviews_source_drift_while_awaiting_its_own_effect() {
        let _native_case = native_runtime_case();
        let fx = Fx::new();
        runtime().block_on(async {
            let (final_request, mut requests) = mpsc::unbounded_channel();
            let (release, signal) = std::sync::mpsc::sync_channel(1);
            let wire = Arc::new(Wire {
                coordinator_requests: AtomicUsize::new(0),
                reviewer_requests: AtomicUsize::new(0),
                final_request,
                release: Mutex::new(signal),
            });
            let mut composed = composition(&fx, None, Arc::new(FakePower::default()));
            composed.runs.scope = None;
            composed.application.connections = Some(devworks_engine::ConnectionPorts {
                http: Arc::new(devworks_core::ports::http::NoNetwork),
                secrets: Arc::new(devworks_core::ports::secrets::SessionSecrets::new()),
                entropy: Arc::new(Entropy),
                endpoints: None,
            });
            composed.application.router = Some(Default::default());
            composed.config.providers = Some(
                devworks_engine::providers::driver::ProviderComposition::new(wire.clone()),
            );
            composed.config.process_tools = Some(ProcessToolComposition {
                bindings: ToolBindings::new(
                    [RegisteredExecutable { id: id("shell"), path: "/bin/sh".into() }],
                    [RegisteredWorkingRoot {
                        id: id("work"), project: Some(ProjectId(id("project-a"))),
                        path: fx.h.root.clone(),
                    }],
                ).unwrap(),
                filesystem: Arc::new(NativeFilesystem::new()),
                protected_roots: ProtectedRoots::new(Vec::new(), Vec::new(), None),
                secrets: Arc::new(devworks_core::ports::secrets::SessionSecrets::new()),
            });
            composed.indexing = Some(devworks_engine::IndexComposition {
                parser: Arc::new(devworks_adapters::index::treesitter::TreeSitterParser::new()),
                source: Arc::new(devworks_adapters::index::NativeSourceTree::new()),
                fs: Arc::new(NativeFilesystem::new()),
                settings: Default::default(),
                config: devworks_engine::IndexingConfig {
                    discover_at_boot: false,
                    ..Default::default()
                },
                enrichers: vec![],
            });
            std::fs::create_dir(fx.h.root.join("review-source")).unwrap();
            std::fs::create_dir(fx.h.root.join("capacity-work")).unwrap();
            let app = Arc::new(boot(composed).await.unwrap());
            // Keep the accepted process delivery queued until the test has published source drift.
            app.dispatcher().stop();
            super::super::plan_review_tests::seed_source(&fx, &app);
            seed_reviewed_plan(&fx, &app);
            seed_local_route(&fx);
            let frame = json!({
                "mode": "Command", "protocol": PROTOCOL, "schemaRevision": 3, "requestKey": fx.key(),
                "body": {"kind": "router.configure", "action": {"kind": "Tier", "tier": "High",
                    "expectedRevision": "1", "memberIds": ["watch-model"]}},
            });
            applied(&app.command(&fx.user, "router.configure", frame.to_string().as_bytes()).unwrap());
            app.runs().install_launcher(Arc::new(AgentLauncher::new(
                fx.store.clone(), fx.h.ids.clone(), app.agents().clone(),
                Arc::new(NativeAgentTurnExecutor::new(&app)),
            ))).unwrap();
            applied(&fx.start(&fx.user, "root-f", 1, 1));
            let describe = || {
                let activations = read(&*fx.store, |s| s.runs().activations(&ActivationFilter::Root(rid("root-f")), None, 5));
                let readiness = read(&*fx.store, |s| s.work().readiness(&id("root-f")));
                format!("provider requests {}, reviewer requests {}, activations {activations:?}, readiness {readiness:?}, events {:?}",
                    wire.coordinator_requests.load(Ordering::SeqCst), wire.reviewer_requests.load(Ordering::SeqCst), fx.events("root-f"))
            };
            let accepted = tokio::time::timeout(Duration::from_secs(12), requests.recv()).await
                .unwrap_or_else(|_| panic!("coordinator never acknowledged the queued effect: {}", describe())).unwrap();
            assert!(accepted.contains("operationId"), "{accepted}");
            let operation = read(&*fx.store, |s| s.operations().operations_for_capability(
                "process.execute", &[OperationState::IntentRecorded], None, 1,
            )).into_iter().next().unwrap_or_else(|| panic!("the accepted process intent is absent: {accepted}"));
            assert!(accepted.contains(operation.id.0.as_str()), "{accepted}");
            assert_eq!(operation.state, OperationState::IntentRecorded);
            let entry = read(&*fx.store, |s| s.queue().deliveries(&DeliveryFilter {
                consumer: None, operation_id: Some(operation.id.clone()),
            }, None, 10)).entries.pop().unwrap();
            let gate = Arc::new(RunGate::new(app.runs().service().clone(), None));
            fx.tx({
                let gate = gate.clone();
                let entry = entry.clone();
                move |_, tx| {
                    assert!(gate.may_dispatch(tx, &entry)?);
                    tx.indexing().publish(&id("review-repository"), &id("review-view-2"), 1)?;
                    assert!(!gate.may_dispatch(tx, &entry)?);
                    Ok(())
                }
            });
            release.send(()).unwrap();

            let resumed = tokio::time::timeout(Duration::from_secs(12), requests.recv()).await
                .unwrap_or_else(|_| panic!("coordinator never received the new review: {}", describe())).unwrap();
            assert!(resumed.contains("record a disposition for every finding"), "{resumed}");
            assert_eq!(wire.reviewer_requests.load(Ordering::SeqCst), 1);
            fx.tx(move |_, tx| {
                assert_eq!(tx.operations().operation(&operation.id)?.unwrap().state,
                    OperationState::IntentRecorded);
                assert!(!gate.may_dispatch(tx, &entry)?);
                Ok(())
            });
            app.shutdown().await;
        });
    }
}
