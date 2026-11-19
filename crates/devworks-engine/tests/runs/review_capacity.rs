//! Readiness review must run while the only ordinary worker slot is occupied.
use super::*;
use devworks_core::operations::agents::DelegateInput;
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::AgentRecord;
use devworks_core::protocol::support::{BoundedList, BoundedText};
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

fn seed_reviewed_plan(fx: &Fx) -> ArtifactRef {
    let document = seed_redirect_artifact(
        fx,
        "capacity-plan",
        "capacity-plan-version",
        &"a".repeat(64),
    );
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
        let report = seed_reviewed_plan(&fx);
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
