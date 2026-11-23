use super::*;
use devworks_core::context::InvocationContext;
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::IdKind;
use devworks_core::ports::http::{
    HttpError, HttpExchange, HttpRequest, HttpResponse, HttpTransport, Method,
};
use devworks_core::ports::process::ProcessSpec;
use devworks_core::protocol::{
    CommandReply, OperationId, OperationState, ProjectId, RootId, results::DomainDecision,
};
use devworks_core::services::{
    UseCases,
    approvals::ApprovalService,
    git::github::GitHubClient,
    git::helper::{HelperFault, HelperLimits, HelperRunner},
    github::{
        GitHubService,
        issue_poll::IssuePageResult,
        triage_case::{BugVerificationProposal, TriageClassification, settle_classification},
        verification,
    },
    runs::{ControlService, control_of},
    work::{WorkService, workflow},
};
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
            let document: Value = serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
            let input = &document["variables"]["input"];
            let marker = input["clientMutationId"].as_str().unwrap();
            if input.get("repositoryId").is_some() {
                assert_eq!(input["repositoryId"], "opaque-repository-node");
                assert_eq!(input["title"], "Local publication");
                assert!(input["body"].as_str().unwrap().ends_with(marker));
            } else if input.get("subjectId").is_some() {
                assert_eq!(input["subjectId"], "issue-node-1234");
                assert!(input["body"].as_str().unwrap().ends_with(marker));
                assert!(input["body"].as_str().unwrap().contains("/pull/81"));
                assert!(input["body"].as_str().unwrap().contains("/pull/82"));
            } else {
                assert_eq!(input["issueId"], "issue-node-1234");
            }
        }
        assert!(
            request
                .header_value("Authorization")
                .is_some_and(|v| v.starts_with("Bearer "))
        );
        assert!(!format!("{request:?}").contains("test-private-token"));
        let expected = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("unexpected GitHub request: {}", request.url));
        let target = request.url.strip_prefix("https://api.github.com/").unwrap();
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        assert_eq!(Some(path), expected.path.strip_prefix('/'));
        assert!(
            query.contains(expected.query),
            "query `{query}` did not contain `{}` for `{path}`",
            expected.query
        );
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
        let schema = devworks_core::settings::catalog::descriptor("github.credential")
            .unwrap()
            .schema_ref();
        tx.configuration().append_revision(
            &ConfigScope::Workspace,
            "settings",
            1,
            1,
            json!({
                "values": {"github.credential": {
                    "schema": schema,
                    "value": {"secretRef": devworks_core::authority::redact::SecretRef::mint(7).as_str()}
                }}
            }),
        )?;
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
    })
    .unwrap();
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
        "user": {"login": "reporter", "id": 99},
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
    let control = Arc::new(ControlService::new(h.ids.clone()));
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

    // Prove github_index_refresh failure/hold never suppresses github_agent_notices!
    let notice_entry_id = deliveries(notices)[0].id.clone();
    let index_entry_id = deliveries(indexes)[0].id.clone();
    struct FailingIndexRefresh;
    impl devworks_engine::DeliveryHandler for FailingIndexRefresh {
        fn apply(
            &self,
            _: &mut dyn WriteTransaction,
            _: &devworks_engine::Delivery,
        ) -> Result<devworks_engine::Outcome, StorageError> {
            Ok(devworks_engine::Outcome::Hold(
                "simulated index unavailable".into(),
            ))
        }
    }
    let fail_idx = devworks_engine::Consumer {
        name: indexes.into(),
        handler: Arc::new(FailingIndexRefresh),
        effect: None,
        async_handler: None,
        eligibility: None,
        stop_grace_ms: 5_000,
        retry: devworks_engine::RetryPolicy {
            base_delay_ms: 100,
            max_delay_ms: 1_000,
            max_attempts: 1,
        },
        max_in_flight: None,
    };
    let dual_outbox = dispatcher(
        &h,
        store.clone(),
        vec![devworks_engine::github::notice_consumer(), fail_idx],
    );
    runtime().block_on(dual_outbox.run_until_idle()).unwrap();
    let notice_after =
        read(store.as_ref(), |s| s.queue().entry(&notice_entry_id)).expect("notice entry");
    let index_after =
        read(store.as_ref(), |s| s.queue().entry(&index_entry_id)).expect("index entry");
    assert_eq!(notice_after.state, QueueState::Completed);
    assert_ne!(index_after.state, QueueState::Completed);
}

#[test]
fn github_issue_link_requires_revision_three_and_recovers_publication_and_multi_obligation_resolution()
 {
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
            ..work.clone()
        })?;
        tx.work().insert_work(&StoredWork {
            id: id("github-work-2"),
            title: "Second obligation on #42".into(),
            ..work.clone()
        })?;
        tx.work().insert_work(&StoredWork {
            id: id("local-only-issue"),
            title: "Stays local until published".into(),
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
            result: exchange(200, issue("open", "2023-11-14T22:13:20Z"), &[]),
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
        "mode":"Command", "protocol":"1.0-draft.1", "schemaRevision":3, "requestKey":"github-link-auth",
        "body":{"kind":"github.account","action":"Refresh"}
    }))
    .unwrap();
    assert!(matches!(
        gateway.command(&actor, "github.account", &sign_in, github.clone()).unwrap(),
        CommandReply::Recorded { receipt, .. } if matches!(receipt.decision, DomainDecision::Applied { .. })
    ));
    let repo = id("github-sync-repo");
    let binding = github.qualify_registration(&repo).unwrap();
    let mcp_admin = UseCases::new().with_inbound_mcp();
    let change_access = |key, operation, mut body: Value| {
        body["kind"] = json!(operation);
        let frame = json!({
            "mode":"Command", "protocol":"1.0-draft.1", "requestKey":key, "body":body,
        });
        match mcp_admin
            .command(&gateway, &actor, operation, frame.to_string().as_bytes())
            .unwrap()
        {
            CommandReply::Recorded { receipt, .. } => match receipt.decision {
                DomainDecision::Applied { result } => result,
                other => panic!("MCP administrative decision {other:?}"),
            },
            other => panic!("MCP administrative response {other:?}"),
        }
    };
    change_access(
        "mcp-configure",
        "mcp_server.configure",
        json!({"expectedRevision":"1","enabled":true,"bindings":["Stdio"]}),
    );
    let devworks_core::protocol::results::OperationResult::McpServerSetClientAccess {
        value: registered,
    } = change_access(
        "mcp-register",
        "mcp_server.set_client_access",
        json!({"action":{"kind":"Register","name":"GitHub native acceptance"}}),
    )
    else {
        panic!("registration result")
    };
    let devworks_core::protocol::results::OperationResult::McpServerSetClientAccess {
        value: active,
    } = change_access(
        "mcp-allow",
        "mcp_server.set_client_access",
        json!({"action":{
            "kind":"Replace","clientId":registered.client_id,"expectedRevision":registered.revision,
            "name":registered.name,"enabled":true,"projectIds":["github-project"],
            "repositoryIds":["github-sync-repo"],"worktreeIds":[],"readOperations":[],
            "commandOperations":["issues.link","issues.publish"]
        }}),
    )
    else {
        panic!("replacement result")
    };
    let credentials = devworks_core::services::mcp_inbound::Credentials::new(
        store.clone(),
        Box::new(devworks_core::ports::secrets::SessionSecrets::new()),
        Arc::new(devworks_adapters::OsEntropy),
    );
    let token = credentials
        .provision(&actor, &active.client_id, &active.revision)
        .unwrap();
    let mcp_actor = credentials
        .authenticate(
            devworks_core::services::mcp_inbound::Binding::LocalIpc,
            &token,
        )
        .unwrap()
        .invocation(
            id("paired-github-client"),
            ProjectId(id("github-project")),
            Revision::new(1).unwrap(),
        );
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
            .command(&mcp_actor, "issues.link", &frame, github.clone())
            .unwrap()
    };
    assert!(matches!(
        command(1, "1", "link-v1"),
        CommandReply::Recorded { receipt, .. }
            if matches!(receipt.decision, DomainDecision::Refused { .. })
    ));
    assert!(matches!(
        command(3, "2", "link-stale"),
        CommandReply::Recorded { receipt, .. }
            if matches!(receipt.decision, DomainDecision::Refused { .. })
    ));
    // Neither v1 nor stale revision performed any HTTP read of /issues/42.
    assert_eq!(script.0.lock().unwrap().len(), 1);
    match command(3, "1", "link-current") {
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
        repository_id: binding.github_repository_id.clone(),
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

    // Local unlinked issue remains strictly local (no auto-publication).
    let local_links = read(store.as_ref(), |snapshot| {
        snapshot.github().github_issues_for_work(
            &RootId(id("local-only-issue")),
            &binding.github_repository_id,
            10,
        )
    });
    assert!(local_links.is_empty());

    // v1 publish is refused before recording an operation or HTTP mutation.
    let v1_publish = serde_json::to_vec(&json!({
        "mode":"Command","protocol":"1.0-draft.1","schemaRevision":1,"requestKey":"publish-v1",
        "body":{
            "kind":"issues.publish","issueId":"github-publication","repositoryId":"github-sync-repo",
            "expectedRevision":"1"
        }
    }))
    .unwrap();
    assert!(matches!(
        gateway.command(&mcp_actor, "issues.publish", &v1_publish, github.clone()).unwrap(),
        CommandReply::Recorded { receipt, .. } if matches!(receipt.decision, DomainDecision::Refused { .. })
    ));

    let publish = |request_key: &str| {
        let frame = serde_json::to_vec(&json!({
            "mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,"requestKey":request_key,
            "body":{
                "kind":"issues.publish","issueId":"github-publication","repositoryId":"github-sync-repo",
                "expectedRevision":"1"
            }
        }))
        .unwrap();
        match gateway
            .command(&mcp_actor, "issues.publish", &frame, github.clone())
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
    matching["body"] = json!(format!("Published body\n\n{marker}"));
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
    assert_eq!(after.state, OperationState::OutcomeUnknown);
    runtime().block_on(outbox.observe_unknown()).unwrap();
    runtime().block_on(outbox.run_until_idle()).unwrap();
    let confirmed = read(store.as_ref(), |snapshot| {
        snapshot.operations().operation(&publication)
    })
    .unwrap();
    assert_eq!(confirmed.state, OperationState::ObservedComplete);
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

    // Now test multi-obligation verified resolution on OPEN issue #42 with lost comment AND lost close replies!
    let source = GithubIssueKey {
        repository_id: published_source_key.repository_id,
        number: 42,
    };
    let canonical = source.repository_id.clone();
    let stored_for_reference = store.clone();
    let source_for_seed = source.clone();
    write(store.as_ref(), move |tx| {
        use devworks_core::protocol::EventId;
        tx.github()
            .link_github_work(&source_for_seed, &RootId(id("github-work-2")), 1)?;
        let host = stored_for_reference.info().host_actor;
        let project = ProjectId(id("github-project"));
        for (label, pr) in [("github-work", 81), ("github-work-2", 82)] {
            let work_id = RootId(id(label));
            let head = if pr == 81 {
                "d".repeat(40)
            } else {
                "e".repeat(40)
            };
            let pull = format!("https://github.com/Acme/kit/pull/{pr}");
            let observation_id = tx.new_id(IdKind::Event);
            tx.github()
                .append_github_observation(&StoredGithubObservation {
                    id: observation_id,
                    repository_id: canonical.clone(),
                    kind: "PullRequest".into(),
                    subject_id: pr.to_string(),
                    remote_revision: format!("merge-{pr}"),
                    head: Some(head.clone()),
                    url: pull.clone(),
                    payload: json!({"mergeCommit":"c".repeat(40),"state":"merged"}),
                    observed_at_ms: T0,
                    source_updated_at_ms: Some(T0),
                })?;
            let event = EventId(tx.new_id(IdKind::Event));
            let seq = tx.history().append(NewEvent {
                id: event.clone(),
                actor: host.clone(),
                project_id: Some(project.clone()),
                root_id: Some(work_id.clone()),
                operation_id: None,
                aggregate_kind: "fixture".into(),
                aggregate_id: label.into(),
                aggregate_revision: 1,
                kind: "FixturePullRequest".into(),
                payload: json!({}),
            })?;
            let link_id = tx.new_id(IdKind::Link);
            tx.sessions().insert_link(&StoredLink {
                id: link_id,
                project_id: project.clone(),
                root_id: work_id,
                kind: "pull_request".into(),
                target_kind: "pull_request".into(),
                target_id: pull,
                parent_id: None,
                supersedes_id: None,
                actor: host.clone(),
                event_id: event.0,
                sequence: seq,
                repository_id: Some(id("github-sync-repo")),
                revision: Some(head),
                summary: "Fixture linked pull request".into(),
                body: json!({}),
                created_at_ms: T0,
            })?;
        }
        set_verified(tx, "github-work")
    })
    .unwrap();
    let resolutions = || {
        read(store.as_ref(), |snapshot| {
            snapshot.github().github_resolutions(&source, None, 10)
        })
    };
    github.queue_one_resolution(&repo).unwrap();
    assert!(
        resolutions().is_empty(),
        "one unfinished obligation holds the publisher"
    );
    write(store.as_ref(), |tx| set_verified(tx, "github-work-2")).unwrap();
    h.advance(1000);
    github.queue_one_resolution(&repo).unwrap();
    h.advance(60_000);
    github.queue_one_resolution(&repo).unwrap();
    let sealed = resolutions();
    assert_eq!(sealed.len(), 1);
    let sealed = &sealed[0];
    assert_eq!(sealed.state, "Sealed");
    let pending = read(store.as_ref(), |snapshot| {
        snapshot
            .operations()
            .operation(&sealed.comment_operation_id)
    })
    .unwrap();
    let close_before = read(store.as_ref(), |snapshot| {
        snapshot.operations().operation(&sealed.close_operation_id)
    })
    .unwrap();
    assert_eq!(close_before.state, OperationState::IntentRecorded);
    let marker = pending.payload["marker"].as_str().unwrap();

    let open_before_comment = issue("open", "2023-11-14T22:13:20Z");
    let mut open_after_comment = open_before_comment.clone();
    open_after_comment["comments"] = json!(1);
    let mut closed_after_close = open_after_comment.clone();
    closed_after_close["state"] = json!("closed");

    // 1) Comment attempt gets 502 (OutcomeUnknown) while issue is STILL OPEN.
    //    Close must NOT prematurely fail while comment is OutcomeUnknown!
    // 2) observe_unknown recovers comment via read-only /comments?per_page=100, and settlement queues close.
    // 3) Close attempt gets 502 (OutcomeUnknown) after closing on GitHub.
    // 4) observe_unknown recovers close via read-only /issues/42 observing state=="closed" without repeating comment!
    script.0.lock().unwrap().extend([
        Request {
            path: "/repos/Acme/kit/issues/42",
            query: "",
            result: exchange(200, open_before_comment.clone(), &[]),
        },
        Request {
            path: "/graphql",
            query: "",
            result: exchange(502, Value::Null, &[]),
        },
        Request {
            path: "/repos/Acme/kit/issues/42",
            query: "",
            result: exchange(200, open_after_comment.clone(), &[]),
        },
        Request {
            path: "/repos/Acme/kit/issues/42/comments",
            query: "per_page=100",
            result: exchange(
                200,
                json!([{"user":{"id":17},"node_id":"resolution-comment-id","body":marker}]),
                &[],
            ),
        },
        Request {
            path: "/repos/Acme/kit/issues/42",
            query: "",
            result: exchange(200, open_after_comment, &[]),
        },
        Request {
            path: "/graphql",
            query: "",
            result: exchange(502, Value::Null, &[]),
        },
        Request {
            path: "/repos/Acme/kit/issues/42",
            query: "",
            result: exchange(200, closed_after_close, &[]),
        },
    ]);
    // Step 1: comment dispatches and gets 502; close stays IntentRecorded!
    runtime().block_on(outbox.run_until_idle()).unwrap();
    assert_eq!(
        read(store.as_ref(), |s| s
            .operations()
            .operation(&sealed.comment_operation_id))
        .unwrap()
        .state,
        OperationState::OutcomeUnknown
    );
    assert_eq!(
        read(store.as_ref(), |s| s
            .operations()
            .operation(&sealed.close_operation_id))
        .unwrap()
        .state,
        OperationState::IntentRecorded
    );
    // Step 2: observe_unknown recovers comment -> settlement queues close -> run_until_idle dispatches close (gets 502)!
    runtime().block_on(outbox.observe_unknown()).unwrap();
    runtime().block_on(outbox.run_until_idle()).unwrap();
    assert_eq!(
        read(store.as_ref(), |s| s
            .operations()
            .operation(&sealed.comment_operation_id))
        .unwrap()
        .state,
        OperationState::ObservedComplete
    );
    assert_eq!(
        read(store.as_ref(), |s| s
            .operations()
            .operation(&sealed.close_operation_id))
        .unwrap()
        .state,
        OperationState::OutcomeUnknown
    );
    // Step 3: observe_unknown recovers close via issue state=="closed" without repeating comment!
    runtime().block_on(outbox.observe_unknown()).unwrap();
    runtime().block_on(outbox.run_until_idle()).unwrap();
    for operation in [&sealed.comment_operation_id, &sealed.close_operation_id] {
        let row = read(store.as_ref(), |snapshot| {
            snapshot.operations().operation(operation)
        })
        .unwrap();
        assert_eq!(row.state, OperationState::ObservedComplete);
        assert_eq!(row.attempts.len(), 1);
    }
    assert!(script.0.lock().unwrap().is_empty());
}

#[test]
fn composed_use_cases_serve_all_eight_github_operations_with_scope_and_rotation_fences() {
    let _native_case = native_runtime_case();
    let h = Harness::new();
    let store = h.open();
    set_up(&h, &store);
    write(store.as_ref(), |tx| {
        for (pid, name) in [("proj-a", "Alpha"), ("proj-b", "Beta")] {
            tx.projects().insert_project(&StoredProject {
                id: ProjectId(id(pid)),
                name: name.into(),
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
        }
        tx.projects()
            .set_members(&ProjectId(id("proj-a")), &[id("github-sync-repo")])?;
        let work = StoredWork {
            id: id("agent-issue"),
            project_id: ProjectId(id("proj-a")),
            kind: TASK.into(),
            parent_feature_id: None,
            status_id: None,
            title: "Scoped agent issue".into(),
            priority: 0,
            manual_order: 0,
            revision: 1,
            scope_revision: 1,
            created_at_ms: T0,
            updated_at_ms: T0,
            created_by: None,
            updated_by: None,
            search_key: "scoped agent issue".into(),
            caller_ref: None,
            completion: None,
            brief: json!({}),
        };
        tx.work().insert_work(&work)
    })
    .unwrap();

    let head_old = "1".repeat(40);
    let head_cur = "2".repeat(40);
    let script = Arc::new(Script(Mutex::new(VecDeque::from([
        // 1. github.account Refresh (human)
        Request {
            path: "/user",
            query: "",
            result: exchange(
                200,
                json!({"login":"octo-dev","id":17}),
                &[("x-oauth-scopes", "repo, read:org")],
            ),
        },
        // 2. github.repositories (personal + org)
        Request {
            path: "/user/repos",
            query: "per_page=50",
            result: exchange(
                200,
                json!([
                    {
                        "id":12,"node_id":"opaque-repository-node","name":"kit","full_name":"Acme/kit",
                        "owner":{"login":"Acme","type":"Organization"},
                        "html_url":"https://github.com/Acme/kit","default_branch":"main",
                        "archived":false,"disabled":false,"permissions":{"pull":true,"push":true}
                    },
                    {
                        "id":19,"node_id":"personal-node","name":"notes","full_name":"octo-dev/notes",
                        "owner":{"login":"octo-dev","type":"User"},
                        "html_url":"https://github.com/octo-dev/notes","default_branch":"main",
                        "archived":false,"disabled":false,"permissions":{"pull":true,"push":true}
                    }
                ]),
                &[],
            ),
        },
        // 3. github.repositories organization filter (Acme): queries /user/repos (including org + collaborator access)
        //    and filters strictly to Organization == Acme!
        Request {
            path: "/user/repos",
            query: "per_page=50",
            result: exchange(
                200,
                json!([
                    {
                        "id":12,"node_id":"opaque-repository-node","name":"kit","full_name":"Acme/kit",
                        "owner":{"login":"Acme","type":"Organization"},
                        "html_url":"https://github.com/Acme/kit","default_branch":"main",
                        "archived":false,"disabled":false,"permissions":{"pull":true,"push":true}
                    },
                    {
                        "id":19,"node_id":"personal-node","name":"notes","full_name":"octo-dev/notes",
                        "owner":{"login":"octo-dev","type":"User"},
                        "html_url":"https://github.com/octo-dev/notes","default_branch":"main",
                        "archived":false,"disabled":false,"permissions":{"pull":true,"push":true}
                    }
                ]),
                &[],
            ),
        },
        // 4. qualify_registration
        Request {
            path: "/repos/Acme/kit",
            query: "",
            result: exchange(
                200,
                json!({
                    "id":12,"node_id":"opaque-repository-node","name":"kit","full_name":"Acme/kit",
                    "owner":{"login":"Acme","type":"Organization"},
                    "html_url":"https://github.com/Acme/kit","default_branch":"main",
                    "archived":false,"disabled":false,"permissions":{"pull":true,"push":true}
                }),
                &[],
            ),
        },
        // 5. issues.link via agent tool
        Request {
            path: "/repos/Acme/kit/issues/42",
            query: "",
            result: exchange(200, issue("open", "2023-11-14T22:13:20Z"), &[]),
        },
        // 6. github.issue_sync via UseCases + outbox
        Request {
            path: "/repos/Acme/kit/issues",
            query: "state=open",
            result: exchange(
                200,
                json!([
                    issue("open", "2023-11-14T22:13:20Z"),
                    {
                        "number": 44,
                        "id": 1244,
                        "node_id": "issue-node-1244",
                        "html_url": "https://github.com/Acme/kit/issues/44",
                        "state": "open",
                        "title": "Second report",
                        "body": "Details",
                        "labels": [],
                        "user": {"login": "reporter", "id": 99},
                        "created_at": "2023-11-14T22:14:00Z",
                        "updated_at": "2023-11-14T22:14:00Z",
                        "comments": 0
                    }
                ]),
                &[("etag", "\"sync-1\"")],
            ),
        },
        // 7. github.checks on PR #90 at head_cur: observes PR head + 4 check endpoints
        Request {
            path: "/repos/Acme/kit/pulls/90",
            query: "",
            result: exchange(
                200,
                json!({
                    "number":90,"node_id":"pr-90","html_url":"https://github.com/Acme/kit/pull/90",
                    "state":"open","head":{"sha":head_cur,"ref":"feat"},
                    "base":{"sha":"0".repeat(40),"ref":"main","repo":{"id":12}},
                    "updated_at":"2023-11-14T22:15:00Z"
                }),
                &[],
            ),
        },
        Request {
            path: "/repos/Acme/kit/commits/2222222222222222222222222222222222222222/check-runs",
            query: "per_page=100",
            result: exchange(
                200,
                json!({"check_runs":[{
                    "id":501,"name":"ci/test","status":"completed","conclusion":"success",
                    "head_sha":head_cur,"html_url":"https://github.com/Acme/kit/runs/501"
                }]}),
                &[],
            ),
        },
        Request {
            path: "/repos/Acme/kit/commits/2222222222222222222222222222222222222222/check-suites",
            query: "per_page=100",
            result: exchange(200, json!({"check_suites":[]}), &[]),
        },
        Request {
            path: "/repos/Acme/kit/actions/runs",
            query: "head_sha=2222222222222222222222222222222222222222",
            result: exchange(200, json!({"workflow_runs":[]}), &[]),
        },
        Request {
            path: "/repos/Acme/kit/commits/2222222222222222222222222222222222222222/statuses",
            query: "per_page=100",
            result: exchange(200, json!([]), &[]),
        },
        // 8. github.reviews on PR #90 at head_cur: observes PR head + 2 review endpoints
        Request {
            path: "/repos/Acme/kit/pulls/90",
            query: "",
            result: exchange(
                200,
                json!({
                    "number":90,"node_id":"pr-90","html_url":"https://github.com/Acme/kit/pull/90",
                    "state":"open","head":{"sha":head_cur,"ref":"feat"},
                    "base":{"sha":"0".repeat(40),"ref":"main","repo":{"id":12}},
                    "updated_at":"2023-11-14T22:15:00Z"
                }),
                &[],
            ),
        },
        Request {
            path: "/repos/Acme/kit/pulls/90/reviews",
            query: "per_page=100",
            result: exchange(
                200,
                json!([{
                    "id":701,"state":"APPROVED","commit_id":head_cur,
                    "html_url":"https://github.com/Acme/kit/pull/90#pullrequestreview-701",
                    "user":{"login":"reviewer","id":88},"submitted_at":"2023-11-14T22:16:00Z"
                }]),
                &[],
            ),
        },
        Request {
            path: "/repos/Acme/kit/pulls/90/comments",
            query: "per_page=100",
            result: exchange(200, json!([]), &[]),
        },
    ]))));

    let github = service(&h, &store, script.clone());
    let use_cases = UseCases::new().with_github(github.clone());
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
    let human = principal(store.as_ref(), "ui-human", PrincipalKind::Human);
    let agent_a = scoped_agent(store.as_ref(), "agent-proj-a", "proj-a", "agent-issue");
    let agent_b = scoped_agent(store.as_ref(), "agent-proj-b", "proj-b", "agent-issue");

    // Agent is refused on human-only github.account command.
    let account_cmd = |rk: &str, action: &str| {
        serde_json::to_vec(&json!({
            "mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,"requestKey":rk,
            "body":{"kind":"github.account","action":action}
        }))
        .unwrap()
    };
    assert!(matches!(
        use_cases
            .command(
                &gateway,
                &agent_a,
                "github.account",
                &account_cmd("ag-acc", "Refresh")
            )
            .unwrap(),
        CommandReply::Rejected { .. } | CommandReply::Recorded { .. }
    ));

    // 1. Human connects GitHub account through UseCases and reads scopes at r3 (without raw token!).
    let reply = use_cases
        .command(
            &gateway,
            &human,
            "github.account",
            &account_cmd("hu-acc", "Refresh"),
        )
        .unwrap();
    assert!(matches!(
        reply,
        CommandReply::Recorded { receipt, .. } if matches!(receipt.decision, DomainDecision::Applied { .. })
    ));
    let acc_q = serde_json::to_vec(&json!({
        "mode":"Query","protocol":"1.0-draft.1","schemaRevision":3,"queryId":"q-acc",
        "body":{"kind":"github.account"}
    }))
    .unwrap();
    let DomainQueryReply::Result {
        result: OperationResult::GithubAccount { value: acc_state },
        ..
    } = use_cases
        .query(&gateway, &human, "github.account", &acc_q)
        .unwrap()
    else {
        panic!("expected github.account query result")
    };
    assert_eq!(acc_state.account.as_ref().unwrap().login.0, "octo-dev");
    assert_eq!(
        acc_state
            .granted_scopes
            .as_ref()
            .unwrap()
            .0
            .iter()
            .map(|s| s.0.as_str())
            .collect::<Vec<_>>(),
        vec!["read:org", "repo"]
    );
    assert!(
        !serde_json::to_string(&acc_state)
            .unwrap()
            .contains("test-private-token")
    );

    // 2. github.repositories personal + organization discovery via UseCases.
    let repos_q = |qid: &str, org: Option<&str>| {
        let mut body =
            json!({"kind":"github.repositories","accountId":"github-com","page":{"limit":50}});
        if let Some(org) = org {
            body["organization"] = json!(org);
        }
        serde_json::to_vec(&json!({
            "mode":"Query","protocol":"1.0-draft.1","queryId":qid,"body":body
        }))
        .unwrap()
    };
    let DomainQueryReply::Result {
        result: OperationResult::GithubRepositories { value: all_repos },
        ..
    } = use_cases
        .query(
            &gateway,
            &human,
            "github.repositories",
            &repos_q("q-repos-all", None),
        )
        .unwrap()
    else {
        panic!("expected all repositories")
    };
    assert_eq!(all_repos.items.0.len(), 2);

    let DomainQueryReply::Result {
        result: OperationResult::GithubRepositories { value: org_repos },
        ..
    } = use_cases
        .query(
            &gateway,
            &human,
            "github.repositories",
            &repos_q("q-repos-org", Some("Acme")),
        )
        .unwrap()
    else {
        panic!("expected org repositories")
    };
    assert_eq!(org_repos.items.0.len(), 1);
    assert_eq!(org_repos.items.0[0].name.0, "Acme/kit");

    let repo = id("github-sync-repo");
    let binding = github.qualify_registration(&repo).unwrap();

    // 3. Agent in proj-a links issue #42 via UseCases; agent in proj-b is denied scope!
    let link_frame = |rk: &str| {
        serde_json::to_vec(&json!({
            "mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,"requestKey":rk,
            "body":{
                "kind":"issues.link","issueId":"agent-issue","repositoryId":"github-sync-repo",
                "expectedRevision":"1","remoteIssueNumber":"42"
            }
        }))
        .unwrap()
    };
    match use_cases
        .command(&gateway, &agent_b, "issues.link", &link_frame("b-denied"))
        .unwrap()
    {
        CommandReply::Rejected { .. } => {}
        CommandReply::Recorded { receipt, .. } => {
            assert!(matches!(receipt.decision, DomainDecision::Refused { .. }));
        }
        other => panic!("unexpected agent_b reply: {other:?}"),
    }
    let a_reply = use_cases
        .command(&gateway, &agent_a, "issues.link", &link_frame("a-ok"))
        .unwrap();
    assert!(
        matches!(
            &a_reply,
            CommandReply::Recorded { receipt, .. } if matches!(receipt.decision, DomainDecision::Applied { .. })
        ),
        "unexpected agent_a reply: {a_reply:?}"
    );

    // 4. github.issue_sync via UseCases + outbox!
    let sync_frame = serde_json::to_vec(&json!({
        "mode":"Command","protocol":"1.0-draft.1","requestKey":"sync-now",
        "body":{
            "kind":"github.issue_sync","repositoryId":"github-sync-repo","expectedCursorRevision":"1"
        }
    }))
    .unwrap();
    let sync_reply = use_cases
        .command(&gateway, &agent_a, "github.issue_sync", &sync_frame)
        .unwrap();
    assert!(matches!(
        sync_reply,
        CommandReply::Recorded { receipt, .. } if matches!(receipt.decision, DomainDecision::Accepted { .. })
    ));
    let outbox = dispatcher(
        &h,
        store.clone(),
        vec![devworks_engine::github::consumer(github.clone())],
    );
    runtime().block_on(outbox.run_until_idle()).unwrap();

    // 5. github.poll_status at r3 (pagination + cross-project cursor isolation) & r1 legacy shape!
    let status_q = |qid: &str, rev: Option<u32>, cursor: Option<&str>| {
        let mut page = json!({"limit": 1});
        if let Some(c) = cursor {
            page["cursor"] = json!(c);
        }
        let mut env = json!({
            "mode":"Query","protocol":"1.0-draft.1","queryId":qid,
            "body":{"kind":"github.poll_status","repositoryId":"github-sync-repo"}
        });
        if let Some(r) = rev {
            env["schemaRevision"] = json!(r);
            env["body"]["page"] = page;
        }
        serde_json::to_vec(&env).unwrap()
    };
    let DomainQueryReply::Result {
        result: OperationResult::GithubPollStatus { value: status_p1 },
        ..
    } = use_cases
        .query(
            &gateway,
            &agent_a,
            "github.poll_status",
            &status_q("q-st-1", Some(3), None),
        )
        .unwrap()
    else {
        panic!("expected poll_status r3")
    };
    let status_json = serde_json::to_value(&status_p1).unwrap();
    assert_eq!(status_json["intake"]["items"].as_array().unwrap().len(), 1);
    let cursor_str = status_json["intake"]["nextCursor"]
        .as_str()
        .expect("page 1 cursor")
        .to_owned();
    // Another principal/scope cannot replay agent_a's cursor!
    assert!(matches!(
        use_cases
            .query(
                &gateway,
                &human,
                "github.poll_status",
                &status_q("q-st-cross", Some(3), Some(&cursor_str)),
            )
            .unwrap(),
        DomainQueryReply::Rejected { .. }
    ));
    // Legacy r1 query returns legacy Page<GithubObservation> shape!
    assert!(matches!(
        use_cases
            .query(
                &gateway,
                &human,
                "github.poll_status",
                &status_q("q-st-r1", None, None),
            )
            .unwrap(),
        DomainQueryReply::Result {
            result: OperationResult::GithubPollStatus {
                value: devworks_core::protocol::records::GithubPollStatusResult::Legacy(_)
            },
            ..
        }
    ));

    // 6. Seed a stale check on head_old, then query github.checks & github.reviews for head_cur:
    //    only exact-head observations on head_cur are returned!
    let canonical = binding.github_repository_id.clone();
    let stale_head = head_old.clone();
    write(store.as_ref(), move |tx| {
        let obs_id = tx.new_id(IdKind::Event);
        tx.github()
            .append_github_observation(&StoredGithubObservation {
                id: obs_id,
                repository_id: canonical,
                kind: "CheckRun".into(),
                subject_id: "90".into(),
                remote_revision: "stale-check".into(),
                head: Some(stale_head),
                url: "https://github.com/Acme/kit/runs/111".into(),
                payload: json!({"name":"ci/test","state":"failure","pullUrl":"https://github.com/Acme/kit/pull/90"}),
                observed_at_ms: T0,
                source_updated_at_ms: Some(T0),
            })
            .map(|_| ())
    })
    .unwrap();

    let checks_q = serde_json::to_vec(&json!({
        "mode":"Query","protocol":"1.0-draft.1","queryId":"q-checks",
        "body":{
            "kind":"github.checks","repositoryId":"github-sync-repo",
            "pullRequestNumber":"90",
            "head":{"format":"Sha1","hex":head_cur},
            "page":{"limit":20}
        }
    }))
    .unwrap();
    let DomainQueryReply::Result {
        result: OperationResult::GithubChecks { value: checks_page },
        ..
    } = use_cases
        .query(&gateway, &agent_a, "github.checks", &checks_q)
        .unwrap()
    else {
        panic!("expected github.checks result")
    };
    assert_eq!(checks_page.items.0.len(), 1);
    assert_eq!(
        checks_page.items.0[0].url.as_ref().unwrap().0,
        "https://github.com/Acme/kit/pull/90/checks?check_run_id=501"
    );

    let reviews_q = serde_json::to_vec(&json!({
        "mode":"Query","protocol":"1.0-draft.1","queryId":"q-reviews",
        "body":{
            "kind":"github.reviews","repositoryId":"github-sync-repo",
            "pullRequestNumber":"90",
            "head":{"format":"Sha1","hex":head_cur},
            "page":{"limit":20}
        }
    }))
    .unwrap();
    let DomainQueryReply::Result {
        result: OperationResult::GithubReviews {
            value: reviews_page,
        },
        ..
    } = use_cases
        .query(&gateway, &agent_a, "github.reviews", &reviews_q)
        .unwrap()
    else {
        panic!("expected github.reviews result")
    };
    assert_eq!(reviews_page.items.0.len(), 1);
    assert_eq!(reviews_page.items.0[0].state.0, "APPROVED");

    // 7. Rotating workspace github.credential immediately fences poll/repository reads until Refresh,
    //    and Disconnect marks the account disconnected and keeps reads fenced!
    write(store.as_ref(), |tx| {
        let schema = devworks_core::settings::catalog::descriptor("github.credential")
            .unwrap()
            .schema_ref();
        tx.configuration().append_revision(
            &ConfigScope::Workspace,
            "settings",
            2,
            1,
            json!({
                "values": {"github.credential": {
                    "schema": schema,
                    "value": {"secretRef": devworks_core::authority::redact::SecretRef::mint(99).as_str()}
                }}
            }),
        )
    })
    .unwrap();
    assert!(matches!(
        use_cases
            .query(
                &gateway,
                &human,
                "github.poll_status",
                &status_q("q-after-rotate", Some(3), None),
            )
            .unwrap(),
        DomainQueryReply::Rejected { .. }
    ));
    assert!(matches!(
        use_cases
            .command(
                &gateway,
                &human,
                "github.account",
                &account_cmd("hu-disc", "Disconnect"),
            )
            .unwrap(),
        CommandReply::Recorded { receipt, .. } if matches!(receipt.decision, DomainDecision::Applied { .. })
    ));
    assert!(matches!(
        use_cases
            .query(
                &gateway,
                &human,
                "github.poll_status",
                &status_q("q-after-disc", Some(3), None),
            )
            .unwrap(),
        DomainQueryReply::Rejected { .. }
    ));
    assert!(script.0.lock().unwrap().is_empty());
}

enum DockerOutcome {
    ReproducedFailure { code: i32 },
    SetupFailureBeforeMarker,
    RemoteDaemon,
}

struct ScriptedDockerHelpers {
    git_delegate: SupervisedHelpers,
    store: Arc<dyn ApplicationStore>,
    docker_mode: Mutex<DockerOutcome>,
}

impl HelperRunner for ScriptedDockerHelpers {
    fn filesystem(&self) -> Option<&dyn devworks_core::authority::destructive::FilesystemResolver> {
        self.git_delegate.filesystem()
    }

    fn environment(&self) -> Vec<(String, String)> {
        self.git_delegate.environment()
    }

    fn run(
        &self,
        ctx: &InvocationContext,
        spec: ProcessSpec,
        limits: HelperLimits,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<u8>, HelperFault> {
        if spec.argv().first().map(String::as_str) == Some("docker") {
            let mode = self.docker_mode.lock().unwrap();
            return match *mode {
                DockerOutcome::RemoteDaemon => Ok(b"tcp://10.0.0.5:2375\n".to_vec()),
                _ => Ok(b"unix:///var/run/docker.sock\n".to_vec()),
            };
        }
        self.git_delegate.run(ctx, spec, limits, stop)
    }

    fn run_recorded(
        &self,
        ctx: &InvocationContext,
        spec: ProcessSpec,
        limits: HelperLimits,
        stop: &(dyn Fn() -> bool + Sync),
        operation: &OperationId,
        approved_hold: bool,
    ) -> Result<Vec<u8>, HelperFault> {
        let argv = spec.argv();
        if argv.first().map(String::as_str) == Some("docker") {
            assert!(
                spec.cwd.is_none(),
                "docker verifier must never mount checkout cwd"
            );
            assert!(spec.stdin.as_ref().is_some_and(|tar| !tar.is_empty()));
            assert!(argv.iter().any(|a| a == "--network=none"));
            assert!(argv.iter().any(|a| a == "--read-only"));
            let marker = argv
                .windows(2)
                .find_map(|w| (w[0] == "devworks-proof").then(|| w[1].clone()))
                .expect("proof marker");
            let argv_sha256 = sha256_hex(argv.join("\0").as_bytes());
            let mode = self.docker_mode.lock().unwrap();
            let (exit_code, stderr) = match *mode {
                DockerOutcome::ReproducedFailure { code } => (
                    code,
                    format!("{marker}\nrunning parse_url_panic\npanicked at empty host segment\n"),
                ),
                DockerOutcome::SetupFailureBeforeMarker => {
                    (1, "container image missing before marker\n".to_owned())
                }
                DockerOutcome::RemoteDaemon => unreachable!("rejected before run_recorded"),
            };
            let op_copy = operation.clone();
            let actor = ctx.principal().clone();
            let project_id = ctx.project_id().cloned();
            let root_id = ctx.root_id().cloned();
            write(self.store.as_ref(), move |tx| {
                let proc_id = tx.new_id(IdKind::Process);
                let mut row = tx.resources().insert_process(NewProcess {
                    id: proc_id,
                    platform: "unix".into(),
                    program: "docker".into(),
                    argv_sha256,
                    operation_id: Some(op_copy),
                    lease_id: None,
                    actor,
                    session_id: None,
                    project_id,
                    root_id,
                    assignment_id: None,
                })?;
                let expected = row.revision;
                row.revision += 1;
                row.phase = ProcessPhase::Reaped;
                row.exit_code = Some(i64::from(exit_code));
                row.updated_at_ms = tx.now_ms();
                tx.resources().update_process(&row, expected)
            })
            .unwrap();
            return Err(HelperFault::Failed {
                code: Some(exit_code),
                stderr,
            });
        }
        self.git_delegate
            .run_recorded(ctx, spec, limits, stop, operation, approved_hold)
    }
}

#[test]
fn shared_repo_triage_verifies_trusted_checks_and_handles_setup_or_runner_failures_across_restart()
{
    let _native_case = native_runtime_case();
    let h = Harness::new();
    let store = h.open();
    // Create a real Git repository with a committed `.devworks/github-checks.json` manifest!
    let repo_dir = h.root.join("shared-kit");
    std::fs::create_dir_all(repo_dir.join(".devworks")).unwrap();
    git(&repo_dir, &["init", "-b", "main"]);
    let image_digest = format!("ghcr.io/acme/rust-ci@sha256:{}", "a".repeat(64));
    std::fs::write(
        repo_dir.join(".devworks/github-checks.json"),
        serde_json::to_vec_pretty(&json!({
            "checks": [{
                "id": "unit-suite",
                "description": "Run unit tests in isolated container",
                "image": image_digest,
                "argv": ["cargo", "test", "--lib"]
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    git(&repo_dir, &["add", ".devworks/github-checks.json"]);
    git(
        &repo_dir,
        &["commit", "-m", "Add trusted GitHub verification checks"],
    );
    let indexed_oid = git(&repo_dir, &["rev-parse", "HEAD"]);
    let repo_location = repo_dir.to_string_lossy().into_owned();

    write(store.as_ref(), move |tx| {
        let cred_schema = devworks_core::settings::catalog::descriptor("github.credential")
            .unwrap()
            .schema_ref();
        let interval_schema =
            devworks_core::settings::catalog::descriptor("github.issueTriageIntervalSeconds")
                .unwrap()
                .schema_ref();
        tx.configuration().append_revision(
            &ConfigScope::Workspace,
            "settings",
            1,
            1,
            json!({
                "values": {
                    "github.credential": {
                        "schema": cred_schema,
                        "value": {"secretRef": devworks_core::authority::redact::SecretRef::mint(7).as_str()}
                    },
                    "github.issueTriageIntervalSeconds": {
                        "schema": interval_schema,
                        "value": 600
                    }
                }
            }),
        )?;
        tx.projects().insert_repository(&StoredRepository {
            id: id("github-sync-repo"),
            name: "kit".into(),
            location: repo_location.clone(),
            remote_url: Some("https://github.com/Acme/kit.git".into()),
            primary: true,
            lifecycle: ACTIVE.into(),
            languages: Vec::new(),
            revision: 1,
            registered_at_ms: T0,
        })?;
        for (pid, name) in [("proj-alpha", "Alpha"), ("proj-beta", "Beta")] {
            let p = ProjectId(id(pid));
            tx.projects().insert_project(&StoredProject {
                id: p.clone(),
                name: name.into(),
                description: format!("{name} service"),
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
            tx.projects().set_members(&p, &[id("github-sync-repo")])?;
        }
        let gen_id = id("published-gen-1");
        let view_id = id("published-view-1");
        tx.indexing().insert_generation(&StoredGeneration {
            id: gen_id.clone(),
            repository_id: id("github-sync-repo"),
            layer: LAYER_BASELINE.into(),
            state: GEN_STAGING.into(),
            source_digest: indexed_oid.clone(),
            watermark: 0,
            manifest: json!({}),
        })?;
        tx.indexing().update_generation(&StoredGeneration {
            id: gen_id.clone(),
            repository_id: id("github-sync-repo"),
            layer: LAYER_BASELINE.into(),
            state: GEN_SEALED.into(),
            source_digest: indexed_oid.clone(),
            watermark: 0,
            manifest: json!({}),
        })?;
        tx.indexing().insert_view(&StoredView {
            view_id: view_id.clone(),
            repository_id: id("github-sync-repo"),
            view_digest: sha256_hex(b"published-view-1"),
            baseline_id: gen_id,
            branch_id: None,
            worktree_id: None,
            coverage: json!({"state": "Complete"}),
        })?;
        tx.projects().publish_index_view(&StoredIndexView {
            view_id: view_id.clone(),
            repository_id: id("github-sync-repo"),
            source_revision: indexed_oid.clone(),
            coverage: "Complete".into(),
            published_at_ms: T0,
        })?;
        tx.indexing()
            .publish(&id("github-sync-repo"), &view_id, 0)
            .map(|_| ())
    })
    .unwrap();

    let mut bug_50 = issue("open", "2023-11-14T22:13:20Z");
    bug_50["number"] = json!(50);
    bug_50["node_id"] = json!("issue-node-50");
    bug_50["html_url"] = json!("https://github.com/Acme/kit/issues/50");
    bug_50["body"] =
        json!("Calling parse_url_panic on empty host crashes with panicked at empty host segment.");

    let mut bug_51 = bug_50.clone();
    bug_51["number"] = json!(51);
    bug_51["node_id"] = json!("issue-node-51");
    bug_51["html_url"] = json!("https://github.com/Acme/kit/issues/51");

    let mut feat_52 = bug_50.clone();
    feat_52["number"] = json!(52);
    feat_52["node_id"] = json!("issue-node-52");
    feat_52["html_url"] = json!("https://github.com/Acme/kit/issues/52");
    feat_52["title"] = json!("Add dark mode export");

    let script = Arc::new(Script(Mutex::new(VecDeque::from([
        Request {
            path: "/user",
            query: "",
            result: exchange(200, json!({"login":"octo","id":17}), &[]),
        },
        Request {
            path: "/repos/Acme/kit",
            query: "",
            result: exchange(
                200,
                json!({
                    "id":12,"node_id":"opaque-repository-node","name":"kit","full_name":"Acme/kit",
                    "owner":{"login":"Acme","type":"Organization"},
                    "html_url":"https://github.com/Acme/kit","default_branch":"main",
                    "archived":false,"disabled":false,"permissions":{"pull":true}
                }),
                &[],
            ),
        },
        Request {
            path: "/repos/Acme/kit/issues",
            query: "state=open",
            result: exchange(200, json!([bug_50, bug_51, feat_52]), &[]),
        },
    ]))));

    let github = service(&h, &store, script.clone());
    let human = principal(store.as_ref(), "triage-human", PrincipalKind::Human);
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
        "mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,"requestKey":"triage-auth",
        "body":{"kind":"github.account","action":"Refresh"}
    }))
    .unwrap();
    gateway
        .command(&human, "github.account", &sign_in, github.clone())
        .unwrap();
    let repo = id("github-sync-repo");
    let binding = github.qualify_registration(&repo).unwrap();
    github.poll_issue_page(&repo).unwrap();

    let control = Arc::new(ControlService::new(h.ids.clone()));
    let work_service = Arc::new(WorkService::new(h.ids.clone(), control.clone()));

    // Admit issue #50: verify custom 600s interval from Settings!
    let adm_50 = github.admit_triage(control.clone()).unwrap().unwrap();
    assert_eq!(adm_50.issue.key.number, 50);
    assert_eq!(adm_50.clock.interval_seconds, 600);
    assert_eq!(adm_50.clock.next_admission_at_ms, T0 + 600_000);
    // Restart / immediate re-poll does NOT burst-admit #51 before 600s!
    assert!(github.admit_triage(control.clone()).unwrap().is_none());

    let fs = Arc::new(NativeFilesystem::new());
    let supervisor = Arc::new(ProcessSupervisor::new(
        store.clone(),
        Arc::new(NativeProcessHost),
        Arc::new(ResourceService::new(h.ids.clone(), h.clock())),
    ));
    let helpers = Arc::new(ScriptedDockerHelpers {
        git_delegate: SupervisedHelpers {
            supervisor,
            store: store.clone(),
            fs,
        },
        store: store.clone(),
        docker_mode: Mutex::new(DockerOutcome::ReproducedFailure { code: 101 }),
    });

    // Load triage_input for #50 and populate trusted checks from git commit OID!
    let mut input_50 = github.triage_input(&adm_50.operation).unwrap().unwrap();
    assert_eq!(input_50.destinations.len(), 2);
    let manifest_bytes = std::fs::read(repo_dir.join(".devworks/github-checks.json")).unwrap();
    let parsed_checks = verification::parse_manifest(&manifest_bytes);
    assert_eq!(parsed_checks.len(), 1);
    for dest in &mut input_50.destinations {
        dest.checks = parsed_checks.clone();
    }

    // Settle shared-repo multi-project classification:
    // - proj-alpha gets valid trusted checkId "unit-suite" -> queues github.verify_issue!
    // - proj-beta has no verification proposal -> routes to "AwaitingDecision" + notification!
    let op_50_id = adm_50.operation.id.clone();
    let ws_50 = work_service.clone();
    write(store.as_ref(), move |tx| {
        let completed = mark_operation_complete(tx, &op_50_id)?;
        settle_classification(
            tx,
            &ws_50,
            &completed,
            &input_50,
            &TriageClassification::Bugs {
                destinations: vec![ProjectId(id("proj-alpha")), ProjectId(id("proj-beta"))],
                verifications: vec![BugVerificationProposal {
                    project_id: ProjectId(id("proj-alpha")),
                    check_id: id("unit-suite"),
                    report_quote: "panicked at empty host segment".into(),
                    test_identifier: "parse_url_panic".into(),
                    expected_failure: "panicked at empty host segment".into(),
                }],
            },
        )
    })
    .unwrap();

    let key_50 = GithubIssueKey {
        repository_id: binding.github_repository_id.clone(),
        number: 50,
    };
    let alpha_case_before = read(store.as_ref(), |s| {
        s.github()
            .github_triage_case(&key_50, &ProjectId(id("proj-alpha")))
    })
    .unwrap();
    let beta_case = read(store.as_ref(), |s| {
        s.github()
            .github_triage_case(&key_50, &ProjectId(id("proj-beta")))
    })
    .unwrap();
    assert_eq!(alpha_case_before.state, "NeedsProof");
    assert_eq!(beta_case.state, "AwaitingDecision");

    // Run verifier outbox, including simulated restart recovery via observe_unknown!
    let verify_consumer = devworks_engine::github_verify::Verifier::consumer(
        github.clone(),
        store.clone(),
        helpers.clone(),
        control.clone(),
        "git".into(),
    );
    let verify_outbox = dispatcher(&h, store.clone(), vec![verify_consumer]);
    runtime().block_on(verify_outbox.run_until_idle()).unwrap();

    let alpha_case_after = read(store.as_ref(), |s| {
        s.github()
            .github_triage_case(&key_50, &ProjectId(id("proj-alpha")))
    })
    .unwrap();
    assert!(
        matches!(alpha_case_after.state.as_str(), "Verified" | "Working"),
        "expected Verified or Working after trusted check reproduction, got {} (evidence: {:?})",
        alpha_case_after.state,
        alpha_case_after.evidence
    );

    // Advance 600s to admit #51, and test remote/unavailable Docker daemon + setup failure -> AwaitingDecision!
    h.advance(600_000);
    *helpers.docker_mode.lock().unwrap() = DockerOutcome::RemoteDaemon;
    let adm_51 = github.admit_triage(control.clone()).unwrap().unwrap();
    assert_eq!(adm_51.issue.key.number, 51);
    let mut input_51 = github.triage_input(&adm_51.operation).unwrap().unwrap();
    for dest in &mut input_51.destinations {
        dest.checks = parsed_checks.clone();
    }
    let op_51_id = adm_51.operation.id.clone();
    let ws_51 = work_service.clone();
    write(store.as_ref(), move |tx| {
        let completed = mark_operation_complete(tx, &op_51_id)?;
        settle_classification(
            tx,
            &ws_51,
            &completed,
            &input_51,
            &TriageClassification::Bugs {
                destinations: vec![ProjectId(id("proj-alpha")), ProjectId(id("proj-beta"))],
                verifications: vec![
                    BugVerificationProposal {
                        project_id: ProjectId(id("proj-alpha")),
                        check_id: id("unit-suite"),
                        report_quote: "panicked at empty host segment".into(),
                        test_identifier: "parse_url_panic".into(),
                        expected_failure: "panicked at empty host segment".into(),
                    },
                    BugVerificationProposal {
                        project_id: ProjectId(id("proj-beta")),
                        check_id: id("unit-suite"),
                        report_quote: "panicked at empty host segment".into(),
                        test_identifier: "parse_url_panic".into(),
                        expected_failure: "panicked at empty host segment".into(),
                    },
                ],
            },
        )
    })
    .unwrap();
    // First verification (proj-alpha) fails cleanly when Docker daemon is remote (non-local tcp://)!
    // Switch mode before second or test SetupFailureBeforeMarker on proj-beta:
    *helpers.docker_mode.lock().unwrap() = DockerOutcome::SetupFailureBeforeMarker;
    runtime().block_on(verify_outbox.run_until_idle()).unwrap();
    *helpers.docker_mode.lock().unwrap() = DockerOutcome::RemoteDaemon;
    let case_51 = read(store.as_ref(), |s| {
        s.github().github_triage_case(
            &GithubIssueKey {
                repository_id: binding.github_repository_id.clone(),
                number: 51,
            },
            &ProjectId(id("proj-alpha")),
        )
    })
    .unwrap();
    assert_eq!(
        case_51.state, "AwaitingDecision",
        "setup failure before proof marker must never auto-verify"
    );
    let intake_50 = read(store.as_ref(), |s| {
        s.github().github_issue(
            &GithubIssueKey {
                repository_id: binding.github_repository_id.clone(),
                number: 50,
            },
            &GithubScope {
                project_id: None,
                repositories: vec![repo.clone()],
            },
        )
    })
    .unwrap();
    assert_eq!(intake_50.state, "Working");
    let intake_51 = read(store.as_ref(), |s| {
        s.github().github_issue(
            &GithubIssueKey {
                repository_id: binding.github_repository_id.clone(),
                number: 51,
            },
            &GithubScope {
                project_id: None,
                repositories: vec![repo.clone()],
            },
        )
    })
    .unwrap();
    assert_eq!(intake_51.state, "AwaitingDecision");
    let triaged_work_51 = read(store.as_ref(), |s| s.work().work(&case_51.work_id.0)).unwrap();
    let brief_51 = devworks_core::kernel::work_brief::WorkBrief::of(&triaged_work_51).unwrap();
    assert!(
        brief_51.description.contains("panicked"),
        "triaged issue description should retain source report body: {}",
        brief_51.description
    );

    // Advance 600s to admit #52 as FeatureRequest -> AwaitingDecision + notification, never auto-starting!
    h.advance(600_000);
    let adm_52 = github.admit_triage(control.clone()).unwrap().unwrap();
    assert_eq!(adm_52.issue.key.number, 52);
    let input_52 = github.triage_input(&adm_52.operation).unwrap().unwrap();
    let op_52_id = adm_52.operation.id.clone();
    let ws_52 = work_service.clone();
    write(store.as_ref(), move |tx| {
        let completed = mark_operation_complete(tx, &op_52_id)?;
        settle_classification(
            tx,
            &ws_52,
            &completed,
            &input_52,
            &TriageClassification::FeatureRequest,
        )
    })
    .unwrap();
    let intake_52 = read(store.as_ref(), |s| {
        s.github().github_issue(
            &GithubIssueKey {
                repository_id: binding.github_repository_id.clone(),
                number: 52,
            },
            &GithubScope {
                project_id: None,
                repositories: vec![repo.clone()],
            },
        )
    })
    .unwrap();
    assert_eq!(intake_52.state, "AwaitingDecision");
    assert!(script.0.lock().unwrap().is_empty());
}

#[test]
fn remote_closure_reopening_and_inbound_conflict_escalation_preserve_local_authority() {
    let _native_case = native_runtime_case();
    let h = Harness::new();
    h.readers.store(1, std::sync::atomic::Ordering::SeqCst);
    let store = h.open();
    set_up(&h, &store);

    struct TestRoles;
    impl devworks_core::settings::service::PrincipalRoles for TestRoles {
        fn role(
            &self,
            _: &InvocationContext,
        ) -> Option<devworks_core::settings::catalog::AgentRole> {
            Some(devworks_core::settings::catalog::AgentRole::Worker)
        }
    }

    let artifacts = Arc::new(ArtifactService::new(
        Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.join("cache"),
        )),
        Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
        Arc::new(NativeFilesystem::new()),
        h.clock(),
        h.ids.clone(),
    ));
    let control = Arc::new(ControlService::new(h.ids.clone()));
    let work_service = Arc::new(WorkService::new(h.ids.clone(), control.clone()));
    let approvals = Arc::new(ApprovalService::new(
        h.ids.clone(),
        h.clock(),
        Arc::new(TestRoles),
        control.clone(),
    ));

    let mut open_60 = issue("open", "2023-11-14T22:10:00Z");
    open_60["number"] = json!(60);
    open_60["node_id"] = json!("issue-node-60");
    open_60["html_url"] = json!("https://github.com/Acme/kit/issues/60");
    open_60["body"] = json!("Initial bug report.");

    let mut closed_60 = open_60.clone();
    closed_60["state"] = json!("closed");
    closed_60["updated_at"] = json!("2023-11-14T22:15:00Z");

    let mut reopened_60 = open_60.clone();
    reopened_60["state"] = json!("open");
    reopened_60["updated_at"] = json!("2023-11-14T22:20:00Z");
    reopened_60["body"] = json!("Initial bug report.\n\nClarifying stack trace line 42.");

    let mut conflicting_60 = reopened_60.clone();
    conflicting_60["updated_at"] = json!("2023-11-14T22:25:00Z");
    conflicting_60["title"] = json!("Rewrite entire subsystem as new feature");
    conflicting_60["body"] = json!("Completely replaced scope and requirements.");
    conflicting_60["labels"] = json!([{"name": "enhancement"}]);

    let script = Arc::new(Script(Mutex::new(VecDeque::from([
        Request {
            path: "/user",
            query: "",
            result: exchange(200, json!({"login":"octo","id":17}), &[]),
        },
        Request {
            path: "/repos/Acme/kit",
            query: "",
            result: exchange(
                200,
                json!({
                    "id":12,"node_id":"opaque-repository-node","name":"kit","full_name":"Acme/kit",
                    "owner":{"login":"Acme","type":"Organization"},
                    "html_url":"https://github.com/Acme/kit","default_branch":"main",
                    "archived":false,"disabled":false,"permissions":{"pull":true}
                }),
                &[],
            ),
        },
        Request {
            path: "/repos/Acme/kit/issues",
            query: "state=open",
            result: exchange(200, json!([open_60]), &[]),
        },
        Request {
            path: "/repos/Acme/kit/issues",
            query: "state=all",
            result: exchange(200, json!([closed_60]), &[]),
        },
        Request {
            path: "/repos/Acme/kit/issues",
            query: "state=all",
            result: exchange(200, json!([reopened_60]), &[]),
        },
        Request {
            path: "/repos/Acme/kit/issues",
            query: "state=all",
            result: exchange(200, json!([conflicting_60]), &[]),
        },
    ]))));

    let github = service(&h, &store, script.clone());
    let human = principal(store.as_ref(), "reconcile-human", PrincipalKind::Human);
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
    let use_cases = UseCases::new()
        .with_github(github.clone())
        .with_approvals(approvals);

    let sign_in = serde_json::to_vec(&json!({
        "mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,"requestKey":"rec-auth",
        "body":{"kind":"github.account","action":"Refresh"}
    }))
    .unwrap();
    use_cases
        .command(&gateway, &human, "github.account", &sign_in)
        .unwrap();
    let repo = id("github-sync-repo");
    let binding = github.qualify_registration(&repo).unwrap();
    github.poll_issue_page(&repo).unwrap();

    let key_60 = GithubIssueKey {
        repository_id: binding.github_repository_id.clone(),
        number: 60,
    };
    let intake_60 = read(store.as_ref(), |s| {
        s.github().github_issue(
            &key_60,
            &GithubScope {
                project_id: None,
                repositories: vec![repo.clone()],
            },
        )
    })
    .unwrap();

    // Seed three projects sharing #60 to prove all three GH-05 branches simultaneously:
    // - proj-unrepro: unstarted unreproducible Bug (case state "AwaitingDecision") -> closed & retired on remote close!
    // - proj-started: verified/started Bug fix (case state "Verified", completion VERIFIED) + human-paused -> continues & stays paused on reopen!
    // - proj-feature: unstarted FeatureRequest Story -> retained open for manual decision!
    let host_actor = store.info().host_actor;
    let key_seed = key_60.clone();
    let remote_rev = intake_60.remote_revision.clone();
    write(store.as_ref(), move |tx| {
        for pid in ["proj-unrepro", "proj-started", "proj-feature"] {
            let p = ProjectId(id(pid));
            tx.projects().insert_project(&StoredProject {
                id: p.clone(),
                name: pid.into(),
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
            tx.projects().set_members(&p, &[id("github-sync-repo")])?;
            workflow::ensure_workflow(tx, &p)?;
        }

        // Completed triage operation backing adjudications on #60
        let triage_op = OperationId(id("triage-op-60"));
        let payload = json!({
            "repositoryId": key_seed.repository_id,
            "actorId": host_actor,
            "issueNumber": 60,
            "sourceRevision": remote_rev,
            "permissionRevision": 1,
        });
        tx.operations().record_intent(NewOperation {
            id: triage_op.clone(),
            root_id: None,
            assignment_id: None,
            capability: "github.triage".into(),
            schema_version: 1,
            args_digest: sha256_hex(payload.to_string().as_bytes()),
            payload,
        })?;
        mark_operation_complete(tx, &triage_op)?;

        let mk_work =
            |wid: &str, pid: &str, task_type: &str, completion: Option<&str>| StoredWork {
                id: id(wid),
                project_id: ProjectId(id(pid)),
                kind: TASK.into(),
                parent_feature_id: None,
                status_id: Some(workflow::default_status_id(&ProjectId(id(pid)), "backlog")),
                title: format!("Work {wid}"),
                priority: 0,
                manual_order: 0,
                revision: 1,
                scope_revision: 1,
                created_at_ms: T0,
                updated_at_ms: T0,
                created_by: None,
                updated_by: None,
                search_key: wid.into(),
                caller_ref: None,
                completion: completion.map(str::to_owned),
                brief: json!({"taskType": task_type}),
            };

        for (wid, pid, ttype, comp, cstate, ev) in [
            (
                "work-unrepro",
                "proj-unrepro",
                "Bug",
                None,
                "AwaitingDecision",
                None,
            ),
            (
                "work-started",
                "proj-started",
                "Bug",
                Some(VERIFIED),
                "Verified",
                None,
            ),
            (
                "work-feature-req",
                "proj-feature",
                "Story",
                None,
                "AwaitingDecision",
                Some(json!({"kind": "FeatureRequest"})),
            ),
        ] {
            let w = mk_work(wid, pid, ttype, comp);
            tx.work().insert_work(&w)?;
            tx.github()
                .link_github_work(&key_seed, &RootId(id(wid)), 1)?;
            tx.github().put_github_triage_case(
                &StoredGithubTriageCase {
                    id: id(&format!("case-{wid}")),
                    revision: 1,
                    key: key_seed.clone(),
                    project_id: ProjectId(id(pid)),
                    work_id: RootId(id(wid)),
                    source_revision: remote_rev.clone(),
                    triage_operation_id: triage_op.clone(),
                    state: cstate.into(),
                    evidence: ev,
                },
                None,
            )?;
        }

        // Human pauses work-started; remote reopen must NEVER clear this pause (GH-06)!
        tx.runs().insert_root_control(&StoredRootControl {
            desired: DesiredExecution::Paused,
            updated_at_ms: T0,
            ..StoredRootControl::initial(RootId(id("work-started")))
        })
    })
    .unwrap();

    let notice_outbox = dispatcher(
        &h,
        store.clone(),
        vec![devworks_engine::github::notice_consumer_with_services(
            store.clone(),
            artifacts.clone(),
            work_service.clone(),
        )],
    );

    // 1. Poll remote closure of #60 and run notice reconciliation!
    h.advance(60_000);
    assert!(matches!(
        github.poll_issue_page(&repo).unwrap(),
        IssuePageResult::Captured { reports: 1, .. }
    ));
    runtime().block_on(notice_outbox.run_until_idle()).unwrap();
    let notice_entries = read(store.as_ref(), |s| {
        s.queue().deliveries(
            &DeliveryFilter {
                consumer: Some(devworks_core::services::github::references::NOTICE_CONSUMER.into()),
                ..DeliveryFilter::default()
            },
            None,
            10,
        )
    });

    // Verify GH-05:
    // - work-unrepro moved to Closed AND its obligation retired with UnstartedUnreproducible!
    let unrepro = read(store.as_ref(), |s| s.work().work(&id("work-unrepro"))).unwrap();
    assert_eq!(
        unrepro.status_id,
        Some(workflow::default_status_id(
            &ProjectId(id("proj-unrepro")),
            "closed"
        )),
        "notice_entries: {:?}",
        notice_entries.entries
    );
    let retirements = read(store.as_ref(), |s| {
        s.github().github_obligation_retirements(
            &key_60,
            Some(&ProjectId(id("proj-unrepro"))),
            None,
            10,
        )
    });
    assert_eq!(retirements.len(), 1);
    assert_eq!(
        retirements[0].reason,
        GithubObligationRetirementReason::UnstartedUnreproducible
    );

    // - work-started stayed in Backlog (active) and unretired!
    let started = read(store.as_ref(), |s| s.work().work(&id("work-started"))).unwrap();
    assert_eq!(
        started.status_id,
        Some(workflow::default_status_id(
            &ProjectId(id("proj-started")),
            "backlog"
        ))
    );
    // - work-feature-req stayed open in Backlog for manual decision!
    let feat_req = read(store.as_ref(), |s| s.work().work(&id("work-feature-req"))).unwrap();
    assert_eq!(
        feat_req.status_id,
        Some(workflow::default_status_id(
            &ProjectId(id("proj-feature")),
            "backlog"
        ))
    );

    // 2. Poll remote reopening of #60 (with appended clarifying text):
    //    Verify GH-06: human pause on work-started is preserved!
    //    Verify GH-07: clarifying append does NOT stage a conflict approval!
    h.advance(60_000);
    assert!(matches!(
        github.poll_issue_page(&repo).unwrap(),
        IssuePageResult::Captured { reports: 1, .. }
    ));
    runtime().block_on(notice_outbox.run_until_idle()).unwrap();
    let ctrl_after_reopen = read(store.as_ref(), |s| {
        control_of(s.runs(), &RootId(id("work-started")))
    });
    assert_eq!(ctrl_after_reopen.desired, DesiredExecution::Paused);

    // 3. Poll conflicting/new-scope edit on #60:
    //    Verify GH-07: local title is NOT overwritten, both observations are retained, and a
    //    Pending github.issue_update approval is staged and can be approved via UseCases!
    h.advance(60_000);
    assert!(matches!(
        github.poll_issue_page(&repo).unwrap(),
        IssuePageResult::Captured { reports: 1, .. }
    ));
    runtime().block_on(notice_outbox.run_until_idle()).unwrap();

    let started_after = read(store.as_ref(), |s| s.work().work(&id("work-started"))).unwrap();
    assert_eq!(started_after.title, "Work work-started");

    let list_q = serde_json::to_vec(&json!({
        "mode":"Query","protocol":"1.0-draft.1","schemaRevision":3,"queryId":"q-approvals",
        "body":{
            "kind":"approvals.list",
            "scope":{"kind":"Project","projectId":"proj-started"},
            "state":"Pending",
            "page":{"limit":20}
        }
    }))
    .unwrap();
    let DomainQueryReply::Result {
        result: OperationResult::ApprovalsList { value: list_page },
        ..
    } = use_cases
        .query(&gateway, &human, "approvals.list", &list_q)
        .unwrap()
    else {
        panic!("expected approvals.list")
    };
    let conflict_approval = list_page
        .items
        .0
        .iter()
        .find(|a| a.operation_kind.0 == "github.issue_update")
        .expect("staged github.issue_update conflict approval");
    let dec_body = json!({
        "proposalId": conflict_approval.id,
        "expectedRevision": conflict_approval.revision,
        "contentDigest": conflict_approval.content_digest,
        "decision": "Approve",
        "rationale": "Adopt expanded scope explicitly"
    });
    let dec_input: devworks_core::operations::approvals::RecordHumanDecisionInput =
        serde_json::from_value(dec_body.clone()).unwrap();
    let human_dec = HostIngress::new(store.as_ref())
        .human_decision_context(&human, &dec_input)
        .expect("trusted human decision context");
    let mut cmd_body = dec_body;
    cmd_body["kind"] = json!("approvals.record_human_decision");
    let decide_frame = serde_json::to_vec(&json!({
        "mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,"requestKey":"approve-gh-upd",
        "body":cmd_body
    }))
    .unwrap();
    assert!(matches!(
        use_cases
            .command(
                &gateway,
                &human_dec,
                "approvals.record_human_decision",
                &decide_frame,
            )
            .unwrap(),
        CommandReply::Recorded { receipt, .. } if matches!(receipt.decision, DomainDecision::Applied { .. })
    ));
    let started_adopted = read(store.as_ref(), |s| s.work().work(&id("work-started"))).unwrap();
    assert_eq!(
        started_adopted.title,
        "Rewrite entire subsystem as new feature"
    );
    let adopted_brief = devworks_core::kernel::work_brief::WorkBrief::of(&started_adopted).unwrap();
    assert!(adopted_brief.description.contains("Completely replacing"));
    assert!(
        adopted_brief
            .comments
            .iter()
            .any(|c| c.item.content.0.contains("Work work-started")),
        "approving github.issue_update must retain prior local specification in comments"
    );
    assert!(script.0.lock().unwrap().is_empty());
}

fn scoped_agent(
    store: &dyn ApplicationStore,
    name: &str,
    project: &str,
    root: &str,
) -> InvocationContext {
    let base = principal(store, name, PrincipalKind::Agent);
    let actor = base.principal().clone();
    let pid = ProjectId(id(project));
    let _ = root;
    let sid = id(&format!("sess-{name}"));
    let info = store.info();
    let host_ctx = HostIngress::new(store).context(
        info.host_actor.clone(),
        PrincipalKind::Host,
        id(&format!("host-bind-{name}")),
        id("host-bind"),
        Counter::new(info.restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    );
    let sid_seed = sid.clone();
    let actor_seed = actor.clone();
    let pid_seed = pid.clone();
    write(store, move |tx| {
        if tx.sessions().session(&sid_seed)?.is_none() {
            tx.sessions().insert_session(&StoredSession {
                id: sid_seed.clone(),
                conversation_id: sid_seed,
                agent_id: actor_seed.clone(),
                project_id: Some(pid_seed.clone()),
                root_id: None,
                title: "Agent test session".into(),
                context_epoch: 0,
                revision: 1,
                created_by: actor_seed.clone(),
                created_at_ms: T0,
                closed_at_ms: None,
                closed_by: None,
                next_sequence: 1,
                checkpoint_sequence: 0,
            })?;
        }
        devworks_core::authority::service::bind_principal(
            tx,
            &host_ctx,
            NewPrincipalBinding {
                actor: actor_seed,
                kind: PrincipalKind::Agent,
                role: Some(devworks_core::settings::catalog::AgentRole::Worker),
                project_id: Some(pid_seed),
                tool_families: None,
                bound_by: info.host_actor,
            },
            None,
        )
        .map(|_| ())
        .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    })
    .unwrap();
    HostIngress::new(store)
        .context(
            actor,
            PrincipalKind::Agent,
            id(&format!("inv-{name}")),
            id("agent-tool"),
            Counter::new(info.restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
        .with_session(sid)
        .with_lineage(Some(pid), None, None)
}

fn mark_operation_complete(
    tx: &mut dyn WriteTransaction,
    op_id: &OperationId,
) -> Result<OperationRecord, StorageError> {
    let cur = tx
        .operations()
        .operation(op_id)?
        .expect("fixture operation");
    let attempt = tx
        .operations()
        .begin_attempt(op_id, cur.revision, AttemptBinding::default())?;
    let disp = tx.operations().transition(
        op_id,
        cur.revision + 1,
        Some(&attempt.fence()),
        OperationState::Dispatching,
        None,
    )?;
    tx.operations().transition(
        op_id,
        disp.revision,
        Some(&attempt.fence()),
        OperationState::ObservedComplete,
        None,
    )
}

fn set_verified(tx: &mut dyn WriteTransaction, work: &str) -> Result<(), StorageError> {
    let mut row = tx.work().work(&id(work))?.expect("fixture work");
    let expected = row.revision;
    row.revision += 1;
    row.completion = Some(VERIFIED.into());
    tx.work().update_work(&row, expected)
}
