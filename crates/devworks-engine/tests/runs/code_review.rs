//! Native Git plus the real agent scheduler. Model transport is scripted.
use super::*;
use std::path::Path;
use std::process::Command;

struct CodeReviewWire {
    reviews: Mutex<Vec<Value>>,
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
        let reviewed = rendered.contains("Independently review these exact Git diffs");
        if reviewed {
            assert!(
                rendered.contains("+fn saved_review_source() {}"),
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

fn git(path: &Path, args: &[&str]) -> String {
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
    runtime().block_on(async {
        let source = fx.h.root.join("exact-review-repository");
        let worktrees = fx.h.root.join("exact-review-worktrees");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&worktrees).unwrap();
        git(&source, &["init", "-qb", "main"]);
        std::fs::write(source.join("lib.rs"), "fn prior() {}\n").unwrap();
        git(&source, &["add", "lib.rs"]);
        git(&source, &["commit", "-qm", "Base"]);
        let base = git(&source, &["rev-parse", "HEAD"]);
        let repo = id("review-git-repository");
        let project = ProjectId(id("project-a"));
        write(&*fx.store, {
            let (repo, project, source) = (repo.clone(), project.clone(), source.clone());
            move |tx| {
            let now = tx.now_ms();
            tx.projects().insert_repository(&StoredRepository {
                id: repo.clone(), location: std::fs::canonicalize(&source).unwrap().to_string_lossy().into_owned(),
                name:"Review fixture".into(), remote_url:None, primary:true, lifecycle:"Active".into(),
                languages:vec!["rust".into()], revision:1, registered_at_ms:now,
            })?;
            tx.projects().set_members(&project, &[repo.clone()])?;
            tx.work().set_work_repositories(&id("task-f1"), &[repo.clone()])
            }
        }).unwrap();
        let wire = Arc::new(CodeReviewWire { reviews: Mutex::new(Vec::new()) });
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
        let app = boot(composed).await.unwrap();
        seed_local_route(&fx);
        applied(&app.command(&fx.user, "router.configure", json!({
            "mode":"Command","protocol":PROTOCOL,"schemaRevision":3,"requestKey":"code-review-routing",
            "body":{"kind":"router.configure","action":{
                "kind":"Tier","tier":"High","expectedRevision":"1","memberIds":["watch-model"]
            }},
        }).to_string().as_bytes()).unwrap());
        let checkout = app.command(&fx.user, "git.worktree", json!({
            "mode":"Command","protocol":PROTOCOL,"requestKey":"create-review-worktree",
            "body":{"kind":"git.worktree","repositoryId":repo,"workId":"task-f1","expectedRevision":"1",
                "action":"Create","base":{"format":"Sha1","hex":base},"registeredRootId":"review-root"}
        }).to_string().as_bytes()).unwrap();
        assert!(matches!(checkout, CommandReply::Recorded{receipt,..}
            if matches!(receipt.decision, DomainDecision::Accepted{..})));
        let mut tree = within("the native Git worktree is observed", async {
            loop {
                let list = read(&*fx.store, |snapshot| snapshot.git().git_worktrees(&id("task-f1"),None,10));
                if let Some(tree) = list.into_iter().find(|tree| tree.location.is_some() && tree.pending_operation_id.is_none()) {
                    break tree;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await;
        let path = std::path::PathBuf::from(&tree.path);
        std::fs::write(path.join("lib.rs"), "fn saved_review_source() {}\n").unwrap();
        git(&path, &["add", "lib.rs"]);
        git(&path, &["commit", "-qm", "Exact task change"]);
        let head = git(&path, &["rev-parse", "HEAD"]);
        // The fixture supplies the ordinary owned-worktree checkpoint of that native commit.
        let old = tree.revision;
        tree.head.hex = head.clone();
        tree.revision += 1;
        write(&*fx.store, {
            let tree = tree.clone();
            move |tx| tx.git().update_git_worktree(&tree, old)
        }).unwrap();
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
        let outcome = within("the independent native reviewer reports", async {
            loop {
                let current = read(&*fx.store, |snapshot| snapshot.operations().operation(&operation)).unwrap();
                if current.state == OperationState::ObservedComplete || current.state == OperationState::Failed {
                    break current;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        }).await;
        assert_eq!(outcome.state, OperationState::ObservedComplete, "{outcome:?}");
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
