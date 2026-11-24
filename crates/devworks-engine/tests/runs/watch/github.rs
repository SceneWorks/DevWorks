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
                    let (status, conclusion) = if requests.len() == 1 {
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
