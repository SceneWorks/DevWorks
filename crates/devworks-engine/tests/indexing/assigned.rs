//! The assigned agent uses the registered native graph, indexing, and Git routes.
use super::*;
use devworks_core::ports::{
    AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization, Clock,
};
use devworks_core::protocol::{
    AssignmentId, DomainErrorCode, Id, OperationId, OperationState, ProjectId, Rejection, RootId,
    data::GraphResult,
};
use devworks_core::services::{
    git::{GitConfig, RegisteredGitRoot},
    indexing::{IndexCoordinator, section_config, section_key},
};
use devworks_engine::{git::GitComposition, providers::schemas::RegisteredSchemas};
use devworks_test_support::seed::{ProjectSeed, WorkSeed, rows};

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
            store: &*self.store,
            clock: &*self.clock,
            protected_branches: &[],
        }
        .authorize(ctx, request)
    }
}

#[derive(Default)]
struct CleanupHost {
    on_staging: std::sync::Mutex<Option<StageEdit>>,
}

enum StageEdit {
    Ignored,
    Replacement(PathBuf),
}

impl devworks_core::ports::process::ProcessHost for CleanupHost {
    fn platform(&self) -> &'static str {
        NativeProcessHost.platform()
    }
    fn environment(&self) -> Vec<(String, String)> {
        NativeProcessHost.environment()
    }
    fn current_identity(&self) -> Option<devworks_core::ports::process::NativeIdentity> {
        NativeProcessHost.current_identity()
    }
    fn probe(
        &self,
        identity: &devworks_core::ports::process::NativeIdentity,
    ) -> devworks_core::ports::process::Probe {
        NativeProcessHost.probe(identity)
    }
    fn terminate(
        &self,
        identity: &devworks_core::ports::process::NativeIdentity,
    ) -> devworks_core::ports::process::Probe {
        NativeProcessHost.terminate(identity)
    }
    fn spawn(
        &self,
        spec: &devworks_core::ports::process::ProcessSpec,
    ) -> Result<
        Box<dyn devworks_core::ports::process::ProcessTree>,
        devworks_core::ports::process::ProcessError,
    > {
        if let Some(path) = spec.cwd.as_ref().filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".devworks-cleanup-"))
        }) && let Some(edit) = self.on_staging.lock().unwrap().take()
        {
            std::fs::write(path.join("private.tmp"), "late staged data").unwrap();
            if let StageEdit::Replacement(original) = edit {
                std::fs::create_dir(&original).unwrap();
                std::fs::write(original.join("untouched.txt"), "unrelated replacement").unwrap();
            }
        }
        NativeProcessHost.spawn(spec)
    }
}

fn operation(reply: CommandReply<DomainDecision>) -> OperationId {
    match reply {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

async fn settled(store: &dyn ApplicationStore, id: &OperationId) -> OperationRecord {
    let result = tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            let operation = read(store, |s| s.operations().operation(id)).unwrap();
            if !matches!(
                operation.state,
                OperationState::IntentRecorded
                    | OperationState::Preflighted
                    | OperationState::Dispatching
            ) {
                return operation;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    result.unwrap_or_else(|_| {
        let operation = read(store, |s| s.operations().operation(id)).unwrap();
        let delivery = read(store, |s| {
            s.queue().deliveries(
                &DeliveryFilter {
                    consumer: None,
                    operation_id: Some(id.clone()),
                },
                None,
                10,
            )
        });
        panic!("Git effect did not settle: {operation:?}; {delivery:?}");
    })
}

#[test]
fn assigned_native_graph_refreshes_and_cleanup_retires_exact_worktree() {
    let _case = native_runtime_case();
    let h = Harness::new();
    let executor = runtime();
    let store = h.open();
    let src = sources();
    let repo = src.join("main");
    let owned = src.join("owned");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(&owned).unwrap();
    let baseline = "pub fn baseline() {}\n";
    std::fs::write(repo.join("src/lib.rs"), baseline).unwrap();
    std::fs::write(repo.join(".gitignore"), "private.tmp\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "baseline"]);
    let mut composed = composition(&h, store.clone(), "git");
    let process_host = Arc::new(CleanupHost::default());
    composed.process_host = process_host.clone();
    composed.git = Some(GitComposition {
        config: GitConfig {
            roots: vec![RegisteredGitRoot {
                id: id("owned-root"),
                path: std::fs::canonicalize(&owned).unwrap(),
            }],
            filesystem: Some(Arc::new(NativeFilesystem::new())),
            ..GitConfig::default()
        },
        github_http: None,
    });
    composed.authority = Arc::new(NativeAuthority {
        store: store.clone(),
        clock: h.clock(),
    });
    let app = Arc::new(executor.block_on(boot(composed)).unwrap());
    let human = principal(&*store, "user", PrincipalKind::Human);
    let worker = principal(&*store, "worker", PrincipalKind::Agent);
    base_directory(&store, &human, &src);
    let repository = id(&discovered(&app, &human).remove(0));
    let run = index(&app, &human, repository.as_str(), "assignment-baseline");
    executor.block_on(within("assignment baseline publishes", async {
        loop {
            let state = run_state(&store, &run);
            if state.state == RUN_SUCCEEDED {
                break;
            }
            assert!(!run_terminal(&state.state), "{state:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }));
    let baseline_view = run_state(&store, &run)
        .output_view_id
        .expect("published baseline");
    let head = read(&*store, |s| {
        Ok(s.indexing()
            .worktrees(&repository)?
            .into_iter()
            .find(|t| t.main)
            .unwrap()
            .head_oid
            .unwrap())
    });
    rows(
        &*store,
        &[ProjectSeed::new("project")],
        &[
            WorkSeed::issue("task", "project"),
            WorkSeed::issue("sibling", "project"),
        ],
    )
    .unwrap();
    let over_store = store.clone();
    let host = human.clone();
    let worker_actor = worker.principal().clone();
    let repo_id = repository.clone();
    let project = ProjectId(id("project"));
    let root = RootId(id("task"));
    let assignment = AssignmentId(id("assignment"));
    let session = id("session");
    write(&*over_store, {
        let (project, root, assignment, session) = (
            project.clone(),
            root.clone(),
            assignment.clone(),
            session.clone(),
        );
        move |tx| {
            tx.projects()
                .set_members(&project, std::slice::from_ref(&repo_id))?;
            tx.work()
                .set_work_repositories(&root.0, std::slice::from_ref(&repo_id))?;
            tx.work()
                .set_work_repositories(&id("sibling"), std::slice::from_ref(&repo_id))?;
            devworks_core::authority::service::bind_principal(
                tx,
                &host,
                NewPrincipalBinding {
                    actor: worker_actor.clone(),
                    kind: PrincipalKind::Agent,
                    role: Some(devworks_core::settings::catalog::AgentRole::Worker),
                    project_id: Some(project.clone()),
                    tool_families: None,
                    bound_by: host.principal().clone(),
                },
                None,
            )
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))?;
            let now = tx.now_ms();
            tx.sessions().insert_session(&StoredSession {
                id: session.clone(),
                conversation_id: id("conversation"),
                agent_id: worker_actor,
                project_id: Some(project.clone()),
                root_id: Some(root.clone()),
                title: "Assigned checkout".into(),
                context_epoch: 0,
                revision: 1,
                created_by: host.principal().clone(),
                created_at_ms: now,
                closed_at_ms: None,
                closed_by: None,
                next_sequence: 1,
                checkpoint_sequence: 0,
            })?;
            tx.runs().insert_assignment(&StoredAssignment {
                purpose: AssignmentPurpose::Primary,
                id: assignment.clone(),
                root_id: root.clone(),
                work_id: root,
                parent_id: None,
                session_id: session,
                execution_id: None,
                state: AssignmentState::Active,
                generation: 1,
                slot_held: false,
                revision: 1,
                checkpoint: None,
                scope_key: None,
                scope_digest: None,
                plan_revision: None,
                instruction_revision: 0,
                created_at_ms: now,
            })?;
            tx.runs()
                .insert_agent_assignment_context(&StoredAgentAssignmentContext {
                    assignment_id: assignment,
                    project_id: project,
                    role_id: id("worker"),
                    role_instructions: String::new(),
                    role_revisions: json!({}),
                    brief: "Work on the assigned checkout".into(),
                    task_ids: json!(["task"]),
                    source_checkpoint_id: None,
                    created_at_ms: now,
                })
        }
    })
    .unwrap();
    let worker = worker
        .with_lineage(Some(project.clone()), Some(root.clone()), Some(assignment))
        .with_session(session);
    let sibling = human
        .clone()
        .with_lineage(Some(project), Some(RootId(id("sibling"))), None);
    let git_command = |ctx: &InvocationContext, selector: &str, key: &str, mut body: Value| {
        body["kind"] = json!(selector);
        let frame = json!({"mode":"Command","protocol":"1.0-draft.1","requestKey":key,"body":body});
        app.command(ctx, selector, frame.to_string().as_bytes())
            .unwrap()
    };
    let create = |ctx: &InvocationContext, work: &str, key: &str| {
        let op = operation(git_command(
            ctx,
            "git.worktree",
            key,
            json!({
                "repositoryId":repository,"workId":work,"expectedRevision":"1",
                "action":"Create","base":{"format":"Sha1","hex":head},"registeredRootId":"owned-root"
            }),
        ));
        let done = executor.block_on(settled(&*store, &op));
        assert_eq!(done.state, OperationState::ObservedComplete, "{done:?}");
        read(&*store, |s| s.git().git_worktrees(&id(work), None, 10)).remove(0)
    };
    let tree = create(&worker, "task", "create-owned");
    let sibling_tree = create(&sibling, "sibling", "create-sibling");
    assert!(
        worker.worktree().is_none(),
        "launch context has no caller-supplied checkout"
    );
    let indexed = |tree: &StoredGitWorktree| {
        let path = devworks_core::ports::indexing::SourceTree::canonical_dir(
            &NativeSourceTree::new(),
            Path::new(&tree.path),
        )
        .unwrap();
        read(&*store, |s| {
            Ok(s.indexing()
                .worktrees(&repository)?
                .into_iter()
                .find(|row| Path::new(&row.path) == path)
                .unwrap()
                .worktree_id)
        })
    };
    let own_worktree = indexed(&tree);
    let sibling_worktree = indexed(&sibling_tree);
    let revision = read(&*store, |s| {
        section_config(s.indexing(), &section_key(None))
    })
    .revision;
    let indexing_command = |ctx: &InvocationContext, selector: &str, key: &str, command: Value| {
        let f =
            json!({"protocol":"1.0-draft.1","schemaRevision":2,"requestKey":key,"command":command});
        app.command(ctx, selector, f.to_string().as_bytes())
            .unwrap()
    };
    let refresh = |ctx: &InvocationContext, key: &str, target: &Id| {
        indexing_command(
            ctx,
            "indexing.refresh_overlay",
            key,
            json!({
                "kind":"RefreshOverlay","repositoryId":repository,"worktreeId":target,
                "expectedSourceRevision":revision.to_string()
            }),
        )
    };
    let assert_refresh = |reply: CommandReply<DomainDecision>| {
        let CommandReply::Recorded { receipt, .. } = reply else {
            panic!("{reply:?}")
        };
        let (DomainDecision::Applied {
            result: OperationResult::IndexingRefreshOverlay { value },
        }
        | DomainDecision::NoChange {
            result: OperationResult::IndexingRefreshOverlay { value },
        }) = receipt.decision
        else {
            panic!("{:?}", receipt.decision)
        };
        executor.block_on(within("explicit overlay refresh publishes", async {
            loop {
                let current = run_state(&store, value.id.as_str());
                if current.state == RUN_SUCCEEDED {
                    break;
                }
                assert!(!run_terminal(&current.state), "{current:?}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }));
    };
    std::fs::write(
        Path::new(&sibling_tree.path).join("src/lib.rs"),
        "pub fn sibling_only() {}\n",
    )
    .unwrap();
    assert_refresh(refresh(&human, "sibling-refresh", &sibling_worktree));

    let status = |ctx: &InvocationContext, id: Option<&Id>| {
        let mut f = json!({"mode":"Query","protocol":"1.0-draft.1","schemaRevision":2,"queryId":"status",
            "body":{"kind":"indexing.status","repositoryId":repository}});
        if let Some(id) = id {
            f["body"] = json!({"kind":"indexing.status","overlayId":id});
        }
        app.query(ctx, "indexing.status", f.to_string().as_bytes())
            .unwrap()
    };
    let overlays = |reply| {
        let DomainQueryReply::Result {
            result: OperationResult::IndexingStatus { value },
            ..
        } = reply
        else {
            panic!("{reply:?}")
        };
        value
    };
    let sibling_overlay = overlays(status(&human, None))
        .overlays
        .unwrap()
        .0
        .into_iter()
        .find(|o| o.worktree_id.as_ref() == Some(&sibling_worktree))
        .unwrap();
    assert!(matches!(
        sibling_overlay.state,
        devworks_core::protocol::records::OverlayState::Ready
    ));
    assert!(
        overlays(status(&worker, None))
            .overlays
            .unwrap()
            .0
            .is_empty()
    );

    let exact_sibling = json!({
        "protocol":"1.0-draft.1","schemaRevision":3,"queryId":"exact-sibling",
        "scope":{"kind":"Project","projectId":"project"},
        "view":{"kind":"Exact","viewIds":[sibling_overlay.view_id.as_ref().unwrap()]},
        "query":{"kind":"Search","selection":{"mode":"NodeList","text":"sibling_only","repositoryId":repository}},
        "limits":{"maxResults":100,"maxVisited":100000,"deadlineMs":30000}
    });
    let denied = app
        .query(&worker, "code.search", exact_sibling.to_string().as_bytes())
        .unwrap();
    assert!(
        matches!(&denied, DomainQueryReply::Rejected { fault, .. } if fault.code == DomainErrorCode::PermissionDenied),
        "{denied:?}"
    );
    let checkout = Path::new(&tree.path);
    let edit = checkout.join("src/lib.rs");
    let graph_search = |name: &str| {
        json!({"protocol":"1.0-draft.1","schemaRevision":3,"queryId":name,"scope":{"kind":"Project","projectId":"project"},"current":true,
            "query":{"kind":"Search","selection":{"mode":"NodeList","text":name,"repositoryId":repository}},
            "limits":{"maxResults":100,"maxVisited":100000,"deadlineMs":30000}})
    };
    let run_search = |ctx: &InvocationContext, frame: Value| {
        let (runtime, context) = (app.clone(), ctx.clone());
        executor.block_on(async {
            tokio::task::spawn_blocking(move || {
                runtime
                    .query(&context, "code.search", frame.to_string().as_bytes())
                    .unwrap()
            })
            .await
            .unwrap()
        })
    };
    let parked = checkout.with_extension("fixture-parked");
    std::fs::rename(checkout, &parked).unwrap();
    std::fs::create_dir_all(checkout.join("src")).unwrap();
    std::fs::copy(parked.join(".git"), checkout.join(".git")).unwrap();
    std::fs::write(&edit, "pub fn replaced_directory() {}\n").unwrap();
    let replaced = run_search(&worker, graph_search("replaced_directory"));
    assert!(
        matches!(&replaced, DomainQueryReply::Rejected { fault, .. } if fault.code == DomainErrorCode::ViewUnavailable),
        "{replaced:?}"
    );
    let replaced = refresh(&worker, "replaced-checkout-refresh", &tree.id);
    assert!(
        matches!(&replaced, CommandReply::Recorded { receipt, .. }
            if matches!(&receipt.decision, DomainDecision::Refused { fault } if fault.code == DomainErrorCode::ViewUnavailable)
        ),
        "{replaced:?}"
    );
    std::fs::remove_dir_all(checkout).unwrap();
    std::fs::rename(parked, checkout).unwrap();

    git(&repo, &["config", "filter.capture-proof.clean", "exit 41"]);
    let attributes = checkout.join(".gitattributes");
    std::fs::write(&attributes, "src/lib.rs filter=capture-proof\n").unwrap();
    std::fs::write(&edit, "pub fn should_not_capture() {}\n").unwrap();
    let refused_capture = run_search(&worker, graph_search("should_not_capture"));
    assert!(
        matches!(&refused_capture, DomainQueryReply::Rejected { fault, .. } if fault.code == DomainErrorCode::ViewUnavailable),
        "{refused_capture:?}"
    );
    let mut published = graph_search("baseline");
    published.as_object_mut().unwrap().remove("current");
    published["view"] = json!({"kind":"Published","repositoryIds":[repository]});
    let published = run_search(&worker, published);
    assert!(
        matches!(&published, DomainQueryReply::Result { .. }),
        "{published:?}"
    );
    std::fs::remove_file(attributes).unwrap();
    git(&repo, &["config", "--unset", "filter.capture-proof.clean"]);

    let mut last = None;
    let mut historical = None;
    let mut historical_exact = None;
    for name in ["first_unsaved_symbol", "second_unsaved_symbol"] {
        std::fs::write(&edit, format!("pub fn {name}() {{}}\n")).unwrap();
        let answer = run_search(&worker, graph_search(name));
        let DomainQueryReply::Result {
            result: OperationResult::CodeSearch { value },
            ..
        } = answer
        else {
            panic!("{answer:?}")
        };
        let view = value.view.unwrap();
        assert_eq!(
            view.views[0].snapshot.worktree_id.as_ref(),
            Some(&own_worktree)
        );
        let GraphResult::CodeSearch { value } = value.result else {
            panic!("search result")
        };
        assert!(value.items.0.iter().any(|item| matches!(
            item, devworks_core::protocol::records::CodeSearchHit::Node { node } if node.name.0 == name
        )), "{value:?}");
        if let Some((prior, _)) = &last {
            assert_ne!(&view.views[0].view_id, prior);
        } else {
            let mut exact = graph_search(name);
            exact.as_object_mut().unwrap().remove("current");
            exact["view"] = json!({"kind":"Exact","viewIds":[view.views[0].view_id]});
            historical_exact = Some((view.views[0].view_id.clone(), exact.clone()));
            let answer = run_search(&human, exact);
            let DomainQueryReply::Result {
                result: OperationResult::CodeSearch { value },
                ..
            } = answer
            else {
                panic!("{answer:?}")
            };
            let mut retained = graph_search(name);
            retained.as_object_mut().unwrap().remove("current");
            retained["view"] = json!({"kind":"Pinned","pinId":value.view.unwrap().pin_id});
            historical = Some(retained);
        }
        last = Some((view.views[0].view_id.clone(), view.pin_id));
    }
    let historical = historical.unwrap();
    let before_removal = run_search(&human, historical.clone());
    assert!(
        matches!(before_removal, DomainQueryReply::Result { .. }),
        "{before_removal:?}"
    );
    let (own_view, _) = last.unwrap();
    let own = overlays(status(&worker, None));
    assert_eq!(own.current_view_ids.0, vec![own_view.clone()]);
    let own_overlay = own.overlays.unwrap().0.remove(0);
    assert_eq!(own_overlay.worktree_id.as_ref(), Some(&own_worktree));
    assert_eq!(
        overlays(status(&worker, Some(&own_overlay.overlay_id)))
            .overlays
            .unwrap()
            .0
            .len(),
        1
    );
    let retire = json!({"mode":"Command","protocol":"1.0-draft.1","requestKey":"retire-retained",
        "body":{"kind":"indexing.retire_view","viewId":own_overlay.overlay_id,"expectedRevision":own_overlay.revision}});
    let retired = app
        .command(
            &human,
            "indexing.retire_view",
            retire.to_string().as_bytes(),
        )
        .unwrap();
    assert!(
        matches!(&retired, CommandReply::Recorded { receipt, .. }
        if matches!(receipt.decision, DomainDecision::Applied { .. })),
        "{retired:?}"
    );
    assert_refresh(refresh(&worker, "reactivate-retired-overlay", &tree.id));
    let recovered = run_search(&worker, graph_search("second_unsaved_symbol"));
    let DomainQueryReply::Result {
        result: OperationResult::CodeSearch { value },
        ..
    } = recovered
    else {
        panic!("{recovered:?}")
    };
    let recovered = value.view.unwrap();
    assert_ne!(recovered.views[0].view_id, own_view);
    let (own_view, pin) = (recovered.views[0].view_id.clone(), recovered.pin_id);
    let (historical_view, historical_exact) = historical_exact.unwrap();
    read(&*store, |s| {
        assert!(s.indexing().view(&historical_view)?.is_some());
        Ok(())
    });
    let exact_after_reactivation = run_search(&worker, historical_exact);
    assert!(
        matches!(&exact_after_reactivation, DomainQueryReply::Rejected { fault, .. }
        if fault.code == DomainErrorCode::ViewUnavailable),
        "{exact_after_reactivation:?}"
    );
    let after_reactivation = run_search(&human, historical.clone());
    assert!(
        matches!(&after_reactivation, DomainQueryReply::Rejected { fault, .. }
        if fault.code == DomainErrorCode::ViewUnavailable),
        "{after_reactivation:?}"
    );

    let stale = worker.clone().with_session(id("stale-session"));
    let stale_status = status(&stale, None);
    assert!(
        matches!(&stale_status, DomainQueryReply::Rejected { fault, .. } if fault.code == DomainErrorCode::PermissionDenied),
        "{stale_status:?}"
    );

    let cases = UseCases::new().with_indexing(Arc::new(IndexCoordinator::new(
        app.indexing().unwrap().env().clone(),
    )));
    let inventory = RegisteredSchemas::new(store.clone(), cases, None, app.artifacts().clone());
    let selected = inventory.selected_builtins(&worker).unwrap();
    let mut names: Vec<_> = selected
        .iter()
        .map(|schema| schema.capability_id.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "devworks-command/indexing.refresh_overlay",
            "devworks-query/indexing.status"
        ]
    );
    for tool in &selected {
        use devworks_core::providers::materialize::SchemaDocuments;
        assert!(inventory.tool(&worker, tool).is_ok());
    }
    assert_refresh(refresh(&worker, "owned-explicit-refresh", &tree.id));

    let refused = |reply| {
        assert!(
            matches!(
                &reply,
                CommandReply::Rejected {
                    code: Rejection::PermissionDenied,
                    ..
                }
            ),
            "{reply:?}"
        )
    };
    for (target, key) in [
        (&sibling_worktree, "sibling-index-id"),
        (&sibling_tree.id, "sibling-git-id"),
    ] {
        refused(refresh(&worker, key, target));
    }
    refused(indexing_command(
        &worker,
        "indexing.refresh_overlay",
        "published-ref-denied",
        json!({
            "kind":"RefreshOverlay","repositoryId":repository,"ref":"refs/heads/main",
            "expectedSourceRevision":revision.to_string()
        }),
    ));
    for (selector, query) in [
        (
            "indexing.status",
            json!({"overlayId":sibling_overlay.overlay_id}),
        ),
        (
            "indexing.status",
            json!({"repositoryId":"other-repository"}),
        ),
        (
            "indexing.discover",
            json!({"scope":{"kind":"Workspace"},"roots":[],"page":{"limit":50}}),
        ),
    ] {
        let mut body = query;
        body["kind"] = json!(selector);
        let mut frame = json!({"mode":"Query","protocol":"1.0-draft.1","schemaRevision":2,"queryId":"forbidden","body":body});
        if selector == "indexing.discover" {
            frame.as_object_mut().unwrap().remove("schemaRevision");
        }
        let reply = app
            .query(&worker, selector, frame.to_string().as_bytes())
            .unwrap();
        assert!(
            matches!(&reply, DomainQueryReply::Rejected { fault, .. } if fault.code == DomainErrorCode::PermissionDenied),
            "{selector}: {reply:?}"
        );
    }

    let cleanup_from = |ctx: &InvocationContext, target: &StoredGitWorktree, key: &str| {
        let frame = json!({"protocol":"1.0-draft.1","requestKey":key,"command":{
            "kind":"Cleanup","worktreeId":target.id,
            "expectedOwnershipRevision":target.ownership_revision.to_string(),"expectedHead":target.head
        }});
        app.command(ctx, "git.cleanup", frame.to_string().as_bytes())
            .unwrap()
    };
    let cleanup = |target: &StoredGitWorktree, key: &str| cleanup_from(&worker, target, key);
    let sibling_removed = cleanup(&sibling_tree, "sibling-cleanup");
    assert!(
        matches!(&sibling_removed,
        CommandReply::Recorded { receipt, .. } if matches!(receipt.decision, DomainDecision::Refused { .. })),
        "{sibling_removed:?}"
    );
    let dirty = operation(cleanup(&tree, "dirty-cleanup"));
    let done = executor.block_on(settled(&*store, &dirty));
    assert_eq!(done.state, OperationState::Failed, "{done:?}");
    assert!(edit.exists());
    assert_eq!(
        overlays(status(&worker, None)).overlays.unwrap().0[0].overlay_id,
        own_overlay.overlay_id
    );
    std::fs::write(&edit, baseline).unwrap();
    let ignored = Path::new(&tree.path).join("private.tmp");
    std::fs::write(&ignored, "local data").unwrap();
    let dirty = operation(cleanup(&tree, "ignored-cleanup"));
    let done = executor.block_on(settled(&*store, &dirty));
    assert_eq!(done.state, OperationState::Failed, "{done:?}");
    assert_eq!(std::fs::read_to_string(&ignored).unwrap(), "local data");
    std::fs::remove_file(&ignored).unwrap();
    for (set, clear, key) in [
        (
            "--assume-unchanged",
            "--no-assume-unchanged",
            "assume-unchanged",
        ),
        ("--skip-worktree", "--no-skip-worktree", "skip-worktree"),
    ] {
        let checkout = Path::new(&tree.path);
        git(checkout, &["update-index", set, "--", "src/lib.rs"]);
        let local = format!("pub fn hidden_{}() {{}}\n", key.replace('-', "_"));
        std::fs::write(&edit, &local).unwrap();
        let refused = operation(cleanup(&tree, key));
        let done = executor.block_on(settled(&*store, &refused));
        assert_eq!(done.state, OperationState::Failed, "{done:?}");
        assert_eq!(std::fs::read_to_string(&edit).unwrap(), local);
        git(checkout, &["update-index", clear, "--", "src/lib.rs"]);
        std::fs::write(&edit, baseline).unwrap();
    }
    git(checkout, &["checkout", "--detach", &tree.head.hex]);
    let detached = operation(cleanup(&tree, "detached-head"));
    let done = executor.block_on(settled(&*store, &detached));
    assert_eq!(done.state, OperationState::Failed, "{done:?}");
    assert!(checkout.is_dir());
    git(
        checkout,
        &["checkout", tree.branch.strip_prefix("refs/heads/").unwrap()],
    );

    let path = checkout.to_str().unwrap();
    git(&repo, &["worktree", "lock", "--", path]);
    let locked = operation(cleanup(&tree, "locked-worktree"));
    let done = executor.block_on(settled(&*store, &locked));
    assert_eq!(done.state, OperationState::Failed, "{done:?}");
    assert!(checkout.is_dir());
    git(&repo, &["worktree", "unlock", "--", path]);

    *process_host.on_staging.lock().unwrap() = Some(StageEdit::Ignored);
    let staged_dirty = operation(cleanup(&tree, "staged-ignored"));
    let done = executor.block_on(settled(&*store, &staged_dirty));
    assert_eq!(done.state, OperationState::Failed, "{done:?}");
    assert_eq!(
        std::fs::read_to_string(&ignored).unwrap_or_else(|error| panic!("{error}: {done:?}")),
        "late staged data"
    );
    std::fs::remove_file(&ignored).unwrap();

    let cleaned = operation(cleanup(&tree, "cleaned"));
    let done = executor.block_on(settled(&*store, &cleaned));
    assert_eq!(done.state, OperationState::ObservedComplete, "{done:?}");
    let retry = cleanup(&tree, "cleaned");
    assert!(
        matches!(&retry,
            CommandReply::Recorded { receipt, replayed: true, .. }
                if matches!(&receipt.decision, DomainDecision::Accepted { operation_id } if operation_id == &cleaned)
        ),
        "{retry:?}"
    );
    assert!(!Path::new(&tree.path).exists());
    assert!(inventory.selected_builtins(&worker).unwrap().is_empty());
    let current = run_search(&worker, graph_search("baseline"));
    let DomainQueryReply::Result {
        result: OperationResult::CodeSearch { value },
        ..
    } = current
    else {
        panic!("{current:?}")
    };
    assert!(value.view.unwrap().views[0].snapshot.worktree_id.is_none());
    let retired = run_search(&human, historical);
    assert!(
        matches!(&retired, DomainQueryReply::Rejected { fault, .. } if fault.code == DomainErrorCode::ViewUnavailable),
        "{retired:?}"
    );
    let missing = status(&worker, None);
    assert!(
        matches!(&missing, DomainQueryReply::Rejected { fault, .. } if fault.code == DomainErrorCode::PermissionDenied),
        "{missing:?}"
    );
    git(&repo, &["show-ref", "--verify", &tree.branch]);
    assert!(Path::new(&sibling_tree.path).is_dir());
    assert!(matches!(
        overlays(status(&human, Some(&sibling_overlay.overlay_id)))
            .overlays
            .unwrap()
            .0[0]
            .state,
        devworks_core::protocol::records::OverlayState::Ready
    ));
    read(&*store, |s| {
        let tree = s.git().git_worktree(&tree.id)?.unwrap();
        assert!(tree.removed);
        assert_eq!(tree.last_operation_id, Some(cleaned));
        let row = s.indexing().overlay(&own_overlay.overlay_id)?.unwrap();
        assert_eq!(row.state, OVERLAY_UNAVAILABLE);
        assert!(row.view_id.is_none());
        assert!(s.indexing().view(&own_view)?.is_none());
        assert!(s.graph().graph_pin(&pin)?.is_none());
        assert!(s.indexing().view(&baseline_view)?.is_some());
        Ok(())
    });
    let f = json!({"protocol":"1.0-draft.1","schemaRevision":3,"queryId":"retired-pin","scope":{"kind":"Project","projectId":"project"},
        "view":{"kind":"Pinned","pinId":pin},"query":{"kind":"Source","selection":{"kind":"File","repositoryId":repository,"path":"src/lib.rs","startLine":"0","endLine":"0"}},
        "limits":{"maxResults":100,"maxVisited":100000,"deadlineMs":30000}});
    let retired = app
        .query(&worker, "code.source", f.to_string().as_bytes())
        .unwrap();
    assert!(
        matches!(&retired, DomainQueryReply::Rejected { fault, .. } if matches!(
            fault.code, DomainErrorCode::ViewUnavailable | DomainErrorCode::PermissionDenied
        )),
        "{retired:?}"
    );
    let sibling_checkout = PathBuf::from(&sibling_tree.path);
    std::fs::write(sibling_checkout.join("src/lib.rs"), baseline).unwrap();
    *process_host.on_staging.lock().unwrap() =
        Some(StageEdit::Replacement(sibling_checkout.clone()));
    let staged = operation(cleanup_from(
        &sibling,
        &sibling_tree,
        "replacement-during-cleanup",
    ));
    let unknown = executor.block_on(settled(&*store, &staged));
    assert_eq!(unknown.state, OperationState::OutcomeUnknown, "{unknown:?}");
    let digest = devworks_core::fingerprint::sha256_hex(staged.0.as_str().as_bytes());
    let recovery = sibling_checkout.with_file_name(format!(".devworks-cleanup-{}", &digest[..24]));
    assert_eq!(
        std::fs::read_to_string(recovery.join("private.tmp")).unwrap(),
        "late staged data"
    );
    assert_eq!(
        std::fs::read_to_string(sibling_checkout.join("untouched.txt")).unwrap(),
        "unrelated replacement"
    );
    read(&*store, |s| {
        let row = s.git().git_worktree(&sibling_tree.id)?.unwrap();
        assert!(!row.removed);
        assert_eq!(row.pending_operation_id.as_ref(), Some(&staged));
        Ok(())
    });
    executor.block_on(app.shutdown());
    std::fs::remove_dir_all(src).unwrap();
}
