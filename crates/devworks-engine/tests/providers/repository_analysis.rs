//! Repository inference uses the real router and usage journal with scripted transport.
use super::*;
use devworks_core::ports::{
    AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization, Clock,
};
use devworks_core::protocol::{
    CommandReply, OperationId, OperationState,
    results::{DomainDecision, DomainQueryReply, OperationResult},
};
use std::{path::Path, process::Command, time::Duration};

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
        devworks_core::authority::HostAuthority {
            store: self.store.as_ref(),
            clock: self.clock.as_ref(),
            protected_branches: &[],
        }
        .authorize(ctx, request)
    }
}

fn response(content: Value) -> String {
    let delta = json!({"model": "fixture-model", "choices": [{
        "index": 0, "delta": {"content": content.to_string()}, "finish_reason": null
    }]});
    let usage = json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
        "usage":{"prompt_tokens": 100, "completion_tokens": 25}});
    format!("data: {delta}\n\ndata: {usage}\n\ndata: [DONE]\n\n")
}

fn submit(
    app: &devworks_engine::Runtime,
    ctx: &InvocationContext,
    operation: &str,
    key: &str,
    mut input: Value,
) -> CommandReply<DomainDecision> {
    input["kind"] = json!(operation);
    let revision = match operation {
        "repository_docs.generate" => 14,
        operation if operation.starts_with("analysis.") => 3,
        _ => 1,
    };
    let raw = json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":revision,
        "requestKey":key,"body":input})
    .to_string();
    let reply = app.command(ctx, operation, raw.as_bytes()).unwrap();
    if operation == "repository_docs.generate"
        && let CommandReply::Recorded { receipt, .. } = &reply
        && let DomainDecision::Accepted { operation_id } = &receipt.decision
    {
        eprintln!(
            "documentation fixture {key} uses {}",
            operation_id.0.as_str()
        );
    }
    reply
}

fn accepted(reply: CommandReply<DomainDecision>) -> OperationId {
    match reply {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

async fn terminal(store: &dyn ApplicationStore, operation: &OperationId) -> OperationRecord {
    let label = format!("dispatcher settles operation {}", operation.0.as_str());
    within(&label, async {
        loop {
            let row = read(store, |s| s.operations().operation(operation)).unwrap();
            if matches!(
                row.state,
                OperationState::ObservedComplete
                    | OperationState::Failed
                    | OperationState::OutcomeUnknown
                    | OperationState::CancelledBeforeDispatch
            ) {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
}

fn prepare_repo(base: &Path) {
    let repo = base.join("analysis-native");
    let project = repo.join("components").join("support");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(
        project.join("DocSupport.csproj"),
        "<Project Sdk=\"Microsoft.NET.Sdk\"></Project>\n",
    )
    .unwrap();
    std::fs::write(
        project.join("src/lib.rs"),
        "pub fn fixture_first_party() -> u32 { 4 }\n",
    )
    .unwrap();
    let remote = base.join("documentation-origin.git");
    let remote_path = remote.to_str().unwrap();
    for command in [
        vec!["init", "-q"],
        vec!["add", "-A"],
        vec!["commit", "-qm", "fixture"],
        vec!["config", "user.name", "Fixture"],
        vec!["config", "user.email", "fixture@example.test"],
        vec!["config", "commit.gpgsign", "false"],
        vec!["init", "--bare", remote_path],
        vec!["remote", "add", "origin", remote_path],
    ] {
        let output = Command::new("git")
            .current_dir(&repo)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(command)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn repository_analysis_uses_the_router_and_records_synthesis_once() {
    let _case = native_runtime_case();
    runtime().block_on(async {
        let script = response(json!({"summary":"A test project.",
            "confidence":"high","endpoints":[],"services":[],"databaseTables":[],
            "externalDependencies":[],"nodes":[]}));
        let f = Fx::new("CompatibleChat", Script::sse(&script), false);
        let base = f.h.root.join("analysis-fixtures");
        prepare_repo(&base);
        let user = principal(f.store.as_ref(), "analysis-user", PrincipalKind::Human);
        let settings = [
            ("routing.tierHigh.members", json!([f.model])),
            ("repositories.baseDirectory", json!(base)),
        ].into_iter().map(|(key, value)| {
            let spec = devworks_core::settings::catalog::descriptor(key).unwrap();
            serde_json::from_value(json!({"key": key, "value": {"schema": spec.schema_ref(), "value": value}})).unwrap()
        }).collect::<Vec<_>>();
        let owner = user.clone();
        write(f.store.as_ref(), move |tx| {
            devworks_core::settings::service::apply_human_assignments(
                tx, &owner, &ConfigScope::Workspace, &settings, json!({})
            ).map_err(|error| StorageError::InvalidRecord(format!("{error:?}")))?;
            Ok(())
        }).unwrap();

        let mut composition = tool_runtime_composition(&f);
        composition.authority = Arc::new(NativeAuthority { store: f.store.clone(), clock: f.h.clock() });
        composition.indexing = Some(devworks_engine::IndexComposition {
            parser: Arc::new(TreeSitterParser::new()), source: Arc::new(NativeSourceTree::new()),
            fs: Arc::new(NativeFilesystem::new()),
            settings: devworks_core::services::indexing::IndexSettings::default(),
            config: devworks_engine::indexing::IndexingConfig {
                discover_at_boot: false, tick: Duration::from_millis(40),
                ..Default::default()
            },
            enrichers: Vec::new(),
        });
        let docs_root = f.h.root.join("documentation-worktrees");
        std::fs::create_dir(&docs_root).unwrap();
        composition.git = Some(devworks_engine::git::GitComposition {
            config: devworks_core::services::git::GitConfig {
                transport: Some(Arc::new(devworks_adapters::git_transport::NativeGitTransportFiles)),
                roots: vec![devworks_core::services::git::RegisteredGitRoot {
                    id: id("documentation-worktrees"),
                    path: std::fs::canonicalize(&docs_root).unwrap(),
                }],
                filesystem: Some(Arc::new(NativeFilesystem::new())),
                ..Default::default()
            },
            github_http: None,
        });
        let app = devworks_engine::boot(composition).await.unwrap();
        let discovered = json!({"mode":"Query","protocol":"1.0-draft.1","queryId":"analysis-sources",
            "body":{"kind":"indexing.discover","scope":{"kind":"Workspace"},"roots":[],"page":{"limit":5}}});
        let reply = app.query(&user, "indexing.discover", discovered.to_string().as_bytes()).unwrap();
        let DomainQueryReply::Result { result: OperationResult::IndexingDiscover { value }, .. } = reply
            else { panic!("{reply:?}") };
        let repository = value.items.0[0].id.clone();
        let views = json!({"kind":"Published","repositoryIds":[repository]});
        let indexed = json!({"mode":"Command","protocol":"1.0-draft.1","requestKey":"index-analysis-fixture",
            "body":{"kind":"indexing.index","scope":{"kind":"Workspace"},"repositoryIds":[repository],
                "expectedSourceRevision":"1","views":views}});
        let reply = app.command(&user, "indexing.index", indexed.to_string().as_bytes()).unwrap();
        let CommandReply::Recorded { receipt, .. } = reply else { panic!("{reply:?}") };
        let DomainDecision::Applied { result: OperationResult::IndexingIndex { value } } = receipt.decision
            else { panic!("{:?}", receipt.decision) };
        within("the source is indexed", async {
            loop {
                let row = read(f.store.as_ref(), |s| s.indexing().run(&value.id)).unwrap();
                if row.state == RUN_SUCCEEDED { break; }
                assert!(!run_terminal(&row.state), "{row:?}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await;
        assert!(f.http.lock().unwrap().requests.lock().unwrap().is_empty());

        let common = json!({"scope":{"kind":"Workspace"},"views":views,
            "expectedSourceRevision":"1","nodeIds":[],"batchLimit":1});
        let mut analyze = common.clone();
        analyze["refreshMode"] = json!("WithProjectAnalysis");
        analyze["includeAllSource"] = json!(true);
        let project = accepted(submit(&app, &user, "analysis.reanalyze", "analyze-fixture", analyze.clone()));
        let settled = terminal(f.store.as_ref(), &project).await;
        let run: Id = serde_json::from_value(settled.payload["runId"].clone()).unwrap();
        let analysis = read(f.store.as_ref(), |s| s.analysis().analysis_run(&run)).unwrap();
        assert_eq!(settled.state, OperationState::ObservedComplete,
            "phases {:?}, transport count {}", analysis.phases.iter()
                .map(|phase| (phase.phase, phase.state, phase.diagnostic.as_deref(),
                    phase.subjects.iter().map(|subject| subject.diagnostic.as_deref()).collect::<Vec<_>>()))
                .collect::<Vec<_>>(), f.http.lock().unwrap().requests.lock().unwrap().len());
        assert_eq!(accepted(submit(&app, &user, "analysis.reanalyze", "analyze-fixture", analyze)), project);
        {
            let transport = f.http.lock().unwrap();
            let requests = transport.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let request = std::str::from_utf8(requests[0].body.as_ref().unwrap()).unwrap();
            assert!(request.contains("pub fn fixture_first_party()"));
        }

        let synthesis_wire = response(json!({"repoSummary":"Synthesis of the fixture project.", "confidence":"high"}));
        f.http.lock().unwrap().body = VecDeque::from([synthesis_wire.into_bytes()]);
        let summary = accepted(submit(&app, &user, "analysis.synthesize", "synthesize-fixture", common.clone()));
        assert_eq!(terminal(f.store.as_ref(), &summary).await.state, OperationState::ObservedComplete);
        let saved = read(f.store.as_ref(), |s| s.history().page(&EventFilter::All, 0, 500));
        let events = saved.events.iter().filter(|event| event.kind == "RepositoryAnalysisSynthesized")
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["repositoryId"], json!(repository));
        assert_eq!(events[0].payload["projectCheckpointIds"].as_array().unwrap().len(), 1);
        let reply = submit(&app, &user, "analysis.process_batch", "process-saved-fixture", common.clone());
        assert!(matches!(reply, CommandReply::Recorded { receipt, .. }
            if matches!(receipt.decision, DomainDecision::NoChange { .. })));
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 2);

        let attempt_id = analysis.phases.iter().flat_map(|phase| &phase.subjects)
            .find_map(|subject| subject.output.as_ref())
            .and_then(|output| serde_json::from_value::<Id>(output["attempt"].clone()).ok()).unwrap();
        let attempt = read(f.store.as_ref(), |s| s.usage().inference_attempt(&attempt_id)).unwrap();
        assert_eq!(attempt.actor_id, *user.principal());
        assert_eq!(attempt.purpose, "Enrichment");

        // A completed transport with an invalid structured result cannot emit another synthesis.
        f.http.lock().unwrap().body = VecDeque::from([response(json!({"summary": ["invalid"]})).into_bytes()]);
        let bad = accepted(submit(&app, &user, "analysis.synthesize", "malformed-summary", common));
        assert_eq!(terminal(f.store.as_ref(), &bad).await.state, OperationState::Failed);
        let saved = read(f.store.as_ref(), |s| s.history().page(&EventFilter::All, 0, 500));
        assert_eq!(saved.events.iter().filter(|event| event.kind == "RepositoryAnalysisSynthesized").count(), 1);
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 3);

        check_agent_analysis(&f, &app, &user, repository).await;
        observe_after_source_lease_expiry(&f, analysis, attempt_id);
        app.shutdown().await;
        f.usage.stop();
        f.usage.drain().await;
    });
}

async fn check_agent_analysis(
    f: &Fx,
    app: &devworks_engine::Runtime,
    user: &InvocationContext,
    repository: Id,
) {
    use devworks_core::protocol::{DomainErrorCode, ProjectId};
    use devworks_core::settings::catalog::AgentRole;
    let project = ProjectId(id("analysis-scope"));
    let work = principal(f.store.as_ref(), "analysis-worker", PrincipalKind::Agent).with_lineage(
        Some(project.clone()),
        None,
        None,
    );
    let review = principal(f.store.as_ref(), "analysis-reviewer", PrincipalKind::Agent)
        .with_lineage(Some(project.clone()), None, None);
    let plan_review = principal(
        f.store.as_ref(),
        "analysis-plan-reviewer",
        PrincipalKind::Agent,
    )
    .with_lineage(Some(project.clone()), None, None);
    let caller = user.clone();
    let owner = project.clone();
    let members = vec![repository.clone()];
    let bindings = [
        (work.principal().clone(), AgentRole::Worker),
        (review.principal().clone(), AgentRole::CodeReviewer),
        (plan_review.principal().clone(), AgentRole::PlanReviewer),
    ];
    write(f.store.as_ref(), move |tx| {
        let now = tx.now_ms();
        tx.projects().insert_project(&StoredProject {
            id: owner.clone(),
            name: "Analysis scope".into(),
            description: String::new(),
            lifecycle: ACTIVE.into(),
            revision: 1,
            scope_revision: 1,
            created_at_ms: now,
            updated_at_ms: now,
            archived_at_ms: None,
            deleted_at_ms: None,
            archive_epoch: 0,
            archive_stop_pending: false,
        })?;
        tx.projects().set_members(&owner, &members)?;
        for (actor, role) in bindings {
            bind_principal(
                tx,
                &caller,
                NewPrincipalBinding {
                    actor,
                    kind: PrincipalKind::Agent,
                    role: Some(role),
                    project_id: Some(owner.clone()),
                    tool_families: None,
                    bound_by: caller.principal().clone(),
                },
                None,
            )
            .map_err(|err| StorageError::InvalidRecord(format!("{err:?}")))?;
        }
        Ok(())
    })
    .unwrap();

    let schemas = app.providers().unwrap().schemas();
    let worker_tools = schemas.selected_builtins(&work).unwrap();
    let reviewer_tools = schemas.selected_builtins(&review).unwrap();
    for operation in devworks_core::services::graph::repository_analysis::COMMANDS {
        let capability = format!("devworks-command/{operation}");
        assert!(
            worker_tools
                .iter()
                .any(|tool| tool.capability_id.as_str() == capability),
            "{capability}"
        );
        assert!(
            !reviewer_tools
                .iter()
                .any(|tool| tool.capability_id.as_str() == capability),
            "{capability}"
        );
    }
    let alias = worker_tools
        .iter()
        .find(|tool| tool.wire_name == "get_repository_health")
        .unwrap();
    assert!(schemas.tool(&work, alias).is_ok());
    assert!(schemas.resolve(&work, &alias.input_schema).is_ok());
    assert!(schemas.tool(&plan_review, alias).is_err());
    assert!(schemas.resolve(&plan_review, &alias.input_schema).is_err());

    let scope = json!({"kind":"Project","projectId":project});
    let views = json!({"kind":"Published","repositoryIds":[repository]});
    let mut request = json!({"scope":scope,"views":views,"expectedSourceRevision":"1",
        "nodeIds":[],"batchLimit":8,"refreshMode":"VitalsOnly"});
    let scoped = accepted(submit(
        app,
        user,
        "analysis.reanalyze",
        "analysis-project-scoped",
        request.clone(),
    ));
    let record = terminal(f.store.as_ref(), &scoped).await;
    assert_eq!(record.state, OperationState::ObservedComplete, "{record:?}");
    assert!(rejected(
        submit(
            app,
            &review,
            "analysis.reanalyze",
            "reviewer-refusal",
            request.clone()
        ),
        DomainErrorCode::PermissionDenied
    ));
    request["scope"]["projectId"] = json!("wrong-project");
    let wrong = submit(
        app,
        &work,
        "analysis.reanalyze",
        "wrong-project-refusal",
        request,
    );
    assert!(
        rejected(wrong.clone(), DomainErrorCode::PermissionDenied)
            || rejected(wrong.clone(), DomainErrorCode::ViewUnavailable),
        "{wrong:?}"
    );

    let body = json!({"mode":"Query","protocol":"1.0-draft.1","schemaRevision":3,"queryId":"agent-health",
        "body":{"kind":"analysis.health","scope":scope,"views":views,"page":{"limit":10},"filters":[]}});
    let reply = app
        .query(&work, "analysis.health", body.to_string().as_bytes())
        .unwrap();
    let DomainQueryReply::Result {
        result: OperationResult::AnalysisHealth { value },
        ..
    } = reply
    else {
        panic!("{reply:?}")
    };
    let reference = value
        .report
        .expect("the authorized worker reads project evidence");
    assert!(
        f.artifacts
            .read_exact(f.store.as_ref(), &work, &reference, 16 * 1024 * 1024)
            .is_ok()
    );

    check_repository_documentation(f, app, user, &project, &repository).await;
    write(f.store.as_ref(), move |tx| {
        tx.projects().set_members(&project, &[])
    })
    .unwrap();
    let reply = app
        .query(&work, "analysis.health", body.to_string().as_bytes())
        .unwrap();
    assert!(matches!(reply, DomainQueryReply::Rejected { .. }));
    assert!(
        f.artifacts
            .read_exact(f.store.as_ref(), &work, &reference, 16 * 1024 * 1024)
            .is_err()
    );
    assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 5);
}

async fn check_repository_documentation(
    f: &Fx,
    app: &devworks_engine::Runtime,
    user: &InvocationContext,
    project: &devworks_core::protocol::ProjectId,
    repository: &Id,
) {
    use devworks_core::protocol::{DomainErrorCode, RootId, records::AnalysisRecord};
    use devworks_core::services::git::context::documentation_destination;
    let task = id("document-the-repository");
    let context =
        user.clone()
            .with_lineage(Some(project.clone()), Some(RootId(task.clone())), None);
    let (project_id, task_id, repo_id) = (project.clone(), task.clone(), repository.clone());
    write(f.store.as_ref(), move |tx| {
        let now = tx.now_ms();
        tx.work().insert_work(&StoredWork {
            id: task_id.clone(),
            project_id,
            kind: "task".into(),
            title: "Document repository".into(),
            parent_feature_id: None,
            status_id: None,
            priority: 0,
            manual_order: 0,
            revision: 1,
            scope_revision: 1,
            created_at_ms: now,
            updated_at_ms: now,
            created_by: None,
            updated_by: None,
            search_key: "documentation".into(),
            caller_ref: None,
            completion: None,
            brief: json!({"acceptance":[{
                "id":"docs-delivered","text":"Publish complete repository documentation",
                "evidenceIds":[],"state":"Unmet"
            }]}),
        })?;
        tx.work().set_work_repositories(&task_id, &[repo_id])
    })
    .unwrap();
    let views = json!({"kind":"Published","repositoryIds":[repository]});
    let scope = json!({"kind":"Project","projectId":project});
    let query = json!({"mode":"Query","protocol":"1.0-draft.1","schemaRevision":3,"queryId":"documentation-view",
        "body":{"kind":"analysis.status","scope":scope,"views":views,"page":{"limit":10},"filters":[]}});
    let status = || -> AnalysisRecord {
        let result = app
            .query(user, "analysis.status", query.to_string().as_bytes())
            .unwrap();
        let DomainQueryReply::Result {
            result: OperationResult::AnalysisStatus { value },
            ..
        } = result
        else {
            panic!("{result:?}")
        };
        value
    };
    let observed = status();
    let head = observed.observation.as_ref().unwrap().bindings.0[0]
        .snapshot
        .head
        .clone();
    let checkout = accepted(submit(
        app,
        &context,
        "git.worktree",
        "documentation-worktree",
        json!({
            "workId":task, "repositoryId":repository, "expectedRevision":"1", "action":"Create",
            "base":head, "registeredRootId":"documentation-worktrees"
        }),
    ));
    let completed = terminal(f.store.as_ref(), &checkout).await;
    assert_eq!(
        completed.state,
        OperationState::ObservedComplete,
        "{completed:?}"
    );
    let tree = read(f.store.as_ref(), |read| {
        read.git().git_worktrees(&task, None, 5)
    })
    .remove(0);
    let path = Path::new(&tree.path);
    std::fs::write(path.join("local-user-notes.md"), "my notes\n").unwrap();

    f.http.lock().unwrap().body = VecDeque::from([response(json!({
        "summary":"A repository with a validated docs destination.",
        "confidence":"high","endpoints":[],"services":[],"databaseTables":[],
        "externalDependencies":[],"nodes":[]
    }))
    .into_bytes()]);
    let plan = json!({"scope":scope,"views":views,"expectedSourceRevision":"1","nodeIds":[],"batchLimit":2});
    let mut analyze = plan.clone();
    analyze["refreshMode"] = json!("WithProjectAnalysis");
    analyze["includeAllSource"] = json!(true);
    let projects = accepted(submit(
        app,
        user,
        "analysis.reanalyze",
        "document-projects",
        analyze,
    ));
    let completed = terminal(f.store.as_ref(), &projects).await;
    assert_eq!(
        completed.state,
        OperationState::ObservedComplete,
        "{completed:?}"
    );

    let (bound, root) =
        documentation_destination(f.store.as_ref(), user, project, repository, &head).unwrap();
    f.http.lock().unwrap().body = VecDeque::from([response(json!({
        "repoSummary":"A durable repository documentation acceptance fixture.", "confidence":"high"
    }))
    .into_bytes()]);
    let summary = accepted(submit(
        app,
        user,
        "analysis.synthesize",
        "documentation-synthesis",
        plan,
    ));
    let completed = terminal(f.store.as_ref(), &summary).await;
    assert_eq!(
        completed.state,
        OperationState::ObservedComplete,
        "{completed:?}"
    );
    let evidence = within(
        "synthesis generates owned repository documentation",
        async {
            loop {
                let page = read(f.store.as_ref(), |read| {
                    read.history().page(&EventFilter::All, 0, 500)
                });
                if let Some(event) = page
                    .events
                    .iter()
                    .find(|event| event.kind == "RepositoryDocumentationGenerated")
                {
                    break event.clone();
                }
                let synthesis = page
                    .events
                    .iter()
                    .find(|event| {
                        event.kind == "RepositoryAnalysisSynthesized"
                            && event.operation_id.as_ref() == Some(&summary)
                    })
                    .expect("project synthesis event");
                let delivery = read(f.store.as_ref(), |read| {
                    read.queue().delivery(
                        devworks_core::services::graph::repository_analysis::DOCS_CONSUMER,
                        &synthesis.id,
                    )
                })
                .expect("documentation delivery");
                assert!(
                    !matches!(delivery.state, QueueState::Failed | QueueState::Reconciling),
                    "{delivery:?}"
                );
                if delivery.state == QueueState::Completed {
                    let tree =
                        read(f.store.as_ref(), |read| read.git().git_worktree(&tree.id)).unwrap();
                    if let Some(op) = &tree.pending_operation_id {
                        let effect =
                            read(f.store.as_ref(), |read| read.operations().operation(op)).unwrap();
                        assert!(
                            !matches!(
                                effect.state,
                                OperationState::Failed | OperationState::OutcomeUnknown
                            ),
                            "{effect:?}"
                        );
                    } else {
                        assert_eq!(tree.last_operation_id.as_ref(), Some(&checkout), "{tree:?}");
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        },
    )
    .await;
    let document = std::fs::read_to_string(path.join("CODEGRAPH.md")).unwrap();
    assert!(document.contains("A durable repository documentation acceptance fixture."));
    assert!(document.contains(&head.hex));
    assert_eq!(
        std::fs::read_to_string(path.join("local-user-notes.md")).unwrap(),
        "my notes\n"
    );
    assert_eq!(evidence.payload["repositoryId"], json!(repository));
    assert_eq!(evidence.payload["sourceHead"], json!(head));
    let index = Command::new("git")
        .arg("diff")
        .arg("--cached")
        .arg("--name-only")
        .current_dir(path)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(index.stdout).unwrap().trim(),
        "CODEGRAPH.md\ncomponents/support/CODEGRAPH.md"
    );
    assert!(
        std::fs::read_to_string(path.join("components/support/CODEGRAPH.md"))
            .unwrap()
            .contains("A repository with a validated docs destination.")
    );
    let input = json!({
        "repositoryId":repository, "views":{"kind":"Exact","viewIds":[evidence.payload["viewId"]]},
        "expectedRevision":evidence.payload["analysisRevision"], "outputRootId":root,
        "topics":["all"], "analysisRunId":evidence.payload["analysisRunId"],
    });
    let manual = accepted(submit(
        app,
        &bound,
        "repository_docs.generate",
        "manual-docs",
        input.clone(),
    ));
    let completed = terminal(f.store.as_ref(), &manual).await;
    assert_eq!(
        completed.state,
        OperationState::ObservedComplete,
        "{completed:?}"
    );
    assert_eq!(
        manual,
        accepted(submit(
            app,
            &bound,
            "repository_docs.generate",
            "manual-docs",
            input.clone()
        ))
    );
    let mut wrong = input.clone();
    wrong["outputRootId"] = json!("another-output-root");
    assert!(rejected(
        submit(
            app,
            &bound,
            "repository_docs.generate",
            "wrong-docs-root",
            wrong
        ),
        DomainErrorCode::PermissionDenied,
    ));
    assert_eq!(
        std::fs::read_to_string(path.join("CODEGRAPH.md")).unwrap(),
        document
    );

    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["update-index", "--assume-unchanged", "--", "CODEGRAPH.md"]);
    std::fs::write(
        path.join("CODEGRAPH.md"),
        "unrelated edit to the generated file\n",
    )
    .unwrap();
    let collision = submit(
        app,
        &bound,
        "repository_docs.generate",
        "doc-edit",
        input.clone(),
    );
    assert!(rejected(collision, DomainErrorCode::SourceChanged));
    assert_eq!(
        std::fs::read_to_string(path.join("CODEGRAPH.md")).unwrap(),
        "unrelated edit to the generated file\n"
    );
    assert!(
        read(f.store.as_ref(), |read| read.git().git_worktree(&tree.id))
            .unwrap()
            .pending_operation_id
            .is_none()
    );
    git(&[
        "update-index",
        "--no-assume-unchanged",
        "--",
        "CODEGRAPH.md",
    ]);
    std::fs::write(path.join("CODEGRAPH.md"), document).unwrap();
    std::fs::remove_file(path.join("local-user-notes.md")).unwrap();

    let mut delivery = input;
    delivery["delivery"] = json!("CommitAndPush");
    let generated = accepted(submit(
        app,
        &bound,
        "repository_docs.generate",
        "doc-delivery",
        delivery,
    ));
    assert_eq!(
        terminal(f.store.as_ref(), &generated).await.state,
        OperationState::ObservedComplete
    );
    assert_eq!(
        git(&["rev-parse", "HEAD"]),
        head.hex,
        "generation itself must not commit"
    );
    accept_documentation_review(f, app, user, project, "git.commit").await;
    accept_documentation_review(f, app, user, project, "git.push").await;
    let delivered = within(
        "the remote source receives reviewed repository docs",
        async {
            loop {
                let page = read(f.store.as_ref(), |read| {
                    read.history().page(&EventFilter::All, 0, 500)
                });
                if let Some(event) = page
                    .events
                    .into_iter()
                    .find(|event| event.kind == "RepositoryDocumentationPublished")
                {
                    break event;
                }
                f.h.advance(200);
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        },
    )
    .await;
    assert_eq!(
        delivered.payload["deliveredHead"],
        git(&["rev-parse", "HEAD"])
    );
    assert_eq!(delivered.payload["files"].as_array().unwrap().len(), 2);
    let evidence = read(f.store.as_ref(), |read| {
        read.sessions()
            .links(&RootId(task.clone()), &["evidence"], None, 25)
    })
    .into_iter()
    .find(|link| link.event_id == delivered.id.0 && link.target_kind == "repository_documentation")
    .expect("only verified publication supplies documentation acceptance evidence");
    let result = submit(
        app,
        user,
        "tasks.acceptance_evidence",
        "accept-doc-delivery",
        json!({
            "taskId": task, "expectedRevision":"1", "criteria":[{
                "id":"docs-delivered","text":"Publish complete repository documentation",
                "evidenceIds":[evidence.id],"state":"Met"
            }]
        }),
    );
    assert!(matches!(result, CommandReply::Recorded { receipt, .. }
        if matches!(receipt.decision, DomainDecision::Applied { .. })));
}

async fn accept_documentation_review(
    f: &Fx,
    app: &devworks_engine::Runtime,
    user: &InvocationContext,
    project: &devworks_core::protocol::ProjectId,
    name: &str,
) {
    let query = json!({"mode":"Query","protocol":"1.0-draft.1","schemaRevision":3,
        "queryId":"documentation-approvals","body":{"kind":"approvals.list",
            "scope":{"kind":"Project","projectId":project},"page":{"limit":25}}});
    let review = within(
        "documentation delivery enters the normal review queue",
        async {
            loop {
                let reply = app
                    .query(user, "approvals.list", query.to_string().as_bytes())
                    .unwrap();
                let view = serde_json::to_value(reply).unwrap();
                if let Some(found) = view["result"]["value"]["items"]
                    .as_array()
                    .and_then(|items| {
                        items.iter().find(|item| {
                            item["operationKind"] == name && item["state"] == "Pending"
                        })
                    })
                {
                    break found.clone();
                }
                f.h.advance(200);
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        },
    )
    .await;
    let vote = json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,
        "requestKey":format!("accept-docs-{}", name.replace('.', "-")),"body":{"kind":"approvals.record_human_decision",
            "proposalId":review["id"],"expectedRevision":review["revision"],"contentDigest":review["contentDigest"],
            "decision":"Approve","rationale":"Checked exact generated documentation contents and destination"}});
    let result = app
        .record_human_decision(user, vote.to_string().as_bytes())
        .unwrap();
    assert!(
        matches!(result, CommandReply::Recorded { receipt, .. }
        if matches!(receipt.decision, DomainDecision::Applied { .. })),
        "{name} human decision"
    );
}

fn rejected(
    reply: CommandReply<DomainDecision>,
    expected: devworks_core::protocol::DomainErrorCode,
) -> bool {
    match reply {
        CommandReply::Rejected { code, .. } => {
            matches!(code, devworks_core::protocol::Rejection::PermissionDenied)
                && expected == devworks_core::protocol::DomainErrorCode::PermissionDenied
        }
        CommandReply::Recorded { receipt, .. } => {
            matches!(receipt.decision, DomainDecision::Refused { fault }
            if fault.code == expected)
        }
        _ => false,
    }
}

/// A terminal router result may reach analysis after its computation lease is gone.
fn observe_after_source_lease_expiry(f: &Fx, mut run: StoredAnalysisRun, attempt: Id) {
    use devworks_core::services::graph::repository_analysis::{
        AnalysisModel, AnalysisModelInput, AnalysisService, Execution, ModelObservation,
        ProviderAnalysis,
    };

    struct Observed {
        request: Id,
        attempt: Id,
    }
    impl AnalysisModel for Observed {
        fn generate(&self, _: AnalysisModelInput) -> ModelObservation {
            panic!("recovery must not start a provider request");
        }
        fn observe(&self, _: &InvocationContext, request: &Id) -> ModelObservation {
            assert_eq!(&self.request, request);
            ModelObservation::Completed(ProviderAnalysis {
                text: String::from(r#"{"summary":"Recovered report.","confidence":"high"}"#),
                request: self.request.clone(),
                attempt: self.attempt.clone(),
                model: "fixture-model".into(),
            })
        }
    }

    let previous = run
        .phases
        .iter_mut()
        .find(|phase| phase.phase == AnalysisPhase::Projects)
        .unwrap();
    let subject = previous.subjects.first_mut().unwrap();
    let request = subject.request.clone();
    subject.output = None;
    subject.state = OperationState::OutcomeUnknown;
    previous.state = OperationState::OutcomeUnknown;
    previous.report = None;
    run.phases
        .retain(|phase| phase.phase == AnalysisPhase::Projects);
    run.id = id("analysis-delayed-run");
    run.active_operation = OperationId(id("analysis-delayed-operation"));
    run.operation = run.active_operation.clone();
    run.pin_id = id("released-analysis-pin");
    run.revision = 1;
    run.state = OperationState::OutcomeUnknown;
    let run_id = run.id.clone();
    let operation = write(f.store.as_ref(), move |tx| {
        tx.analysis().insert_analysis_run(&run)?;
        let op = tx.operations().record_intent(NewOperation {
            id: run.active_operation.clone(), root_id: None, assignment_id: None,
            capability: "analysis.reanalyze".into(), schema_version: 3, args_digest: run.intent.options_digest,
            payload: json!({"runId":run.id,"deliveryId":"analysis-original-delivery","authority":run.owner}),
        })?;
        let attempt = tx.operations().begin_attempt(&op.id, op.revision, AttemptBinding::default())?;
        let ready = tx.operations().operation(&op.id)?.unwrap();
        let sent = tx.operations().transition(&op.id, ready.revision, Some(&attempt.fence()),
            OperationState::Dispatching, None)?;
        tx.operations().transition(&op.id, sent.revision, Some(&attempt.fence()),
            OperationState::OutcomeUnknown, None)
    }).unwrap();
    let service = AnalysisService::new(f.store.clone(), f.artifacts.clone(), f.h.ids.clone(), None);
    assert!(service.install_model(Arc::new(Observed { request, attempt })));
    assert!(matches!(service.observe(&operation), Execution::Failed(_)));
    let recovered = read(f.store.as_ref(), |s| s.analysis().analysis_run(&run_id)).unwrap();
    assert_eq!(recovered.state, OperationState::Failed);
    assert_eq!(
        recovered.phases[0].diagnostic.as_deref(),
        Some("AnalysisCaptureExpired")
    );
    let subject = &recovered.phases[0].subjects[0];
    assert_eq!(subject.state, OperationState::ObservedComplete);
    assert_eq!(
        subject.output.as_ref().unwrap()["result"]["summary"],
        "Recovered report."
    );
}
