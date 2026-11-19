//! The native launcher and provider boundary for independent feature-plan review.
use super::*;
use devworks_core::ports::{
    AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization, Clock,
};
use devworks_core::services::artifacts::evidence::Evidence;
use devworks_core::services::work::model::{PendingPlan, plan_digest};
use devworks_core::services::work::plan_review::finding_id;

const PLAN: &str = "Acceptance: retain all committed changes if the index is rebuilt.";
const SOURCE: &str = "fn captured_rebuild_contract() {}";
const REDIRECT: &str = "Private conversation about the old design.";

struct FixtureEntropy;
impl devworks_core::ports::http::Entropy for FixtureEntropy {
    fn fill(&self, bytes: &mut [u8]) {
        bytes.fill(7);
    }
}

struct NativeAuthority {
    store: Arc<dyn ApplicationStore>,
    clock: Arc<dyn Clock>,
}

impl AuthorityPort for NativeAuthority {
    fn authorize(
        &self,
        ctx: &InvocationContext,
        request: &AuthorityRequest<'_>,
    ) -> Result<Authorization, AuthorityDenial> {
        HostAuthority {
            store: &*self.store,
            clock: &*self.clock,
            protected_branches: &[],
        }
        .authorize(ctx, request)
    }
}

#[derive(Default)]
struct ReviewWire(Mutex<Vec<Value>>);

impl StreamingHttp for ReviewWire {
    fn open(
        &self,
        request: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        let payload: Value = serde_json::from_slice(request.body.as_ref().unwrap()).unwrap();
        let body = payload.to_string();
        let isolated = body.contains("reviewedPlanDigest");
        self.0.lock().unwrap().push(payload.clone());
        let (delta, finish_reason) = if !isolated {
            (json!({"content": "done"}), "stop")
        } else if !body.contains("Use the allowed exact source views") {
            (
                json!({"content": "{\"needsSource\":true,\"findings\":[]}"}),
                "stop",
            )
        } else if !body.contains(SOURCE) {
            let name = payload["tools"][0]["function"]["name"]
                .as_str()
                .expect("source tool");
            assert_eq!(name, "code_source_query");
            assert_eq!(payload["tools"].as_array().unwrap().len(), 1);
            let arguments = json!({
                "scope": {"kind": "Project", "projectId": "project-a"},
                "view": {"kind": "Exact", "viewIds": ["review-view-1"]},
                "query": {"kind": "Source", "selection": {"kind": "File",
                    "repositoryId": "review-repository", "path": "lib.rs", "startLine": "1", "endLine": "1"}},
                "limits": {"maxResults": 10, "maxVisited": 10000, "deadlineMs": 5000}
            });
            let mut outside = arguments.clone();
            outside["view"]["viewIds"] = json!(["review-view-2"]);
            (
                json!({"tool_calls": [
                    {"index": 0, "id": "review-outside-view", "type": "function",
                        "function": {"name": name, "arguments": outside.to_string()}},
                    {"index": 1, "id": "review-source", "type": "function",
                        "function": {"name": name, "arguments": arguments.to_string()}}
                ]}),
                "tool_calls",
            )
        } else {
            let content = json!({"needsSource": false, "findings": [{
                "concern": "The plan does not cover interrupted rebuilds.",
                "consequence": "A second launch can publish an incomplete index.",
                "acceptanceCase": "Interrupt after writing a shard, then verify restart keeps every committed change."
            }]}).to_string();
            (json!({"content": content}), "stop")
        };
        let frame =
            json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]});
        Ok(Box::new(AgentStream {
            head: ResponseHead {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
            },
            body: Some(format!("data: {frame}\n\ndata: [DONE]\n\n").into_bytes()),
        }))
    }
}

fn route(fx: &Fx, app: &devworks_engine::Runtime) {
    super::watch_tests::seed_route(fx);
    let frame = json!({
        "mode": "Command", "protocol": PROTOCOL, "schemaRevision": 3, "requestKey": "review-route",
        "body": {"kind": "router.configure", "action": {"kind": "Tier", "tier": "High",
            "expectedRevision": "1", "memberIds": ["watch-model"]}},
    });
    let decision = app
        .command(&fx.user, "router.configure", frame.to_string().as_bytes())
        .unwrap();
    assert!(
        matches!(&decision, CommandReply::Recorded { receipt, .. }
        if matches!(receipt.decision, DomainDecision::Applied { .. })),
        "{decision:?}"
    );
}

fn seed_source(fx: &Fx, app: &devworks_engine::Runtime) {
    let repo = id("review-repository");
    let location =
        fx.h.root
            .join("review-source")
            .to_string_lossy()
            .into_owned();
    let source = app
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
                title: "Fixture captured source".into(),
                media_type: "text/plain".into(),
                tags: vec![],
                source: json!({"kind": "TestSource"}),
            },
            SOURCE.as_bytes(),
            |_, _| Ok(()),
        )
        .unwrap()
        .0;
    fx.tx(move |_, tx| {
        tx.projects().insert_repository(&StoredRepository {
            id: repo.clone(),
            location,
            name: "Review repository".into(),
            remote_url: None,
            primary: true,
            lifecycle: "Active".into(),
            languages: vec!["rust".into()],
            revision: 1,
            registered_at_ms: T0,
        })?;
        tx.projects()
            .set_members(&ProjectId(id("project-a")), std::slice::from_ref(&repo))?;
        tx.work()
            .set_work_repositories(&id("root-f"), std::slice::from_ref(&repo))?;
        let version = tx.artifacts().version(&source.version_id)?.unwrap();
        let capture = StoredGraphSourceCapture {
            id: id("review-capture"),
            repository_id: repo.clone(),
            unit_key: "lib.rs".into(),
            source_digest: version.blob_digest.clone(),
            blob_digest: version.blob_digest,
            blob_generation: version.blob_generation,
            byte_count: version.byte_count,
            created_at_ms: T0,
        };
        tx.graph().insert_graph_source_capture(&capture)?;
        let mut generation = StoredGeneration {
            id: id("review-generation"),
            repository_id: repo.clone(),
            layer: LAYER_BASELINE.into(),
            state: GEN_STAGING.into(),
            source_digest: "a".repeat(40),
            watermark: 0,
            manifest: json!({}),
        };
        tx.indexing().insert_generation(&generation)?;
        tx.indexing().put_unit(
            &StoredUnit {
                generation_id: generation.id.clone(),
                unit_key: capture.unit_key,
                content_digest: capture.source_digest,
                action: UNIT_REPLACE.into(),
                payload: json!({"sourceCapture": {"id": capture.id, "sha256": capture.blob_digest,
                "textEncoding": "Utf8", "lineCount": 1, "byteCount": capture.byte_count}}),
            },
            &[],
            &[],
        )?;
        generation.state = GEN_SEALED.into();
        generation.watermark = 1;
        tx.indexing().update_generation(&generation)?;
        let mut view = StoredView {
            view_id: id("review-view-1"),
            repository_id: repo.clone(),
            view_digest: devworks_core::fingerprint::sha256_hex(b"first review view"),
            baseline_id: generation.id,
            branch_id: None,
            worktree_id: None,
            coverage: json!({"state": "Complete", "commit": "a".repeat(40), "sourceRevision": 1}),
        };
        tx.indexing().insert_view(&view)?;
        tx.indexing().publish(&repo, &view.view_id, 0)?;
        view.view_id = id("review-view-2");
        view.view_digest = devworks_core::fingerprint::sha256_hex(b"second review view");
        view.coverage = json!({"state": "Complete", "commit": "b".repeat(40), "sourceRevision": 2});
        tx.indexing().insert_view(&view)
    });
}

#[test]
fn native_plan_review_holds_until_routing_is_repaired_and_reviews_without_planning_context() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    runtime().block_on(async {
        let wire = Arc::new(ReviewWire::default());
        let mut composed = composition_with(
            &fx, None, Arc::new(FakePower::default()),
            Arc::new(NativeAuthority { store: fx.store.clone(), clock: fx.h.clock() }),
            Arc::new(FakeHost::default()),
        );
        composed.runs.scope = None;
        composed.application = devworks_engine::ApplicationComposition {
            connections: Some(devworks_engine::ConnectionPorts {
                http: Arc::new(devworks_core::ports::http::NoNetwork),
                secrets: Arc::new(devworks_core::ports::secrets::SessionSecrets::new()),
                entropy: Arc::new(FixtureEntropy),
                endpoints: None,
            }),
            router: Some(Default::default()),
            ..Default::default()
        };
        composed.config.providers = Some(
            devworks_engine::providers::driver::ProviderComposition::new(wire.clone()),
        );
        let shared = fx.h.root.join("shared-skills");
        std::fs::create_dir_all(shared.join("review-marker")).unwrap();
        std::fs::write(shared.join("review-marker/SKILL.md"), "Shared skill marker outside the plan.").unwrap();
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
        composed.config.instructions = Some(devworks_engine::runtime::InstructionComposition {
            source: Arc::new(devworks_adapters::index::NativeSourceTree::new()),
            files: Arc::new(devworks_adapters::fs::skills::NativeSkillFiles::new(
                fx.h.root.join("harness-skills"),
            )),
            roots: devworks_core::services::instructions::SkillRoots {
                shared: Some(shared),
                harness: fx.h.root.join("harness-skills"),
            },
        });
        let app = Arc::new(boot(composed).await.unwrap());
        let skill = read(&*fx.store, |snapshot| {
            snapshot.instructions().instruction_items(&InstructionFilter {
                kind: Some(InstructionKind::Skill),
                scopes: vec![ConfigScope::Workspace],
                name: None,
            }, None, 1)
        }).pop().unwrap();
        applied(&app.command(&fx.user, "skills.rollback", json!({
            "mode": "Command", "protocol": PROTOCOL, "requestKey": fx.key(),
            "body": {"kind": "skills.rollback", "instructionId": skill.id,
                "expectedRevision": "1", "targetRevision": "1"}
        }).to_string().as_bytes()).unwrap());
        let reference = app.artifacts().publish_evidence(
            &*fx.store,
            Evidence {
                scope: ArtifactScope::Project(ProjectId(id("project-a"))),
                producer: fx.user.principal().clone(),
                principal_kind: "Human".into(),
                agent_session: None,
                root: None,
                assignment: None,
                title: "Feature plan".into(),
                media_type: "text/plain".into(),
                tags: vec![],
                source: json!({"kind": "TestPlan"}),
            },
            PLAN.as_bytes(), |_, _| Ok(()),
        ).unwrap().0;
        seed_source(&fx, &app);
        let draft = serde_json::from_value(json!({
            "content": reference, "repositoryIds": ["review-repository"], "acceptance": [], "goals": [],
            "acceptanceTests": [],
            "requirements": [{"id": "E1", "statement": "Retain commits across rebuild", "status": "Confirmed", "coveredByTaskIds": []}],
        })).unwrap();
        let digest = plan_digest(&draft);
        let pending = serde_json::to_value(PendingPlan {
            digest: digest.clone(),
            draft,
            budget: None,
            review_required: true,
        }).unwrap();
        fx.tx(move |_, tx| tx.work().put_feature_plan(&StoredFeaturePlan {
            feature_id: id("root-f"),
            revision: 1,
            approved: None,
            approved_by: None,
            approved_at_ms: None,
            pending: Some(pending),
            pending_revision: Some(1),
        }, None));
        let run_control = |operation: &str, key: &str, mut command: Value| {
            command["kind"] = json!(operation);
            app.command(&fx.user, operation, json!({
                "protocol": PROTOCOL, "requestKey": key, "command": command,
            }).to_string().as_bytes()).unwrap()
        };
        let start = |key: &str, revision: i64| run_control("runs.start", key, json!({
            "rootId": "root-f", "expectedWorkRevision": "1",
            "expectedControlRevision": revision.to_string(),
        }));
        let start_key = fx.key();
        applied(&start(&start_key, 1));
        assert!(replayed(&start(&start_key, 1)));
        let attachment = seed_redirect_artifact(
            &fx, "prior-attachment", "prior-attachment-version", &"b".repeat(64),
        );
        applied(&app.command(&fx.user, "runs.redirect", json!({
            "mode": "Command", "protocol": PROTOCOL, "requestKey": fx.key(), "body": {
                "kind": "runs.redirect", "rootId": "root-f",
                "expectedControlRevision": fx.control("root-f").revision.to_string(),
                "message": REDIRECT, "attachments": [attachment],
            },
        }).to_string().as_bytes()).unwrap());
        app.runs().install_launcher(Arc::new(AgentLauncher::new(
            fx.store.clone(),
            fx.h.ids.clone(),
            app.agents().clone(),
            Arc::new(NativeAgentTurnExecutor::new(&app)),
        ))).unwrap();
        let review = |index: &str| {
            read(&*fx.store, |snapshot| snapshot.work().readiness(&id("root-f")))
                .map_or(Value::Null, |r| r.body["planReviews"][0][index].clone())
        };
        within("The reviewer failure is recorded", async {
            while review("outcome")["kind"] != "Failed" || fx.live("root-f").is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await;
        assert!(review("outcome")["reason"].as_str().unwrap().contains("route"), "{:?}", review("outcome"));
        assert!(wire.0.lock().unwrap().is_empty());
        assert_eq!(fx.eligibility(&fx.user, "root", "root-f", "Implementation")["eligible"], false);
        let failed_session = review("session");

        let failed_actor = ActorId(id(review("actor").as_str().unwrap()));
        let failed_assignment = AssignmentId(id(review("assignment").as_str().unwrap()));
        let failed_activation = id(review("activation").as_str().unwrap());
        let failed_epoch = review("ownerEpoch").as_i64().unwrap();
        let stale = devworks_core::context::HostIngress::new(&*fx.store).context(
            failed_actor, PrincipalKind::Agent, id("late-review-invocation"), id("native-agent"),
            devworks_core::protocol::Counter::new(fx.store.info().restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        ).with_session(id(failed_session.as_str().unwrap()))
            .with_lineage(Some(ProjectId(id("project-a"))), Some(rid("root-f")), Some(failed_assignment))
            .with_activation(failed_activation, failed_epoch);

        route(&fx, &app);
        applied(&start(&fx.key(), fx.control("root-f").revision));
        within("A new review completes through native routing", async {
            while review("outcome")["kind"] != "Complete" || fx.live("root-f").is_some() {
                let outcome = review("outcome");
                assert!(outcome["kind"] != "Failed" || review("session") == failed_session,
                    "retry failed: {outcome}; requests: {:?}", wire.0.lock().unwrap());
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await;
        assert_ne!(review("session"), failed_session);
        assert_eq!(review("outcome")["findings"], 1);
        {
            let requests = wire.0.lock().unwrap();
            let reviewer: Vec<_> = requests.iter().filter(|r| r.to_string().contains("reviewedPlanDigest")).collect();
            assert_eq!(reviewer.len(), 3, "{reviewer:?}");
            assert!(reviewer[2].to_string().contains(SOURCE));
            assert!(reviewer[2].to_string().contains("domain_PermissionDenied"));
            for request in &reviewer {
                let body = request.to_string();
                assert!(body.contains(PLAN));
                assert!(body.contains("Retain commits across rebuild"));
                assert!(!body.contains(REDIRECT));
                assert!(!body.contains("prior-attachment"));
                assert!(!body.contains("review-marker"));
                assert!(!body.contains("Shared skill marker"));
            }
            let isolated = reviewer[0];
            assert!(!isolated.to_string().contains("review-view-1"));
            assert!(reviewer[1].to_string().contains("review-view-1"));
            assert!(isolated["tools"].as_array().is_none_or(Vec::is_empty), "{isolated}");
        }

        assert_eq!(fx.eligibility(&fx.user, "root", "root-f", "Implementation")["eligible"], false);
        let session = id(review("session").as_str().unwrap());
        assert!(!read(&*fx.store, |s| s.artifacts().links(&id("prior-attachment")))
            .iter().any(|link| link.subject == SubjectKey::session(&session)));
        let status = read(&*fx.store, |snapshot| snapshot.work().work(&id("root-f"))).unwrap();
        let assessment_revision = read(&*fx.store, |s| s.work().readiness(&id("root-f")))
            .unwrap().revision.to_string();
        let assess = |findings: Value| {
            app.command(&fx.user, "features.edit", json!({
                "mode": "Command", "protocol": PROTOCOL, "schemaRevision": 3, "requestKey": fx.key(),
                "body": {"kind": "features.edit", "workId": "root-f",
                    "expectedRevision": status.revision.to_string(), "changes": {
                        "readiness": {"kind": "Assess", "planRevision": "1", "planDigest": digest,
                            "expectedAssessmentRevision": assessment_revision,
                            "summary": "Rebuild behavior reviewed", "findings": findings}
                    }
                }
            }).to_string().as_bytes()).unwrap()
        };
        let refused = assess(json!([]));
        assert!(matches!(decision(&refused), DomainDecision::Refused { .. }), "{refused:?}");
        let late_result = fx.tx(move |_, tx| {
            Ok(devworks_core::services::work::plan_review::settle(
                tx, &stale, devworks_core::services::work::plan_review::ReviewOutcome::Failed {
                    reason: "late result from the abandoned reviewer".into(),
                },
            ))
        });
        assert!(matches!(late_result, Err(StorageError::ScopeDenied) | Ok(false)), "{late_result:?}");
        assert_eq!(review("session"), json!(session));
        assert_eq!(review("outcome")["kind"], "Complete");
        let report = review("outcome")["report"].clone();
        applied(&assess(json!([{
            "id": finding_id(&session, 0), "disposition": "ResolvedDecision", "state": "Resolved",
            "blocking": false, "prerequisite": false, "requirementIds": ["E1"], "affectedWorkIds": [],
            "summary": "Interrupted rebuild behavior verified", "missing": "", "consequence": "",
            "attempts": [], "insufficiency": "", "closureCondition": "",
            "decision": "The test specification includes the interruption case.",
            "confidence": "High", "evidence": [report], "closureEvidence": [],
        }])));
        let unapproved = fx.eligibility(&fx.user, "root", "root-f", "Implementation");
        assert_eq!(unapproved["eligible"], false);
        assert_eq!(unapproved["blockers"]["items"][0]["cause"], "Approval", "{unapproved}");
        let work = read(&*fx.store, |s| s.work().work(&id("root-f"))).unwrap();
        let requested = app.command(&fx.user, "features.request_plan_approval", json!({
            "mode": "Command", "protocol": PROTOCOL, "requestKey": fx.key(),
            "body": {"kind": "features.request_plan_approval", "featureId": "root-f",
                "expectedRevision": work.revision.to_string(), "planRevision": "1", "planDigest": digest},
        }).to_string().as_bytes()).unwrap();
        let DomainDecision::Applied {
            result: devworks_core::protocol::results::OperationResult::FeaturesRequestPlanApproval {
                value: proposal,
            },
        } = decision(&requested) else {
            panic!("plan approval request: {requested:?}");
        };
        applied(&app.record_human_decision(&fx.user, json!({
            "mode": "Command", "protocol": PROTOCOL, "schemaRevision": 3, "requestKey": fx.key(),
            "body": {"kind": "approvals.record_human_decision", "proposalId": proposal.id,
                "expectedRevision": proposal.revision, "contentDigest": proposal.content_digest,
                "decision": "Approve", "rationale": "Approve the exact reviewed draft"},
        }).to_string().as_bytes()).unwrap());
        assert_eq!(review("session"), json!(session));
        assert_eq!(fx.eligibility(&fx.user, "root", "root-f", "Implementation")["eligible"], true);
        applied(&run_control("runs.pause", &fx.key(), json!({
            "rootId": "root-f", "expectedControlRevision": fx.control("root-f").revision.to_string(),
        })));
        let holds: Vec<_> = fx.holds("root-f").into_iter()
            .filter(|hold| hold.cause.manual_resume())
            .map(|hold| json!({"holdId": hold.id, "revision": hold.revision.to_string()}))
            .collect();
        applied(&run_control("runs.continue", &fx.key(), json!({
            "rootId": "root-f", "expectedControlRevision": fx.control("root-f").revision.to_string(),
            "coveredHolds": holds,
        })));
        within("Continue reuses the settled review", async {
            while fx.live("root-f").is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await;
        assert_eq!(review("session"), json!(session));
        assert_eq!(wire.0.lock().unwrap().iter()
            .filter(|body| body.to_string().contains("reviewedPlanDigest")).count(), 3);
        fx.tx(|_, tx| {
            tx.indexing().publish(&id("review-repository"), &id("review-view-2"), 1)?;
            Ok(())
        });
        assert_eq!(fx.eligibility(&fx.user, "root", "root-f", "Implementation")["eligible"], false);
    });
}
