use super::*;
use devworks_core::ports::http::{
    HttpError, HttpExchange, HttpRequest, HttpResponse, HttpTransport, Method,
};
use devworks_core::protocol::{CommandReply, ProjectId, results::DomainDecision};
use devworks_core::services::git::github::GitHubClient;
use devworks_core::services::github::{GitHubService, issue_poll::IssuePageResult};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Mutex;

struct Request {
    path: &'static str,
    query: &'static str,
    result: HttpExchange,
}

fn exchange(status: u16, value: Value, headers: &[(&str, &str)]) -> HttpExchange {
    HttpExchange {
        response: HttpResponse {
            status,
            body: value.to_string().into_bytes(),
        },
        headers: headers
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
    }
}

struct Script(Mutex<VecDeque<Request>>);

impl HttpTransport for Script {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        self.send_with_headers(request)
            .map(|exchange| exchange.response)
    }

    fn send_with_headers(&self, request: &HttpRequest) -> Result<HttpExchange, HttpError> {
        let is_graphql = request.url == "https://api.github.com/graphql";
        assert_eq!(
            request.method,
            if is_graphql {
                Method::Post
            } else {
                Method::Get
            }
        );
        if is_graphql {
            let variables: Value =
                serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
            assert_eq!(
                variables["variables"]["input"]["repositoryId"],
                "opaque-repository-node"
            );
            assert_eq!(
                variables["variables"]["input"]["title"],
                "Local publication"
            );
            let marker = variables["variables"]["input"]["clientMutationId"]
                .as_str()
                .unwrap();
            assert!(
                variables["variables"]["input"]["body"]
                    .as_str()
                    .unwrap()
                    .ends_with(marker)
            );
        }
        assert_eq!(
            request.header_value("Authorization"),
            Some("Bearer test-private-token")
        );
        assert!(!format!("{request:?}").contains("test-private-token"));
        let expected = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected GitHub request");
        let target = request.url.strip_prefix("https://api.github.com/").unwrap();
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        assert_eq!(Some(path), expected.path.strip_prefix('/'));
        assert!(query.contains(expected.query));
        Ok(expected.result)
    }
}

fn service(
    h: &Harness,
    store: &Arc<dyn ApplicationStore>,
    wire: Arc<Script>,
) -> Arc<GitHubService> {
    Arc::new(GitHubService::new(
        store.clone(),
        Arc::new(GitHubClient {
            http: wire,
            secrets: Arc::new(GitHubSecrets),
        }),
        h.ids.clone(),
        h.clock(),
    ))
}

fn set_up(h: &Harness, store: &Arc<dyn ApplicationStore>) {
    let location = h.root.to_string_lossy().into_owned();
    write(store.as_ref(), move |tx| {
        let schema =
            devworks_core::settings::catalog::descriptor("github.credential").unwrap().schema_ref();
        tx.configuration().append_revision(&ConfigScope::Workspace, "settings", 1, 1, json!({
            "values": {"github.credential": {
                "schema": schema,
                "value": {"secretRef": devworks_core::authority::redact::SecretRef::mint(7).as_str()}
            }}
        }))?;
        tx.projects().insert_repository(&StoredRepository {
            id: id("github-sync-repo"),
            name: "kit".into(),
            location,
            remote_url: Some("https://github.com/Acme/kit.git".into()),
            primary: true,
            lifecycle: "Active".into(),
            languages: Vec::new(),
            revision: 1,
            registered_at_ms: T0,
        })
    }).unwrap();
}

fn issue(state: &str, timestamp: &str) -> Value {
    json!({
        "number": 42,
        "id": 1234,
        "node_id": "issue-node-1234",
        "html_url": "https://github.com/Acme/kit/issues/42",
        "state": state,
        "title": "Report from the remote",
        "body": "A scripted issue report",
        "labels": [{"name": "bug"}],
        "user": {"login": "reporter"},
        "created_at": timestamp,
        "updated_at": timestamp,
        "comments": 0
    })
}

#[test]
fn github_issue_poll_resumes_its_exact_page_and_coalesces_remote_updates_without_admission() {
    let _native_case = native_runtime_case();
    let h = Harness::new();
    let store = h.open();
    set_up(&h, &store);
    let open = issue("open", "2023-11-14T22:13:20Z");
    let mut pull = open.clone();
    pull["pull_request"] = json!({"url": "https://api.github.com/repos/Acme/kit/pulls/42"});
    let script = Arc::new(Script(Mutex::new(VecDeque::from([
        Request {
            path: "/user",
            query: "",
            result: exchange(
                200,
                json!({"login":"signed-in-user", "id":17}),
                &[("x-oauth-scopes", "repo")],
            ),
        },
        Request {
            path: "/repos/Acme/kit",
            query: "",
            result: exchange(
                200,
                json!({
                    "id":12, "node_id":"opaque-repository-node",
                    "name":"kit","full_name":"Acme/kit","owner":{"login":"Acme","type":"Organization"},
                    "html_url":"https://github.com/Acme/kit","default_branch":"main",
                    "archived":false, "disabled":false, "permissions":{"pull":true,"push":false},
                    "temp_clone_token":"not-a-stored-fact"
                }),
                &[],
            ),
        },
        Request {
            path: "/repos/Acme/kit/issues",
            query: "state=open",
            result: exchange(
                200,
                json!([open.clone(), pull]),
                &[
                    ("etag", "\"open-1\""),
                    (
                        "link",
                        "<https://api.github.com/repositories/12/issues?state=open&page=2>; rel=\"next\"",
                    ),
                    ("x-poll-interval", "75"),
                ],
            ),
        },
        Request {
            path: "/repositories/12/issues",
            query: "page=2",
            result: exchange(200, json!([open]), &[]),
        },
        Request {
            path: "/repos/Acme/kit/issues",
            query: "state=all",
            result: exchange(200, json!([issue("closed", "2023-11-14T22:15:22Z")]), &[]),
        },
    ]))));
    let git = service(&h, &store, script.clone());
    let actor = principal(store.as_ref(), "github-human", PrincipalKind::Human);
    let authority = HostAuthority {
        store: store.as_ref(),
        clock: &*h.clock,
        protected_branches: &[],
    };
    let gateway = Gateway {
        store: store.as_ref(),
        authority: &authority,
        ids: &*h.ids,
    };
    let refresh = serde_json::to_vec(&json!({
        "mode":"Command",
        "protocol":"1.0-draft.1",
        "requestKey":"connecting-github",
        "body":{"kind":"github.account","action":"Refresh"}
    }))
    .unwrap();
    let result = gateway
        .command(&actor, "github.account", &refresh, git.clone())
        .unwrap();
    assert!(matches!(result, CommandReply::Recorded {
        receipt, .. } if matches!(receipt.decision, DomainDecision::Applied { .. })));
    let repo = id("github-sync-repo");
    let binding = git.qualify_registration(&repo).unwrap();
    let scope = GithubScope {
        repositories: vec![repo.clone()],
        project_id: None,
    };
    let identity = read(store.as_ref(), |snapshot| {
        snapshot
            .github()
            .github_observation(&binding.observation_id, &scope)
    })
    .unwrap();
    assert!(!identity.payload.to_string().contains("not-a-stored-fact"));
    assert_eq!(identity.subject_id, "opaque-repository-node");
    assert!(matches!(git.poll_issue_page(&repo).unwrap(),
        IssuePageResult::Captured { reports: 1, next_due_at_ms } if next_due_at_ms == T0 + 75_000));
    assert_eq!(git.poll_issue_page(&repo).unwrap(), IssuePageResult::NotDue);
    let key = GithubIssueKey {
        repository_id: binding.github_repository_id.clone(),
        number: 42,
    };
    let first = read(store.as_ref(), |snapshot| {
        snapshot.github().github_issue(&key, &scope)
    })
    .unwrap();
    drop(git);
    drop(store);
    h.advance(75_000);

    let store = h.open();
    let git = service(&h, &store, script.clone());
    assert!(matches!(
        git.poll_issue_page(&repo).unwrap(),
        IssuePageResult::Captured { reports: 0, .. }
    ));
    assert_eq!(
        read(store.as_ref(), |snapshot| snapshot
            .github()
            .github_issue(&key, &scope))
        .unwrap(),
        first
    );
    h.advance(60_000);
    assert!(matches!(
        git.poll_issue_page(&repo).unwrap(),
        IssuePageResult::Captured { reports: 1, .. }
    ));
    let updated = read(store.as_ref(), |snapshot| {
        snapshot.github().github_issue(&key, &scope)
    })
    .unwrap();
    assert_eq!(updated.state, "Queued");
    assert_eq!(updated.revision, first.revision + 1);
    assert_ne!(updated.observation_id, first.observation_id);
    assert!(updated.operation_id.is_none());
    let clock = read(store.as_ref(), |snapshot| {
        snapshot.github().github_intake_clock()
    });
    assert_eq!(clock.last_admitted_at_ms, None);
    let observation = read(store.as_ref(), |snapshot| {
        snapshot
            .github()
            .github_observation(&updated.observation_id, &scope)
    })
    .unwrap();
    assert_eq!(observation.payload["state"], "closed");
    let control = Arc::new(devworks_core::services::runs::ControlService::new(
        h.ids.clone(),
    ));
    let admitted = git.admit_triage(control.clone()).unwrap().unwrap();
    assert_eq!(admitted.issue.key, key);
    assert_eq!(admitted.issue.state, "Triaging");
    assert_eq!(
        admitted.operation.payload["sourceRevision"],
        updated.remote_revision
    );
    assert_eq!(admitted.clock.last_admitted_at_ms, Some(T0 + 135_000));
    assert_eq!(
        admitted.clock.next_admission_at_ms,
        T0 + 135_000 + 1_800_000
    );
    assert!(git.admit_triage(control).unwrap().is_none());
    let ready = read(store.as_ref(), |snapshot| {
        snapshot.queue().deliveries(
            &DeliveryFilter {
                consumer: Some(devworks_core::services::github::triage::CONSUMER.into()),
                operation_id: Some(admitted.operation.id.clone()),
                ..DeliveryFilter::default()
            },
            None,
            10,
        )
    });
    assert_eq!(ready.entries.len(), 1);
    let head = |oid: &str| {
        json!({
            "ref": "refs/heads/main", "object":{"type":"commit", "sha":oid}
        })
    };
    let ref_request = |result| Request {
        path: "/repos/Acme/kit/git/ref/heads/main",
        query: "",
        result,
    };
    script.0.lock().unwrap().extend([
        ref_request(exchange(200, head(&"a".repeat(40)), &[("etag", "a")])),
        ref_request(exchange(200, head(&"b".repeat(40)), &[("etag", "b")])),
        ref_request(exchange(304, Value::Null, &[])),
    ]);
    let deliveries = |consumer: &str| {
        read(store.as_ref(), |snapshot| {
            snapshot.queue().deliveries(
                &DeliveryFilter {
                    consumer: Some(consumer.into()),
                    ..DeliveryFilter::default()
                },
                None,
                10,
            )
        })
        .entries
    };
    let notices = devworks_core::services::github::references::NOTICE_CONSUMER;
    let indexes = devworks_core::services::github::references::INDEX_CONSUMER;
    assert!(matches!(
        git.poll_default_ref(&repo).unwrap(),
        IssuePageResult::Captured { reports: 0, .. }
    ));
    assert!(deliveries(notices).is_empty());
    assert!(deliveries(indexes).is_empty());
    h.advance(60_000);
    assert!(matches!(
        git.poll_default_ref(&repo).unwrap(),
        IssuePageResult::Captured { reports: 1, .. }
    ));
    assert_eq!(deliveries(notices).len(), 1);
    assert_eq!(deliveries(indexes).len(), 1);
    assert_eq!(
        deliveries(notices)[0].event_id,
        deliveries(indexes)[0].event_id
    );
    assert_eq!(deliveries(indexes)[0].payload["head"], "b".repeat(40));
    h.advance(60_000);
    assert!(matches!(
        git.poll_default_ref(&repo).unwrap(),
        IssuePageResult::Captured { reports: 0, .. }
    ));
    assert_eq!(deliveries(notices).len(), 1);
    assert_eq!(deliveries(indexes).len(), 1);
    assert!(script.0.lock().unwrap().is_empty());
}

#[test]
fn github_issue_link_requires_revision_two_and_the_exact_current_work() {
    let _native_case = native_runtime_case();
    let h = Harness::new();
    let store = h.open();
    set_up(&h, &store);
    write(store.as_ref(), |tx| {
        let project = ProjectId(id("github-project"));
        tx.projects().insert_project(&StoredProject {
            id: project.clone(),
            name: "GitHub project".into(),
            description: String::new(),
            lifecycle: ACTIVE.into(),
            revision: 1,
            scope_revision: 1,
            created_at_ms: T0,
            updated_at_ms: T0,
            archived_at_ms: None,
            deleted_at_ms: None,
            archive_epoch: 0,
            archive_stop_pending: false,
        })?;
        tx.projects()
            .set_members(&project, &[id("github-sync-repo")])?;
        let work = StoredWork {
            id: id("github-work"),
            project_id: project,
            kind: TASK.into(),
            parent_feature_id: None,
            status_id: None,
            title: "Local work title".into(),
            priority: 0,
            manual_order: 0,
            revision: 1,
            scope_revision: 1,
            created_at_ms: T0,
            updated_at_ms: T0,
            created_by: None,
            updated_by: None,
            search_key: "local work title".into(),
            caller_ref: None,
            completion: None,
            brief: json!({}),
        };
        tx.work().insert_work(&work)?;
        tx.work().insert_work(&StoredWork {
            id: id("github-publication"),
            title: "Local publication".into(),
            ..work
        })
    })
    .unwrap();
    let script = Arc::new(Script(Mutex::new(VecDeque::from([
        Request {
            path: "/user",
            query: "",
            result: exchange(200, json!({"login":"signed-in-user", "id":17}), &[]),
        },
        Request {
            path: "/repos/Acme/kit",
            query: "",
            result: exchange(
                200,
                json!({
                    "id":12, "node_id":"opaque-repository-node", "name":"kit", "full_name":"Acme/kit",
                    "owner":{"login":"Acme","type":"Organization"},
                    "html_url":"https://github.com/Acme/kit","default_branch":"main",
                    "archived":false,"disabled":false,"permissions":{"pull":true}
                }),
                &[],
            ),
        },
        Request {
            path: "/repos/Acme/kit/issues/42",
            query: "",
            result: exchange(200, issue("closed", "2023-11-14T22:13:20Z"), &[]),
        },
    ]))));
    let github = service(&h, &store, script.clone());
    let actor = principal(store.as_ref(), "github-link-human", PrincipalKind::Human);
    let authority = HostAuthority {
        store: store.as_ref(),
        clock: &*h.clock,
        protected_branches: &[],
    };
    let gateway = Gateway {
        store: store.as_ref(),
        authority: &authority,
        ids: &*h.ids,
    };
    let sign_in = serde_json::to_vec(&json!({
        "mode":"Command", "protocol":"1.0-draft.1","requestKey":"github-link-auth",
        "body":{"kind":"github.account","action":"Refresh"}
    }))
    .unwrap();
    assert!(matches!(
        gateway.command(&actor, "github.account", &sign_in, github.clone()).unwrap(),
        CommandReply::Recorded { receipt, .. } if matches!(receipt.decision, DomainDecision::Applied { .. })
    ));
    let repo = id("github-sync-repo");
    let binding = github.qualify_registration(&repo).unwrap();
    let command = |schema, revision, key: &str| {
        let frame = serde_json::to_vec(&json!({
            "mode":"Command", "protocol":"1.0-draft.1", "schemaRevision":schema, "requestKey":key,
            "body":{
                "kind":"issues.link", "issueId":"github-work", "repositoryId":"github-sync-repo",
                "expectedRevision":revision, "remoteIssueNumber":"42"
            }
        }))
        .unwrap();
        gateway
            .command(&actor, "issues.link", &frame, github.clone())
            .unwrap()
    };
    assert!(matches!(
        command(1, "1", "link-v1"),
        CommandReply::Recorded { receipt, .. }
            if matches!(receipt.decision, DomainDecision::Refused { .. })
    ));
    assert!(matches!(
        command(2, "2", "link-stale"),
        CommandReply::Recorded { receipt, .. }
            if matches!(receipt.decision, DomainDecision::Refused { .. })
    ));
    assert_eq!(script.0.lock().unwrap().len(), 1);
    match command(2, "1", "link-current") {
        CommandReply::Recorded { receipt, .. } => {
            let DomainDecision::Applied {
                result: OperationResult::IssuesLink { value },
            } = receipt.decision
            else {
                panic!("missing linked issue result")
            };
            let value = serde_json::to_value(value).unwrap();
            assert_eq!(value["url"], "https://github.com/Acme/kit/issues/42");
            assert_eq!(value["remoteIssueId"], "issue-node-1234");
            assert_eq!(value["remoteRepositoryId"], "opaque-repository-node");
            assert_eq!(value["workId"], "github-work");
        }
        other => panic!("unexpected link reply {other:?}"),
    }
    assert!(script.0.lock().unwrap().is_empty());
    let key = GithubIssueKey {
        repository_id: binding.github_repository_id,
        number: 42,
    };
    let links = read(store.as_ref(), |snapshot| {
        snapshot.github().github_work_links(&key, None, None, 2)
    });
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].work_id.0, id("github-work"));
    let work = read(store.as_ref(), |snapshot| {
        snapshot.work().work(&id("github-work"))
    })
    .unwrap();
    assert_eq!(work.title, "Local work title");
    assert_eq!(work.revision, 1);

    let publish = |request_key: &str| {
        let frame = serde_json::to_vec(&json!({
            "mode":"Command","protocol":"1.0-draft.1","schemaRevision":2,"requestKey":request_key,
            "body":{
                "kind":"issues.publish","issueId":"github-publication","repositoryId":"github-sync-repo",
                "expectedRevision":"1"
            }
        })).unwrap();
        match gateway
            .command(&actor, "issues.publish", &frame, github.clone())
            .unwrap()
        {
            CommandReply::Recorded { receipt, .. } => match receipt.decision {
                DomainDecision::Accepted { operation_id } => operation_id,
                other => panic!("unexpected publication decision {other:?}"),
            },
            other => panic!("unexpected publication reply {other:?}"),
        }
    };
    let publication = publish("publish-1");
    assert_eq!(publish("publish-duplicate"), publication);
    let before = read(store.as_ref(), |snapshot| {
        snapshot.operations().operation(&publication)
    })
    .unwrap();
    let marker = before.payload["marker"].as_str().unwrap();
    let mut matching = issue("open", "2023-11-14T22:13:20Z");
    matching["node_id"] = json!("issue-node-1235");
    matching["number"] = json!(43);
    matching["html_url"] = json!("https://github.com/Acme/kit/issues/43");
    matching["body"] = json!(format!(
        "Published body

{marker}"
    ));
    matching["user"]["id"] = json!(17);
    script.0.lock().unwrap().extend([
        Request {
            path: "/graphql",
            query: "",
            result: exchange(502, Value::Null, &[]),
        },
        Request {
            path: "/repos/Acme/kit/issues",
            query: "state=all",
            result: exchange(200, json!([matching]), &[]),
        },
    ]);
    let outbox = dispatcher(
        &h,
        store.clone(),
        vec![devworks_engine::github::consumer(github.clone())],
    );
    runtime().block_on(outbox.run_until_idle()).unwrap();
    let after = read(store.as_ref(), |snapshot| {
        snapshot.operations().operation(&publication)
    })
    .unwrap();
    assert_eq!(
        after.state,
        devworks_core::protocol::OperationState::OutcomeUnknown
    );
    runtime().block_on(outbox.observe_unknown()).unwrap();
    let confirmed = read(store.as_ref(), |snapshot| {
        snapshot.operations().operation(&publication)
    })
    .unwrap();
    assert_eq!(
        confirmed.state,
        devworks_core::protocol::OperationState::ObservedComplete
    );
    assert_eq!(confirmed.attempts.len(), 1);
    let published_source_key = GithubIssueKey {
        repository_id: key.repository_id,
        number: 43,
    };
    let links = read(store.as_ref(), |snapshot| {
        snapshot
            .github()
            .github_work_links(&published_source_key, None, None, 2)
    });
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].work_id.0, id("github-publication"));
    assert!(script.0.lock().unwrap().is_empty());
}
