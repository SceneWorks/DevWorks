//! Native Git plus the real agent scheduler. Model transport is scripted.
use super::*;
use devworks_core::services::runs::root_assignments;
use std::path::Path;
use std::process::Command;

struct CodeReviewWire {
    reviews: Mutex<Vec<Value>>,
    requests: Mutex<Vec<Value>>,
}
impl StreamingHttp for CodeReviewWire {
    fn open(
        &self,
        request: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        let payload: Value = serde_json::from_slice(request.body.as_ref().unwrap()).unwrap();
        let rendered = payload.to_string();
        self.requests.lock().unwrap().push(payload.clone());
        let reviewed = rendered.contains("+fn saved_review_source() {}");
        if reviewed {
            assert!(
                rendered.contains("Independently review these exact Git diffs"),
                "{rendered}"
            );
            assert!(!rendered.contains("private coordination instruction"));
            assert!(
                payload["tools"].as_array().is_none_or(Vec::is_empty),
                "reviewer is read only"
            );
            self.reviews.lock().unwrap().push(payload);
        }
        let content = if reviewed {
            r#"{"findings":[]}"#
        } else {
            "done"
        };
        let frame =
            json!({"choices":[{"index":0,"delta":{"content":content},"finish_reason":"stop"}]});
        Ok(Box::new(AgentStream {
            head: ResponseHead {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
            },
            body: Some(format!("data: {frame}\n\ndata: [DONE]\n\n").into_bytes()),
        }))
    }
}

pub(super) fn git(path: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=Review fixture",
            "-c",
            "user.email=review@example.test",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "Git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[test]
fn local_code_review_runs_once_over_the_saved_diff_and_satisfies_exact_readiness() {
    let _case = native_runtime_case();
    let fx = Fx::new();
    fx.scope.approve(&rid("root-f"), 1, &[]);
    runtime().block_on(async {
        let source = fx.h.root.join("exact-review-repository");
        let worktrees = fx.h.root.join("exact-review-worktrees");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&worktrees).unwrap();
        std::fs::write(source.join("AGENTS.md"), "private coordination instruction\n").unwrap();
        git(&source, &["init", "-qb", "main"]);
        std::fs::write(source.join("lib.rs"), "fn prior() {}\n").unwrap();
        git(&source, &["add", "lib.rs"]);
        git(&source, &["commit", "-qm", "Base"]);
        let base = git(&source, &["rev-parse", "HEAD"]);
        let repo = id("review-git-repository");
        let project = ProjectId(id("project-a"));
        let location = worktrees.join("task");
        let branch = "devworks/task-review-fixture";
        git(&source, &["worktree", "add", "-qb", branch, location.to_str().unwrap(), &base]);
        std::fs::write(location.join("lib.rs"), "fn saved_review_source() {}\n").unwrap();
        git(&location, &["add", "lib.rs"]);
        git(&location, &["commit", "-qm", "Exact task change"]);
        let head = git(&location, &["rev-parse", "HEAD"]);
        let fs = NativeFilesystem::new();
        let identity = |path: &str| {
            use devworks_core::authority::destructive::FilesystemResolver;
            fs.resolve(path, None).unwrap().target.unwrap().identity.unwrap()
        };
        let path = std::fs::canonicalize(&location).unwrap().to_string_lossy().into_owned();
        let tree = StoredGitWorktree {
            id: id("saved-review-worktree"),
            repository_id: repo.clone(),
            work_id: id("task-f1"),
            owner: fx.user.principal().clone(),
            ownership_revision: 1,
            owner_root_id: Some(rid("task-f1")), owner_assignment_id: None,
            registered_root_id: id("review-root"),
            path: path.clone(),
            branch: format!("refs/heads/{branch}"),
            head: devworks_core::protocol::data::GitOid {
                format: devworks_core::protocol::data::ObjectFormat::Sha1, hex: head.clone(),
            },
            revision: 1,
            removed: false,
            location: Some(GitWorktreeLocation {
                root: identity(&path),
                git_dir: identity(&git(&location, &["rev-parse", "--absolute-git-dir"])),
                common_dir: identity(&git(&location, &["rev-parse", "--path-format=absolute", "--git-common-dir"])),
            }),
            pending_operation_id: None, last_operation_id: None, last_result: None,
        };
        write(&*fx.store, {
            let (repo, project, source, tree) = (repo.clone(), project.clone(), source.clone(), tree.clone());
            move |tx| {
              let now = tx.now_ms();
              tx.projects().insert_repository(&StoredRepository {
                id: repo.clone(), location: std::fs::canonicalize(&source).unwrap().to_string_lossy().into_owned(),
                name:"Review fixture".into(), remote_url:None, primary:true, lifecycle:"Active".into(),
                languages:vec!["rust".into()], revision:1, registered_at_ms:now,
              })?;
              tx.projects().set_members(&project, &[repo.clone()])?;
              tx.work().set_work_repositories(&id("root-f"), &[repo.clone()])?;
              tx.work().set_work_repositories(&id("task-f1"), &[repo])?;
              tx.git().insert_git_worktree(&tree)
            }
        }).unwrap();
        let wire = Arc::new(CodeReviewWire { reviews: Mutex::new(Vec::new()), requests: Mutex::default() });
        let mut composed = composition_with(
            &fx, None, Arc::new(FakePower::default()),
            Arc::new(HostAuthorityForTest { store: fx.store.clone(), clock: fx.h.clock() }),
            Arc::new(devworks_adapters::process::NativeProcessHost),
        );
        composed.git = Some(devworks_engine::git::GitComposition {
            github_http: None,
            config: devworks_core::services::git::GitConfig {
                roots:vec![devworks_core::services::git::RegisteredGitRoot {
                    id:id("review-root"), path:std::fs::canonicalize(&worktrees).unwrap(),
                }],
                filesystem:Some(Arc::new(NativeFilesystem::new())),
                ..Default::default()
            },
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
        composed.application = devworks_engine::ApplicationComposition {
            connections: Some(devworks_engine::ConnectionPorts {
                http: Arc::new(devworks_core::ports::http::NoNetwork),
                secrets: Arc::new(devworks_core::ports::secrets::SessionSecrets::new()),
                entropy: Arc::new(TestEntropy),
                endpoints: None,
            }),
            router: Some(Default::default()),
            ..Default::default()
        };
        composed.config.providers = Some(devworks_engine::providers::driver::ProviderComposition::new(wire.clone()));
        let app = Arc::new(boot(composed).await.unwrap());
        app.runs().install_launcher(Arc::new(AgentLauncher::new(
            fx.store.clone(), fx.h.ids.clone(), app.agents().clone(),
            Arc::new(NativeAgentTurnExecutor::new(&app)),
        ))).unwrap();
        seed_local_route(&fx);
        applied(&app.command(&fx.user, "router.configure", json!({
            "mode":"Command","protocol":PROTOCOL,"schemaRevision":3,"requestKey":"code-review-routing",
            "body":{"kind":"router.configure","action":{
                "kind":"Tier","tier":"High","expectedRevision":"1","memberIds":["watch-model"]
            }},
        }).to_string().as_bytes()).unwrap());
        let target = json!({"taskId":"task-f1","diffs":[{
            "repositoryId":repo,"worktreeId":tree.id,"base":{"format":"Sha1","hex":base},
            "head":{"format":"Sha1","hex":head}
        }]});
        let command = json!({"mode":"Command","protocol":PROTOCOL,"schemaRevision":15,
            "requestKey":"request-local-review","body":{"kind":"reviews.request","rootId":"root-f",
            "expectedRevision":"1","policyRevision":"1","localTarget":target}});
        let reply = app.command(&fx.user, "reviews.request", command.to_string().as_bytes()).unwrap();
        let operation = match reply {
            CommandReply::Recorded{receipt,..} => match receipt.decision {
                DomainDecision::Accepted{operation_id} => operation_id,
                other => panic!("review admission {other:?}"),
            },
            other => panic!("review admission {other:?}"),
        };
        applied(&app.command(&fx.user, "runs.start", json!({"protocol":PROTOCOL,"requestKey":"start-code-review","command":{"kind":"runs.start","rootId":"root-f","expectedWorkRevision":"1","expectedControlRevision":"1"}}).to_string().as_bytes()).unwrap());
        let outcome = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let current = read(&*fx.store, |snapshot| snapshot.operations().operation(&operation)).unwrap();
                if current.state == OperationState::ObservedComplete || current.state == OperationState::Failed {
                    break current;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        }).await.unwrap_or_else(|_| {
            let (operation, reviews) = read(&*fx.store, |snapshot| {
                Ok((
                    snapshot.operations().operation(&operation)?,
                    (snapshot.git().code_reviews(None, Some(&rid("root-f")), None, 10)?,
                    snapshot.history().page(&EventFilter::Root(rid("root-f")), 0, 100)?.events
                        .into_iter().filter(|event| matches!(event.kind.as_str(), "ActivationRetryScheduled" | "CommandDecided"))
                        .map(|event| event.payload).collect::<Vec<_>>()),
                ))
            });
            panic!("independent code review did not finish: {operation:?}; {reviews:?}; {:?}", (fx.holds("root-f"), fx.events("root-f"), fx.inspection("root-f").assignments, wire.reviews.lock().unwrap().len()))
        });
        assert_eq!(outcome.state, OperationState::ObservedComplete, "{outcome:?}; {:?}", read(&*fx.store, |snapshot| {
            let history = snapshot.history().page(&EventFilter::Root(rid("root-f")), 0, 175)?.events
                .into_iter().filter(|event| matches!(event.kind.as_str(), "ActivationRetryScheduled" | "ChildFailed" | "CodeReviewFailed" | "ProviderCallCompleted" | "ProviderCallFailed" | "AssignmentStateChanged"))
                .map(|event| (event.kind, event.payload)).collect::<Vec<_>>();
            let sessions = root_assignments(snapshot.runs(), &rid("root-f"), false)?.into_iter()
                .map(|assignment| snapshot.sessions().entries(&assignment.session_id, 0, 100))
                .collect::<Result<Vec<_>,_>>()?;
            Ok((history,sessions,wire.requests.lock().unwrap().clone()))
        }));
        assert_eq!(wire.reviews.lock().unwrap().len(), 1);
        let readiness = json!({"mode":"Query","protocol":PROTOCOL,"schemaRevision":15,
            "queryId":"review-readiness","body":{"kind":"reviews.readiness","rootId":"root-f",
            "policyRevision":"1","localTarget":target}});
        let result = app.query(&fx.user,"reviews.readiness",readiness.to_string().as_bytes()).unwrap();
        assert!(matches!(result, DomainQueryReply::Result {
            result:devworks_core::protocol::results::OperationResult::ReviewsReadiness{value},..}
            if value.ready && value.review_ids.0.len() == 1));
        let replay = app.command(&fx.user, "reviews.request", command.to_string().as_bytes()).unwrap();
        assert!(matches!(replay, CommandReply::Recorded { replayed: true, .. }));
        assert_eq!(wire.reviews.lock().unwrap().len(), 1);
        app.shutdown().await;
    });
}

struct TestEntropy;
impl devworks_core::ports::http::Entropy for TestEntropy {
    fn fill(&self, bytes: &mut [u8]) {
        bytes.fill(11);
    }
}
struct HostAuthorityForTest {
    store: Arc<dyn ApplicationStore>,
    clock: Arc<dyn devworks_core::ports::Clock>,
}
impl devworks_core::ports::AuthorityPort for HostAuthorityForTest {
    fn authorize(
        &self,
        ctx: &InvocationContext,
        request: &devworks_core::ports::AuthorityRequest<'_>,
    ) -> Result<devworks_core::ports::Authorization, devworks_core::ports::AuthorityDenial> {
        HostAuthority {
            store: &*self.store,
            clock: &*self.clock,
            protected_branches: &[],
        }
        .authorize(ctx, request)
    }
}
