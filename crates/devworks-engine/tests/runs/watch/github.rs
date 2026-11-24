use super::*;
use devworks_core::authority::redact::SecretRef;
use devworks_core::ports::http::{HttpError, HttpExchange, HttpResponse, HttpTransport};
use devworks_core::ports::secrets::{Secret, SecretResolver, SecretUnavailable};
use devworks_core::services::git::github::GitHubClient;
use devworks_core::services::github::GitHubService;

struct Credentials;

impl SecretResolver for Credentials {
    fn resolve(&self, _: &SecretRef) -> Result<Secret, SecretUnavailable> {
        Ok(Secret::new("native-watcher-fixture"))
    }
}

#[derive(Default)]
struct RecordedWorkflow {
    requests: Mutex<Vec<Option<String>>>,
}

impl HttpTransport for RecordedWorkflow {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        self.send_with_headers(request)
            .map(|exchange| exchange.response)
    }

    fn send_with_headers(&self, request: &HttpRequest) -> Result<HttpExchange, HttpError> {
        let mut headers = Vec::new();
        let (status, value) = match request.url.as_str() {
            "https://api.github.com/user" => (200, json!({"login":"watch-user","id":17})),
            "https://api.github.com/repos/Acme/watch" => (
                200,
                json!({
                    "id": 12, "node_id":"watch-repo-node", "name":"watch", "full_name":"Acme/watch",
                    "owner":{"login":"Acme", "type":"Organization"},
                    "html_url":"https://github.com/Acme/watch", "default_branch":"main",
                    "archived":false, "disabled":false, "permissions":{"pull":true,"push":false}
                }),
            ),
            "https://api.github.com/repositories/12/actions/runs/42" => {
                let mut requests = self.requests.lock().unwrap();
                requests.push(request.header_value("If-None-Match").map(str::to_owned));
                if requests.len() == 2 {
                    (304, Value::Null)
                } else if requests.len() == 3 {
                    headers.push(("retry-after".to_owned(), "60".to_owned()));
                    (429, Value::Null)
                } else {
                    headers.push(("etag".to_owned(), "\"watch-etag\"".to_owned()));
                    let (status, conclusion) = if matches!(requests.len(), 1 | 5) {
                        ("in_progress", Value::Null)
                    } else {
                        ("completed", json!("failure"))
                    };
                    (
                        200,
                        json!({
                            "id":42, "repository":{"id":12},
                            "html_url":"https://github.com/Acme/watch/actions/runs/42",
                            "status":status, "conclusion":conclusion
                        }),
                    )
                }
            }
            url => panic!("unexpected GitHub request {url}"),
        };
        Ok(HttpExchange {
            response: HttpResponse {
                status,
                body: value.to_string().into_bytes(),
            },
            headers,
        })
    }
}

fn qualify(fx: &Fx, client: Arc<GitHubClient>, remote: &'static str) {
    let location = fx.h.root.to_string_lossy().into_owned();
    fx.tx(move |_, tx| {
        let schema = |key| devworks_core::settings::catalog::descriptor(key).unwrap().schema_ref();
        tx.configuration().append_revision(
            &ConfigScope::Workspace,
            "settings",
            1,
            1,
            json!({"values":{
                "github.credential":{
                    "schema":schema("github.credential"), "value":{"secretRef":SecretRef::mint(42).as_str()}
                },
                "github.importOpenIssues":{"schema":schema("github.importOpenIssues"),"value":false},
                "github.syncLinkedIssueFields":{"schema":schema("github.syncLinkedIssueFields"),"value":false},
                "github.pollForChanges":{"schema":schema("github.pollForChanges"),"value":false}
            }}),
        )?;
        let repo = id("watch-github-repo");
        tx.projects().insert_repository(&StoredRepository {
            id: repo.clone(),
            name: remote.rsplit('/').next().unwrap().into(),
            location,
            remote_url: Some(format!("https://github.com/{remote}.git")),
            primary: true,
            lifecycle: "Active".into(),
            languages: Vec::new(),
            revision: 1,
            registered_at_ms: 0,
        })?;
        let project = ProjectId(id("project-a"));
        let mut members = tx.projects().members(&project)?;
        members.push(repo);
        tx.projects().set_members(&project, &members)
    });
    let service = Arc::new(GitHubService::new(
        fx.store.clone(),
        client,
        fx.h.ids.clone(),
        fx.h.clock(),
    ));
    let authority = HostAuthority {
        store: fx.store.as_ref(),
        clock: &*fx.h.clock,
        protected_branches: &[],
    };
    let gateway = Gateway {
        store: fx.store.as_ref(),
        authority: &authority,
        ids: &*fx.h.ids,
    };
    let refresh = serde_json::to_vec(&json!({
        "mode":"Command", "protocol":"1.0-draft.1", "requestKey":"connect-watch-github",
        "body":{"kind":"github.account","action":"Refresh"}
    }))
    .unwrap();
    let reply = gateway
        .command(&fx.user, "github.account", &refresh, service.clone())
        .unwrap();
    assert!(matches!(reply, CommandReply::Recorded { .. }));
    service
        .qualify_registration(&id("watch-github-repo"))
        .unwrap();
}

async fn app_with_github(fx: &Fx, client: Arc<GitHubClient>) -> Runtime {
    let (mut composed, _) = watch_composition(fx, &fx.h.root);
    composed.git = Some(devworks_engine::git::GitComposition {
        config: devworks_core::services::git::GitConfig {
            github: Some(client),
            ..Default::default()
        },
        github_http: None,
    });
    devworks_engine::boot(composed).await.unwrap()
}

fn create_github_watch(app: &Runtime, ctx: &InvocationContext, run_id: &str, key: &str) -> Id {
    let frame = serde_json::to_vec(&json!({
        "mode":"Command", "protocol":"1.0-draft.1", "schemaRevision":9, "requestKey":key,
        "body":{"kind":"watch.create", "github":{"repositoryId":"watch-github-repo", "workflowRunId":run_id}}
    })).unwrap();
    let reply = app.command(ctx, "watch.create", &frame).unwrap();
    let CommandReply::Recorded { receipt, .. } = reply else {
        panic!("{reply:?}")
    };
    let DomainDecision::Applied {
        result: OperationResult::WatchCreate { value },
    } = receipt.decision
    else {
        panic!("{:?}", receipt.decision)
    };
    value.id
}

#[test]
fn a_github_run_shares_conditional_polls_and_the_backup_reads_it_directly() {
    let _native_case = native_runtime_case();
    runtime().block_on(async {
        let fx = Fx::new();
        let wire = Arc::new(RecordedWorkflow::default());
        let client = Arc::new(GitHubClient {
            http: wire.clone(),
            secrets: Arc::new(Credentials),
        });
        qualify(&fx, client.clone(), "Acme/watch");
        let app = app_with_github(&fx, client).await;
        let (ctx, _) = agent_context(&fx);
        let mut watchers = Vec::new();
        for key in ["github-watch-first", "github-watch-second"] {
            watchers.push(create_github_watch(&app, &ctx, "42", key));
        }
        let supervisor = app.watchers().unwrap();
        assert_eq!(supervisor.settle().unwrap(), 0);
        assert_eq!(*wire.requests.lock().unwrap(), vec![None]);
        fx.h.advance(30_000);
        assert_eq!(supervisor.settle().unwrap(), 0);
        assert_eq!(
            *wire.requests.lock().unwrap(),
            vec![None, Some("\"watch-etag\"".into())]
        );
        let host = principal(
            fx.store.as_ref(),
            "watch-github-backup",
            PrincipalKind::Host,
        );
        let _ = supervisor.service().backup_pass(&host).unwrap();
        assert_eq!(
            *wire.requests.lock().unwrap(),
            vec![None, Some("\"watch-etag\"".into()), None]
        );
        assert_eq!(supervisor.settle().unwrap(), 0);
        assert_eq!(wire.requests.lock().unwrap().len(), 3);
        fx.h.advance(60_000);
        let _ = supervisor.service().backup_pass(&host).unwrap();
        assert_eq!(
            *wire.requests.lock().unwrap(),
            vec![None, Some("\"watch-etag\"".into()), None, None]
        );
        for watcher in watchers {
            let event = read(&*fx.store, |snapshot| {
                snapshot.watchers().watch_event_for_watcher(&watcher)
            })
            .unwrap();
            assert_eq!(event.payload["outcome"]["kind"], "Failed");
            assert!(
                event.payload["outcome"]["detail"]
                    .as_str()
                    .unwrap()
                    .contains("/actions/runs/42")
            );
        }
        let rerun = create_github_watch(&app, &ctx, "42", "watch-rerun");
        assert_eq!(
            supervisor.settle().unwrap(),
            0,
            "old terminal state is not reused"
        );
        assert_eq!(wire.requests.lock().unwrap().len(), 4);
        fx.h.advance(30_000);
        assert_eq!(supervisor.settle().unwrap(), 0);
        assert_eq!(wire.requests.lock().unwrap().len(), 5);
        assert!(
            read(&*fx.store, |snapshot| {
                snapshot.watchers().watch_event_for_watcher(&rerun)
            })
            .is_none()
        );
        app.shutdown().await;
    });
}

#[derive(Default)]
struct RecordedPull {
    requests: Mutex<Vec<String>>,
    suite_queued: std::sync::atomic::AtomicBool,
}

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

impl HttpTransport for RecordedPull {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        self.send_with_headers(request)
            .map(|exchange| exchange.response)
    }

    fn send_with_headers(&self, request: &HttpRequest) -> Result<HttpExchange, HttpError> {
        let path = request.url.as_str();
        if path.ends_with("/user") || path.ends_with("/repos/Acme/watch") {
            return RecordedWorkflow::default().send_with_headers(request);
        }
        let mut requests = self.requests.lock().unwrap();
        requests.push(path.to_owned());
        let mut headers = Vec::new();
        let value = if path.ends_with("/pulls/7") {
            if requests.len() == 1 {
                headers.push(("x-ratelimit-remaining".into(), "0".into()));
            }
            json!({"number":7,"base":{"repo":{"id":12}},"head":{"sha":HEAD},
                "html_url":"https://github.com/Acme/watch/pull/7"})
        } else if path.contains("/check-runs?") {
            json!({"total_count":1,"check_runs":[
                {"id":1,"head_sha":HEAD,"status":"completed","conclusion":"success"}
            ]})
        } else if path.contains("/status?") {
            json!({"total_count":0,"sha":HEAD,"state":"pending"})
        } else if path.contains("/check-suites?") {
            let queued = self.suite_queued.load(std::sync::atomic::Ordering::Relaxed);
            json!({"total_count":1,"check_suites":[
                {"id":3,"head_sha":HEAD,"status":if queued {"queued"} else {"completed"}}
            ]})
        } else if path.contains("/actions/runs?") {
            json!({"total_count":0,"workflow_runs":[]})
        } else {
            panic!("unexpected request {path}");
        };
        Ok(HttpExchange {
            response: HttpResponse {
                status: 200,
                body: value.to_string().into_bytes(),
            },
            headers,
        })
    }
}

#[test]
fn pr_check_settlement_waits_for_pending_suites_and_honors_head_pacing() {
    let _native_case = native_runtime_case();
    runtime().block_on(async {
        let fx = Fx::new();
        let wire = Arc::new(RecordedPull::default());
        wire.suite_queued.store(true, std::sync::atomic::Ordering::Relaxed);
        let client = Arc::new(GitHubClient {
            http: wire.clone(),
            secrets: Arc::new(Credentials),
        });
        qualify(&fx, client.clone(), "Acme/watch");
        let app = app_with_github(&fx, client).await;
        let (ctx, _) = agent_context(&fx);
        let frame = json!({
            "mode":"Command", "protocol":"1.0-draft.1", "schemaRevision":9, "requestKey":"watch-pull",
            "body":{"kind":"watch.create","github":{"repositoryId":"watch-github-repo","pullRequestNumber":"7"}}
        });
        let reply = app.command(&ctx, "watch.create", frame.to_string().as_bytes()).unwrap();
        let CommandReply::Recorded { receipt, .. } = reply else { panic!("{reply:?}") };
        let DomainDecision::Applied { result: OperationResult::WatchCreate { value } } =
            receipt.decision else { panic!("{:?}", receipt.decision) };
        let event = || read(&*fx.store, |snapshot| {
            snapshot.watchers().watch_event_for_watcher(&value.id)
        });
        let supervisor = app.watchers().unwrap();
        assert_eq!(supervisor.settle().unwrap(), 0);
        assert_eq!(wire.requests.lock().unwrap().len(), 1, "head exhausted the budget");
        fx.h.advance(60_000);
        assert_eq!(supervisor.settle().unwrap(), 0, "check suite is still queued");
        assert_eq!(wire.requests.lock().unwrap().len(), 5);
        assert!(event().is_none());
        wire.suite_queued.store(false, std::sync::atomic::Ordering::Relaxed);
        fx.h.advance(30_000);
        assert_eq!(supervisor.settle().unwrap(), 1);
        assert_eq!(event().unwrap().payload["outcome"]["kind"], "Succeeded");
        app.shutdown().await;
    });
}

struct LiveCredentials(Secret);

impl SecretResolver for LiveCredentials {
    fn resolve(&self, _: &SecretRef) -> Result<Secret, SecretUnavailable> {
        Ok(self.0.clone())
    }
}

#[derive(Default)]
struct LiveWorkflow {
    http: devworks_adapters::http::NativeHttp,
    last_status: Mutex<Option<String>>,
}

impl HttpTransport for LiveWorkflow {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        self.send_with_headers(request)
            .map(|exchange| exchange.response)
    }

    fn send_with_headers(&self, request: &HttpRequest) -> Result<HttpExchange, HttpError> {
        let exchange = self.http.send_with_headers(request)?;
        if request.url.contains("/actions/runs/") && exchange.response.status == 200 {
            let body = serde_json::from_slice::<Value>(&exchange.response.body).unwrap();
            *self.last_status.lock().unwrap() = body["status"].as_str().map(str::to_owned);
        }
        Ok(exchange)
    }
}

#[test]
#[ignore = "requires a separately started real DevWorks Actions run and GitHub credential"]
fn a_live_github_run_reaches_the_saved_watcher_after_a_runtime_restart() {
    let _native_case = native_runtime_case();
    let run =
        std::env::var("DEVWORKS_WATCH_ACCEPTANCE_RUN_ID").expect("a live DevWorks Actions run");
    let token =
        Secret::new(std::env::var("DEVWORKS_WATCH_ACCEPTANCE_TOKEN").expect("GitHub credential"));
    runtime().block_on(async {
        let fx = Fx::new();
        let wire = Arc::new(LiveWorkflow::default());
        let client = Arc::new(GitHubClient {
            http: wire.clone(),
            secrets: Arc::new(LiveCredentials(token)),
        });
        qualify(&fx, client.clone(), "SceneWorks/DevWorks");
        let app = app_with_github(&fx, client.clone()).await;
        let (ctx, _) = agent_context(&fx);
        let watcher = create_github_watch(&app, &ctx, &run, "live-github-watch");
        let event = || {
            read(&*fx.store, |snapshot| {
                snapshot.watchers().watch_event_for_watcher(&watcher)
            })
        };
        assert_eq!(
            app.watchers().unwrap().settle().unwrap(),
            0,
            "target must still be running"
        );
        assert_eq!(
            wire.last_status.lock().unwrap().as_deref(),
            Some("in_progress")
        );
        assert!(event().is_none(), "work cannot settle before restart");
        app.shutdown().await;
        drop(app);
        let app = app_with_github(&fx, client).await;
        let limit = std::time::Instant::now() + Duration::from_secs(40 * 60);
        let completed = loop {
            app.watchers().unwrap().settle().unwrap();
            if let Some(done) = event() {
                break done;
            }
            assert!(std::time::Instant::now() < limit, "live run did not settle");
            tokio::time::sleep(Duration::from_secs(30)).await;
            fx.h.advance(30_000);
        };
        let detail = completed.payload["outcome"]["detail"].as_str().unwrap();
        assert!(detail.starts_with("GitHub conclusion: "));
        assert!(detail.contains(&format!(
            "https://github.com/SceneWorks/DevWorks/actions/runs/{run}"
        )));
        applied(&fx.resume(&fx.user, "root-i"));
        app.watchers().unwrap().deliver().unwrap();
        assert!(matches!(
            event().unwrap().state,
            WatchEventState::Delivered | WatchEventState::Acknowledged
        ));
        println!("{}", detail);
        app.shutdown().await;
    });
}

struct RecordedCodeReviews {
    base: String,
    head: String,
    requests: Mutex<Vec<String>>,
}

impl HttpTransport for RecordedCodeReviews {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        self.requests.lock().unwrap().push(request.url.clone());
        let value = match request.url.as_str() {
            "https://api.github.com/user" => json!({"login":"watch-user","id":17}),
            "https://api.github.com/repos/Acme/watch" => json!({
                "id":12, "node_id":"watch-repo-node", "name":"watch", "full_name":"Acme/watch",
                "owner":{"login":"Acme", "type":"Organization"},
                "html_url":"https://github.com/Acme/watch", "default_branch":"main",
                "archived":false, "disabled":false, "permissions":{"pull":true,"push":false}
            }),
            path if path.ends_with("/pulls/42") => json!({
                "number":42, "html_url":"https://github.com/Acme/watch/pull/42",
                "state":"open", "base":{"repo":{"id":12},"ref":"main","sha":self.base},
                "head":{"sha":self.head}
            }),
            path if path.contains("/pulls/42/reviews?") => json!([{
                "id":15, "commit_id":self.head, "state":"COMMENTED",
                "user":{"id":42,"type":"Bot","login":"test-review[bot]"},
                "submitted_at":"2023-11-14T22:13:19Z", "body":"Actionable comments posted: 0"
            }]),
            path if path.contains("/pulls/42/comments?") => json!([]),
            path if path.ends_with("/branches/main/protection") => json!({
                "required_status_checks":{"checks":[{"context":"build","app_id":999}]}
            }),
            path if path.ends_with("/rules/branches/main") => json!([]),
            url => panic!("unexpected final review observation endpoint {url}"),
        };
        Ok(HttpResponse {
            status: 200,
            body: value.to_string().into_bytes(),
        })
    }
}

#[test]
fn native_final_review_qualifies_current_bot_evidence_and_exact_post_cap_repair() {
    use crate::code_review_tests::git;
    use devworks_core::authority::destructive::FilesystemResolver;
    use devworks_core::ports::storage::{
        NewEvent, NewOperation, StoredGithubObservation, StoredLink,
    };
    use devworks_core::protocol::EventId;
    use devworks_core::protocol::data::{GitOid, ObjectFormat};
    use devworks_core::services::artifacts::evidence::Evidence;

    let _case = native_runtime_case();
    let fx = Fx::new();
    runtime().block_on(async {
        let source = &fx.h.root;
        let trees = source.join("final-review-worktrees");
        std::fs::create_dir(&trees).unwrap();
        git(source, &["init", "-qb", "main"]);
        std::fs::write(source.join("lib.rs"), "fn before() {}\n").unwrap();
        git(source, &["add", "lib.rs"]);
        git(source, &["commit", "-qm", "Base"]);
        let base = git(source, &["rev-parse", "HEAD"]);
        let location = trees.join("issue");
        git(source, &["worktree", "add", "-qb", "review-candidate", location.to_str().unwrap(), &base]);
        std::fs::write(location.join("lib.rs"), "fn candidate() {}\n").unwrap();
        git(&location, &["add", "lib.rs"]);
        git(&location, &["commit", "-qm", "Candidate"]);
        let head = git(&location, &["rev-parse", "HEAD"]);
        let oid = |hex: &str| GitOid { format:ObjectFormat::Sha1, hex:hex.into() };
        let wire = Arc::new(RecordedCodeReviews {
            base:base.clone(), head:head.clone(), requests:Mutex::default(),
        });
        let client = Arc::new(GitHubClient { http:wire.clone(), secrets:Arc::new(Credentials) });
        qualify(&fx, client.clone(), "Acme/watch");
        let (mut composed, _) = watch_composition(&fx, &fx.h.root);
        composed.git = Some(devworks_engine::git::GitComposition {
            github_http: None,
            config:devworks_core::services::git::GitConfig {
                github:Some(client),
                roots:vec![devworks_core::services::git::RegisteredGitRoot {
                    id:id("final-review-root"), path:std::fs::canonicalize(&trees).unwrap(),
                }],
                filesystem:Some(Arc::new(NativeFilesystem::new())),
                ..Default::default()
            },
        });
        let app = boot(composed).await.unwrap();
        let project = ProjectId(id("project-a"));
        let repository = id("watch-github-repo");
        let root = rid("root-i");
        let owner = fx.user.principal().clone();
        let evidence = app.artifacts().publish_evidence(&*fx.store, Evidence {
            scope:ArtifactScope::Project(project.clone()),producer:owner.clone(),
            principal_kind:"Human".into(),agent_session:None,root:Some(root.clone()),
            assignment:None,title:"PR delivery evidence".into(),media_type:"text/plain".into(),
            tags:vec![],source:json!({"kind":"Fixture"}),
        }, b"Qualified PR candidate", |_,_| Ok(())).unwrap().0;
        let fs = NativeFilesystem::new();
        let identity = |path: &str| fs.resolve(path, None).unwrap().target.unwrap().identity.unwrap();
        let real_path = std::fs::canonicalize(&location).unwrap().to_string_lossy().into_owned();
        let worktree = StoredGitWorktree {
            id:id("final-review-worktree"),repository_id:repository.clone(),work_id:root.0.clone(),
            owner:owner.clone(),ownership_revision:1,owner_root_id:Some(root.clone()),
            owner_assignment_id:None,registered_root_id:id("final-review-root"),path:real_path.clone(),
            branch:"refs/heads/review-candidate".into(),head:oid(&head),revision:1,removed:false,
            location:Some(GitWorktreeLocation {
                root:identity(&real_path),
                git_dir:identity(&git(&location, &["rev-parse", "--absolute-git-dir"])),
                common_dir:identity(&git(&location, &["rev-parse", "--path-format=absolute", "--git-common-dir"])),
            }),pending_operation_id:None,last_operation_id:None,last_result:None,
        };
        let pull = "https://github.com/Acme/watch/pull/42";
        let remote = fx.tx({
            let (project, repository, root, base, head) = (project.clone(), repository.clone(), root.clone(), base.clone(), head.clone());
            move |_,tx| {
                let scope = ConfigScope::Repository { project:project.clone(), repository:repository.clone() };
                let bot = "reviews.githubBotId";
                let schema = |key| devworks_core::settings::catalog::descriptor(key).unwrap().schema_ref();
                tx.configuration().append_revision(&scope, "settings", 1, 1,
                    json!({"values":{"reviews.githubBotId":{"schema":schema(bot),"value":42},
                        "delivery.maxReviewFixCycles":{"schema":schema("delivery.maxReviewFixCycles"),"value":1}
                    }}))?;
                tx.work().set_work_repositories(&root.0, std::slice::from_ref(&repository))?;
                tx.git().insert_git_worktree(&worktree)?;
                let operation = OperationId(id("observed-pr-operation"));
                tx.operations().record_intent(NewOperation {
                    id:operation.clone(), root_id:Some(root.clone()),assignment_id:None,
                    capability:"git.pr.create".into(),schema_version:1,args_digest:"fixture".into(),
                    payload:json!({}),
                })?;
                let event = EventId(id("observed-pr-event"));
                let sequence = tx.history().append(NewEvent {
                    id:event.clone(),actor:owner.clone(),project_id:Some(project.clone()),
                    root_id:Some(root.clone()),operation_id:Some(operation),aggregate_kind:"git_pull_request".into(),
                    aggregate_id:"42".into(),aggregate_revision:1,kind:"GitPullRequestObserved".into(),
                    payload:json!({"repositoryId":repository,"pullRequestUrl":pull}),
                })?;
                tx.sessions().insert_link(&StoredLink {
                    id:id("observed-pr-link"),project_id:project,root_id:root,
                    kind:"pull_request".into(),target_kind:"pull_request".into(),target_id:pull.into(),
                    parent_id:None,supersedes_id:None,actor:owner,event_id:event.0,
                    sequence,repository_id:Some(repository.clone()),revision:None,
                    summary:"Observed pull request".into(),body:json!({"worktreeId":worktree.id}),
                    created_at_ms:T0,
                })?;
                let remote = tx.github().github_repository_binding(&repository)?.unwrap().github_repository_id;
                tx.github().append_github_observation(&StoredGithubObservation {
                    id:id("observed-pr-first-head"),repository_id:remote.clone(),kind:"PullRequest".into(),
                    subject_id:"42".into(),remote_revision:"first-head".into(),url:pull.into(),head:Some(head),
                    payload:json!({"resourceId":25,"state":"open","baseRef":"main","baseHead":base}),
                    source_updated_at_ms:Some(T0),observed_at_ms:T0,
                })?;
                Ok(remote)
            }
        });
        let barrier = json!({
            "protocol":PROTOCOL,"deliveryId":"final-review-delivery","rootId":root,
            "kind":"StandaloneIssue","revision":"1","approvedScopeDigest":"0".repeat(64),
            "dependencies":[],"repositories":[{
                "repositoryId":repository,"pullRequestId":"observed-pr-link","baseRef":"refs/heads/main",
                "expectedHead":oid(&head),"observedBase":oid(&base),"reviewSource":"GitHubAutomated",
                "mergeMethod":"Squash","evidence":evidence,"policyRevision":"1"
            }]
        });
        let receipt = app.command(&fx.user, "reviews.request", json!({
            "mode":"Command","protocol":PROTOCOL,"schemaRevision":15,"requestKey":"native-final-review",
            "body":{"kind":"reviews.request","rootId":root,"expectedRevision":"1","policyRevision":"1","barrier":barrier}
        }).to_string().as_bytes()).unwrap();
        let operation = match receipt {
            CommandReply::Recorded {receipt,..} => match receipt.decision {
                DomainDecision::Accepted {operation_id} => operation_id,
                other => panic!("review admission {other:?}"),
            },
            other => panic!("review receipt {other:?}"),
        };
        tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                let current = read(&*fx.store, |snapshot| snapshot.operations().operation(&operation)).unwrap();
                if current.state == OperationState::ObservedComplete {
                    break;
                }
                assert_ne!(current.state, OperationState::Failed, "{current:?}");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await.unwrap_or_else(|_| {
            let state = read(&*fx.store, |snapshot| {
                let observations = snapshot.github().github_observations(&GithubObservationFilter {
                     scope: GithubScope { repositories:vec![repository.clone()], project_id:Some(project.clone()) },
                     ..Default::default()
                }, None, 25)?;
                Ok((snapshot.operations().operation(&operation)?,
                    snapshot.git().code_reviews(Some(&project),Some(&root),None,5)?, observations))
            });
            panic!("final review did not finish: {state:?}; {:?}", wire.requests.lock().unwrap().iter().take(20).collect::<Vec<_>>());
        });
        let readiness = |query: &str, candidate: &Value| app.query(&fx.user,"reviews.readiness",json!({
            "mode":"Query","protocol":PROTOCOL,"schemaRevision":15,"queryId":query,
            "body":{"kind":"reviews.readiness","rootId":root,"policyRevision":"1","barrier":candidate}
        }).to_string().as_bytes()).unwrap();
        assert!(matches!(readiness("current-review", &barrier), DomainQueryReply::Result {
            result:OperationResult::ReviewsReadiness {value},..
        } if value.ready && value.review_ids.0.len() == 1));
        let review = read(&*fx.store, |s| {
            Ok(s.git().code_reviews(Some(&project), Some(&root), None, 10)?.remove(0))
        });
        assert_eq!(review.body["value"]["sources"][0]["head"], json!(oid(&head)));
        assert_eq!(review.body["value"]["sources"][0]["zeroFindingsConfirmed"], true);

        let changed_path = "ci/retry.env";
        std::fs::create_dir(location.join("ci")).unwrap();
        std::fs::write(location.join(changed_path), "CI_RETRY=1\n").unwrap();
        git(&location, &["add", changed_path]);
        git(&location, &["commit", "-qm", "Repair CI fixture"]);
        let candidate = git(&location, &["rev-parse", "HEAD"]);
        let patch = git(&location, &[
            "--no-replace-objects", "diff", "--no-color", "--no-ext-diff", "--no-textconv",
            "--ignore-submodules=none", "--submodule=short", "--binary", "--no-renames",
            "--src-prefix=a/", "--dst-prefix=b/", &head, &candidate, "--", changed_path,
        ]) + "\n";
        let patch_digest = devworks_core::fingerprint::sha256_hex(patch.as_bytes());
        let change = devworks_core::fingerprint::canonical_digest(&json!([changed_path, patch_digest])).unwrap();
        fx.tx({
            let (base, head, candidate) = (base.clone(), head.clone(), candidate.clone());
            move |_,tx| {
                let mut tree = tx.git().git_worktree(&id("final-review-worktree"))?.unwrap();
                tree.revision += 1;
                tree.head.hex = candidate.clone();
                tx.git().update_git_worktree(&tree, 1)?;
                tx.github().append_github_observation(&StoredGithubObservation {
                    id:id("observed-pr-replacement-head"),repository_id:remote.clone(),kind:"PullRequest".into(),
                    subject_id:"42".into(),remote_revision:"replacement-head".into(),url:pull.into(),head:Some(candidate.clone()),
                    payload:json!({"resourceId":25,"state":"open","baseRef":"main","baseHead":base}),
                    source_updated_at_ms:Some(T0+1),observed_at_ms:T0+1,
                })?;
                for (check_id, name, head, conclusion) in [
                    ("build-original-failure", "build", head, "failure"),
                    ("unrelated-candidate-success", "style", candidate.clone(), "success"),
                    ("build-repaired-success", "build", candidate, "success"),
                ] {
                    tx.github().append_github_observation(&StoredGithubObservation {
                        id:id(check_id),repository_id:remote.clone(),kind:"CheckRun".into(),subject_id:"42".into(),
                        remote_revision:check_id.into(),url:format!("{pull}/checks"),head:Some(head),
                        payload:json!({"name":name,"appId":999,"state":"completed","conclusion":conclusion}),
                        source_updated_at_ms:Some(T0+1),observed_at_ms:T0+1,
                    })?;
                }
                Ok(())
            }
        });
        assert!(matches!(readiness("stale-review", &barrier), DomainQueryReply::Result {
            result:OperationResult::ReviewsReadiness {value},..
        } if !value.ready && value.blocker_ids.0 == vec![repository.clone()]));
        let mut candidate_barrier = barrier.clone();
        candidate_barrier["repositories"][0]["expectedHead"] = json!(oid(&candidate));
        let request = |key: &str, args: &str, payload: &Value| {
            let mut body = json!({"kind":"reviews.request","rootId":root,"expectedRevision":"1","policyRevision":"1"});
            body[args] = payload.clone();
            app.command(&fx.user,"reviews.request",json!({
                "mode":"Command","protocol":PROTOCOL,"schemaRevision":15,"requestKey":key,"body":body
            }).to_string().as_bytes()).unwrap()
        };
        let refused = |reply: DomainCommandReply, expected: &str| {
            assert!(matches!(&reply,
                CommandReply::Recorded {receipt,..}
                if matches!(&receipt.decision, DomainDecision::Refused {fault}
                    if fault.target_kind.as_ref().is_some_and(|kind| kind.0 == expected))
            ), "{reply:?}");
        };
        refused(request("review-at-cap", "barrier", &candidate_barrier), "review_round_cap");
        let report = json!({
            "repositoryId":repository,"originalReviewId":review.id,"reviewedHead":oid(&head),
            "candidateHead":oid(&candidate),"preservesProductBehavior":true,"preservesCoverage":true,
            "preservesRequiredGates":true,"rationale":"Only the CI environment fixture changes.",
            "focusedChecks":[{"id":"build-repaired-success","findingsVerified":[]}]
        });
        let save_report = |report: &Value| app.artifacts().publish_evidence(&*fx.store, Evidence {
            scope:ArtifactScope::Project(project.clone()),producer:fx.user.principal().clone(),
            principal_kind:"Human".into(),agent_session:None,root:Some(root.clone()),
            assignment:None,title:"Focused CI verification".into(),media_type:"application/json".into(),
            tags:vec![],source:json!({"kind":"Fixture"}),
        }, report.to_string().as_bytes(), |_,_| Ok(())).unwrap().0;
        let mut unrelated_report = report.clone();
        unrelated_report["focusedChecks"][0]["id"] = json!("unrelated-candidate-success");
        let proof = json!({
            "originalReviewId":review.id,"expectedReviewRevision":review.revision.to_string(),
            "repositoryId":repository,"reviewedHead":oid(&head),"candidateHead":oid(&candidate),
            "diagnoses":[{"failedCheckId":"build-original-failure","diagnosis":"The test runner lacked its CI retry flag."}],
            "changes":[{"changeDigest":change,"reason":{"kind":"RequiredCheck","failedCheckId":"build-original-failure"}}],
            "focusedCheckIds":["build-repaired-success"],"verificationReport":save_report(&report)
        });
        let mut unrelated = proof.clone();
        unrelated["changes"][0]["changeDigest"] = json!("f".repeat(64));
        refused(request("unrelated-repair", "candidateRepair", &unrelated), "review_candidate_delta");
        unrelated = proof.clone();
        unrelated["focusedCheckIds"] = json!(["unrelated-candidate-success"]);
        unrelated["verificationReport"] = json!(save_report(&unrelated_report));
        refused(request("unrelated-check", "candidateRepair", &unrelated), "review_candidate_delta");
        applied(&request("exact-ci-repair", "candidateRepair", &proof));
        assert!(matches!(readiness("verified-ci-repair", &candidate_barrier), DomainQueryReply::Result {
            result:OperationResult::ReviewsReadiness {value},..
        } if value.ready && value.review_ids.0 == vec![review.id]
            && value.candidate_repairs.as_ref().unwrap().0[0].reviewed_head == oid(&head)));
        app.shutdown().await;
    });
}
