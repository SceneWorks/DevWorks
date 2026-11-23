use super::{Harness, T0};
use crate::mcp_inbound::authenticate_http;
use crate::storage::sqlite::SqliteStore;
use devworks_core::admission::{Selection, decode};
use devworks_core::authority::{DenialReason, HostAuthority};
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::gateway::Gateway;
use devworks_core::ports::secrets::{Secret, SessionSecrets};
use devworks_core::ports::storage::{
    ApplicationStore, ArtifactRetention, ArtifactScope, ArtifactState, ReadOptions, StoredArtifact,
    StoredProject, StoredRepository, StoredWork, WriteOptions, read_snapshot, run_write,
};
use devworks_core::ports::{AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization};
use devworks_core::protocol::records::McpClientAccess;
use devworks_core::protocol::results::{DomainCommandReply, DomainDecision, OperationResult};
use devworks_core::protocol::{ActorId, CommandReply, Counter, Id, ProjectId, Revision};
use devworks_core::registry::Mode;
use devworks_core::services::UseCases;
use devworks_core::services::knowledge::KnowledgeService;
use devworks_core::services::mcp_inbound::{Binding, CredentialError, Credentials};
use devworks_core::services::projects::ProjectService;
use devworks_core::services::runs::ControlService;
use hyper::Request;
use serde_json::{Value, json};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::Ordering;

fn id(value: &str) -> Id {
    Id::new(value).unwrap()
}

struct Fixture {
    h: Harness,
    store: Arc<SqliteStore>,
    cases: UseCases,
    credentials: Credentials,
    human: InvocationContext,
}

impl Fixture {
    fn new() -> Self {
        let h = Harness::new();
        let store = Arc::new(h.store());
        let credentials = Credentials::new(
            store.clone(),
            Box::new(SessionSecrets::new()),
            Arc::new(crate::OsEntropy),
        );
        let human = HostIngress::new(&*store).context(
            ActorId(id("operator")),
            PrincipalKind::Human,
            id("native-settings"),
            id("settings-retries"),
            Counter::new(store.info().restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        );
        run_write(&*store, WriteOptions::fixture(), |tx| {
            tx.actors()
                .register(&ActorId(id("operator")), PrincipalKind::Human, "Operator")?;
            for project in ["first", "second"] {
                tx.projects().insert_project(&StoredProject {
                    id: ProjectId(id(project)),
                    name: project.into(),
                    description: String::new(),
                    lifecycle: "Active".into(),
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
            Ok(())
        })
        .unwrap();
        let projects = Arc::new(ProjectService::new(
            h.ids.clone(),
            Arc::new(ControlService::new(h.ids.clone())),
        ));
        let cases = UseCases::new()
            .with_inbound_mcp()
            .with_projects(projects)
            .with_knowledge(Arc::new(KnowledgeService::new(
                h.ids.clone(),
                h.clock.clone(),
                None,
            )));
        Self {
            h,
            store,
            cases,
            credentials,
            human,
        }
    }

    fn command(
        &self,
        actor: &InvocationContext,
        key: &str,
        operation: &str,
        revision: u32,
        mut body: Value,
    ) -> DomainCommandReply {
        body["kind"] = json!(operation);
        let frame = json!({
            "protocol": "1.0-draft.1", "mode": "Command",
            "schemaRevision": revision, "requestKey": key, "body": body,
        });
        let authority = HostAuthority {
            store: &*self.store,
            clock: &*self.h.clock,
            protected_branches: &[],
        };
        let gateway = Gateway {
            store: &*self.store,
            authority: &authority,
            ids: &*self.h.ids,
        };
        self.cases
            .command(&gateway, actor, operation, frame.to_string().as_bytes())
            .unwrap()
    }

    fn admin(&self, key: &str, operation: &str, body: Value) -> OperationResult {
        match self.command(&self.human, key, operation, 1, body) {
            CommandReply::Recorded { receipt, .. } => match receipt.decision {
                DomainDecision::Applied { result } => result,
                other => panic!("expected applied admin operation, got {other:?}"),
            },
            other => panic!("expected recorded admin operation, got {other:?}"),
        }
    }

    fn register(&self) -> McpClientAccess {
        let OperationResult::McpServerSetClientAccess { value } = self.admin(
            "register",
            "mcp_server.set_client_access",
            json!({"action": {"kind": "Register", "name": "Native test client"}}),
        ) else {
            panic!("register result")
        };
        value
    }

    fn replace(&self, key: &str, current: &McpClientAccess, commands: &[&str]) -> McpClientAccess {
        self.replace_scoped(key, current, &[], commands)
    }

    fn replace_scoped(
        &self,
        key: &str,
        current: &McpClientAccess,
        repositories: &[&str],
        commands: &[&str],
    ) -> McpClientAccess {
        let OperationResult::McpServerSetClientAccess { value } = self.admin(
            key, "mcp_server.set_client_access",
            json!({"action": {
                "kind": "Replace", "clientId": current.client_id, "expectedRevision": current.revision,
                "name": current.name, "enabled": true, "projectIds": ["first"],
                "repositoryIds": repositories, "worktreeIds": [], "readOperations": ["projects.get"],
                "commandOperations": commands
            }}),
        ) else {
            panic!("replace result")
        };
        value
    }

    fn configure(&self, port: u16) {
        self.admin(
            "configure",
            "mcp_server.configure",
            json!({"expectedRevision": "1", "enabled": true, "bindings": ["Stdio", "LoopbackHttp"], "httpPort": port}),
        );
    }

    fn claim(
        &self,
        ctx: &InvocationContext,
        key: &str,
        scope: &str,
        origin: Option<&str>,
    ) -> DomainCommandReply {
        let mut claim = json!({
            "subjectId": "subject", "kind": "Fact", "predicate": "notes",
            "value": "The native test writes a scoped entry.", "confidence": 0.9, "evidenceIds": [], "tags": [],
        });
        if let Some(origin) = origin {
            claim["origin"] = json!(origin);
            claim["originRef"] = json!("test-reference");
        }
        self.command(
            ctx,
            key,
            "knowledge.record",
            2,
            json!({
                "scope": {"kind": "Project", "projectId": scope},
                "batch": {
                    "entities": [{"clientId": "subject", "label": "MCP entry", "entityType": "topic", "aliases": []}],
                    "claims": [claim], "evidence": [],
                }
            }),
        )
    }
}

#[test]
fn selected_knowledge_command_keeps_authority_and_receipts_in_sqlite() {
    let fx = Fixture::new();
    fx.configure(8147);
    let registered = fx.register();
    assert!(!registered.enabled);
    assert!(registered.project_ids.0.is_empty());
    assert!(registered.command_operations.0.is_empty());
    assert!(matches!(
        fx.credentials
            .provision(&fx.human, &registered.client_id, &registered.revision),
        Err(CredentialError::Denied)
    ));

    let read_only = fx.replace("enable", &registered, &[]);
    let bearer = fx
        .credentials
        .provision(&fx.human, &read_only.client_id, &read_only.revision)
        .unwrap();
    let binding = Binding::LoopbackHttp { port: 8147 };
    let authorized = fx.credentials.authenticate(binding, &bearer).unwrap();
    let make_context = |verified: &devworks_core::services::mcp_inbound::AuthenticatedClient| {
        verified.invocation(
            id("host-request"),
            ProjectId(id("first")),
            Revision::new(1).unwrap(),
        )
    };
    let read_only_ctx = make_context(&authorized);
    assert!(matches!(
        fx.claim(&read_only_ctx, "read-only", "first", None),
        CommandReply::Rejected { .. }
    ));
    let editor = fx.replace("allow-command", &read_only, &["knowledge.record"]);
    assert!(matches!(
        fx.claim(&read_only_ctx, "stale-proof", "first", None),
        CommandReply::Rejected { .. }
    ));

    let ctx = make_context(&fx.credentials.authenticate(binding, &bearer).unwrap());
    assert!(matches!(
        fx.claim(&ctx, "foreign", "second", None),
        CommandReply::Rejected { .. }
    ));
    let origin = fx.claim(&ctx, "origin", "first", Some("User"));
    assert!(matches!(origin, CommandReply::Recorded { receipt, .. }
        if matches!(receipt.decision, DomainDecision::Refused { .. })));
    let first = fx.claim(&ctx, "recoverable", "first", None);
    let CommandReply::Recorded {
        receipt,
        replayed: false,
        ..
    } = first
    else {
        panic!("expected original decision")
    };
    let DomainDecision::Applied {
        result: OperationResult::KnowledgeRecord { value },
    } = receipt.decision
    else {
        panic!("expected native Knowledge write")
    };
    let claim_id = &value.claim_ids.0[0];
    let persisted_origin = read_snapshot(&*fx.store, &ReadOptions::default(), |s| {
        Ok(s.knowledge()
            .knowledge_claim(claim_id)?
            .unwrap()
            .body
            .origin)
    })
    .unwrap();
    assert_eq!(persisted_origin, "Agent");

    let OperationResult::McpServerSetClientAccess { value: rotated } = fx.admin(
        "rotate", "mcp_server.set_client_access",
        json!({"action": {"kind": "RotateCredential", "clientId": editor.client_id, "expectedRevision": editor.revision}}),
    ) else {
        panic!("rotation result")
    };
    assert!(matches!(
        fx.credentials.authenticate(binding, &bearer),
        Err(CredentialError::Denied)
    ));
    let new_bearer = fx
        .credentials
        .provision(&fx.human, &rotated.client_id, &rotated.revision)
        .unwrap();
    let current_ctx = make_context(&fx.credentials.authenticate(binding, &new_bearer).unwrap());
    let retry = fx.claim(&current_ctx, "recoverable", "first", None);
    assert!(
        matches!(retry, CommandReply::Recorded { replayed: true, receipt: replay, .. }
        if replay.command_id == receipt.command_id)
    );
    fx.credentials.close();
    assert!(matches!(
        fx.claim(&current_ctx, "closed", "first", None),
        CommandReply::Rejected { .. }
    ));
}

fn authorize(
    fx: &Fixture,
    context: &InvocationContext,
    operation: &str,
    mut body: Value,
) -> Result<Authorization, AuthorityDenial> {
    body["kind"] = json!(operation);
    let request = json!({
        "protocol": "1.0-draft.1", "mode": "Command", "requestKey": "scope-test", "body": body
    });
    let admitted = decode(
        Selection {
            operation,
            mode: Mode::Command,
        },
        request.to_string().as_bytes(),
    )
    .unwrap();
    HostAuthority {
        store: &*fx.store,
        clock: &*fx.h.clock,
        protected_branches: &[],
    }
    .authorize(
        context,
        &AuthorityRequest {
            descriptor: admitted.descriptor,
            binding: admitted.binding,
            effect: admitted.effect,
            action: admitted.action,
            input: &admitted.input,
            fingerprint: &admitted.fingerprint,
        },
    )
}

#[test]
fn issue_command_requires_the_explicit_repository_and_the_issues_project() {
    let fx = Fixture::new();
    run_write(&*fx.store, WriteOptions::fixture(), |tx| {
        let project = ProjectId(id("first"));
        for name in ["granted", "withheld"] {
            tx.projects().insert_repository(&StoredRepository {
                id: id(name),
                location: format!("https://example.invalid/{name}"),
                name: name.into(),
                remote_url: None,
                primary: false,
                lifecycle: "Active".into(),
                languages: Vec::new(),
                revision: 1,
                registered_at_ms: T0,
            })?;
        }
        tx.projects()
            .set_members(&project, &[id("granted"), id("withheld")])?;
        for (issue, project_id, kind) in [
            (
                "local-issue",
                project.clone(),
                devworks_core::ports::storage::TASK,
            ),
            (
                "other-issue",
                ProjectId(id("second")),
                devworks_core::ports::storage::TASK,
            ),
            (
                "parent-feature",
                project,
                devworks_core::ports::storage::FEATURE,
            ),
        ] {
            tx.work().insert_work(&StoredWork {
                id: id(issue),
                project_id,
                kind: kind.into(),
                parent_feature_id: None,
                status_id: None,
                title: issue.into(),
                priority: 0,
                manual_order: 0,
                revision: 1,
                scope_revision: 1,
                created_at_ms: T0,
                updated_at_ms: T0,
                created_by: None,
                updated_by: None,
                search_key: String::new(),
                caller_ref: None,
                completion: None,
                brief: json!({}),
            })?;
        }
        tx.work()
            .set_work_repositories(&id("parent-feature"), &[id("withheld")])?;
        Ok(())
    })
    .unwrap();
    fx.configure(8147);
    let active = fx.replace_scoped(
        "enable",
        &fx.register(),
        &["granted"],
        &["issues.link", "issues.publish", "tasks.create"],
    );
    let token = fx
        .credentials
        .provision(&fx.human, &active.client_id, &active.revision)
        .unwrap();
    let ctx = fx
        .credentials
        .authenticate(Binding::LoopbackHttp { port: 8147 }, &token)
        .unwrap()
        .invocation(
            id("issue-call"),
            ProjectId(id("first")),
            Revision::new(1).unwrap(),
        );
    for operation in ["issues.link", "issues.publish"] {
        let check = |repo, issue| {
            let mut input =
                json!({"issueId": issue, "repositoryId": repo, "expectedRevision": "1"});
            if operation == "issues.link" {
                input["remoteIssueNumber"] = json!("17");
            }
            authorize(&fx, &ctx, operation, input)
        };
        assert!(check("granted", "local-issue").is_ok(), "{operation}");
        assert!(
            matches!(
                check("withheld", "local-issue"),
                Err(AuthorityDenial::Policy(DenialReason::OutOfProjectScope))
            ),
            "{operation}"
        );
        assert!(
            matches!(
                check("granted", "other-issue"),
                Err(AuthorityDenial::Policy(DenialReason::OutOfProjectScope))
            ),
            "{operation}"
        );
    }
    let create = authorize(
        &fx,
        &ctx,
        "tasks.create",
        json!({
            "projectId":"first","title":"Child", "description":"", "repositoryIds":[],
            "parentId":"parent-feature","statusId":"ready","priority":0
        }),
    );
    assert!(matches!(
        create,
        Err(AuthorityDenial::Policy(DenialReason::OutOfProjectScope))
    ));
}

#[test]
fn document_command_rechecks_the_artifact_owner_before_deferred_dispatch() {
    let fx = Fixture::new();
    let first = ProjectId(id("first"));
    let second = ProjectId(id("second"));
    let document = id("document");
    let stored = document.clone();
    let owner = first.clone();
    run_write(&*fx.store, WriteOptions::fixture(), move |tx| {
        tx.artifacts().insert_artifact(&StoredArtifact {
            id: stored,
            origin_project_id: Some(owner.clone()),
            origin_work_id: None,
            title: "Document".into(),
            revision: 1,
            scope: ArtifactScope::Project(owner),
            owner: None,
            retention: ArtifactRetention::Explicit,
            scope_revision: 1,
            reference_revision: 1,
            current_version_id: None,
            state: ArtifactState::Live,
            created_at_ms: T0,
            provenance: json!({"managedDocument":true}),
        })
    })
    .unwrap();
    fx.configure(8147);
    let active = fx.replace("enable", &fx.register(), &["documents.index"]);
    let token = fx
        .credentials
        .provision(&fx.human, &active.client_id, &active.revision)
        .unwrap();
    let ctx = fx
        .credentials
        .authenticate(Binding::LoopbackHttp { port: 8147 }, &token)
        .unwrap()
        .invocation(id("document-call"), first, Revision::new(1).unwrap());
    let input = json!({"documentId":document,"versionId":"version","expectedRevision":"1"});
    let authority = authorize(&fx, &ctx, "documents.index", input.clone()).unwrap();
    run_write(&*fx.store, WriteOptions::fixture(), move |tx| {
        let mut artifact = tx.artifacts().artifact(&document)?.unwrap();
        artifact.scope = ArtifactScope::Project(second);
        artifact.scope_revision += 1;
        tx.artifacts().update_artifact(&artifact)
    })
    .unwrap();
    let holds = read_snapshot(&*fx.store, &ReadOptions::default(), |snapshot| {
        devworks_core::authority::authorization_holds(snapshot, &authority)
    })
    .unwrap();
    assert!(!holds);
    assert!(matches!(
        authorize(&fx, &ctx, "documents.index", input),
        Err(AuthorityDenial::Policy(DenialReason::OutOfProjectScope))
    ));
}

#[test]
fn committed_reply_remains_recoverable_when_the_app_closes_before_publication() {
    use devworks_core::context::ReceiptIdentity;
    use devworks_core::gateway::AdmittedCommand;
    use devworks_core::ports::storage::{LedgerEntry, WriteTransaction};
    use devworks_core::ports::{CommandHandler, Decided, HandlerFailure, Preparation, Prepared};
    use std::sync::Mutex;

    struct CloseAfterCommit {
        knowledge: KnowledgeService,
        credentials: Arc<Credentials>,
        identity: Arc<Mutex<Option<ReceiptIdentity>>>,
    }
    impl CommandHandler for CloseAfterCommit {
        fn decide(
            &self,
            ctx: &InvocationContext,
            command: &AdmittedCommand,
            tx: &mut dyn WriteTransaction,
        ) -> Result<Decided, HandlerFailure> {
            *self.identity.lock().unwrap() = Some(command.identity().clone());
            self.knowledge.decide(ctx, command, tx)
        }

        fn settle(
            &self,
            _: &InvocationContext,
            _: Option<&Prepared>,
            reply: Option<&DomainCommandReply>,
            _: &Preparation<'_>,
        ) {
            assert!(matches!(
                reply,
                Some(CommandReply::Recorded {
                    replayed: false,
                    ..
                })
            ));
            self.credentials.close();
        }
    }

    let mut fx = Fixture::new();
    fx.configure(8147);
    let active = fx.replace("enable", &fx.register(), &["knowledge.record"]);
    let credentials = Arc::new(Credentials::new(
        fx.store.clone(),
        Box::new(SessionSecrets::new()),
        Arc::new(crate::OsEntropy),
    ));
    let token = credentials
        .provision(&fx.human, &active.client_id, &active.revision)
        .unwrap();
    let ctx = credentials
        .authenticate(Binding::LocalIpc, &token)
        .unwrap()
        .invocation(
            id("publication-close"),
            ProjectId(id("first")),
            Revision::new(1).unwrap(),
        );
    let recorded_identity = Arc::new(Mutex::new(None));
    fx.cases.register(
        "knowledge.record",
        None,
        Some(Arc::new(CloseAfterCommit {
            knowledge: KnowledgeService::new(fx.h.ids.clone(), fx.h.clock.clone(), None),
            credentials,
            identity: recorded_identity.clone(),
        })),
    );

    assert!(matches!(
        fx.claim(&ctx, "closed-after-commit", "first", None),
        CommandReply::CommandStatusUnknown { .. }
    ));
    let identity = recorded_identity.lock().unwrap().take().unwrap();
    let durable = read_snapshot(&*fx.store, &ReadOptions::default(), |read| {
        read.receipts().lookup(&identity)
    })
    .unwrap();
    assert!(matches!(durable, Some(LedgerEntry::Current(recorded))
        if matches!(recorded.receipt.decision, DomainDecision::Applied { .. })));
}

#[test]
fn private_http_preflight_checks_listener_and_credential_on_every_request() {
    let connection = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let local = connection.local_addr().unwrap();
    let _client = TcpStream::connect(local).unwrap();
    let (server, peer) = connection.accept().unwrap();
    let local = server.local_addr().unwrap();
    let fx = Fixture::new();
    fx.configure(local.port());
    let registered = fx.register();
    let active = fx.replace("enable", &registered, &[]);
    let bearer = fx
        .credentials
        .provision(&fx.human, &active.client_id, &active.revision)
        .unwrap();
    let request = |host: &str, origin: Option<&str>, bearer: Option<&Secret>| {
        let mut builder = Request::post("/mcp")
            .header("Host", host)
            .header("Content-Length", "32");
        if let Some(origin) = origin {
            builder = builder.header("Origin", origin);
        }
        if let Some(bearer) = bearer {
            builder = builder.header("Authorization", format!("Bearer {}", bearer.expose()));
        }
        builder.body(()).unwrap().into_parts().0
    };
    let host = local.to_string();
    let valid = request(&host, None, Some(&bearer));
    assert!(authenticate_http(&fx.credentials, local, peer, &valid).is_ok());
    assert!(
        authenticate_http(
            &fx.credentials,
            local,
            peer,
            &request("example.test", None, Some(&bearer))
        )
        .is_err()
    );
    assert!(
        authenticate_http(
            &fx.credentials,
            local,
            peer,
            &request(&host, Some("http://localhost"), Some(&bearer))
        )
        .is_err()
    );
    assert!(authenticate_http(&fx.credentials, local, peer, &request(&host, None, None)).is_err());
    assert!(
        authenticate_http(
            &fx.credentials,
            local,
            peer,
            &request(&host, None, Some(&Secret::new("invalid")))
        )
        .is_err()
    );
    let other = Fixture::new();
    other.configure(local.port());
    assert!(authenticate_http(&other.credentials, local, peer, &valid).is_err());
    fx.h.clock
        .0
        .store(T0 + 91 * 24 * 60 * 60 * 1_000, Ordering::SeqCst);
    assert!(matches!(
        authenticate_http(&fx.credentials, local, peer, &valid),
        Err(CredentialError::Expired)
    ));
}

#[cfg(unix)]
#[test]
fn local_ipc_requires_private_kernel_peer_and_a_client_credential() {
    use crate::mcp_inbound::unix::PrivateListener;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use tokio::net::UnixStream;

    let fx = Fixture::new();
    fx.configure(8148);
    let registered = fx.register();
    let active = fx.replace("enable", &registered, &[]);
    let bearer = fx
        .credentials
        .provision(&fx.human, &active.client_id, &active.revision)
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    // Use a short private directory name because Unix sockaddr paths have a small fixed bound.
    let directory = root.path().join("m");
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    runtime.block_on(async {
        let unsafe_path = PrivateListener::bind(&directory);
        assert!(matches!(unsafe_path, Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied));
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let listener = PrivateListener::bind(&directory).unwrap();
        let path = listener.path().to_owned();
        assert_eq!(fs::symlink_metadata(&path).unwrap().mode() & 0o777, 0o600);
        let _peer = UnixStream::connect(&path).await.unwrap();
        let connection = listener.accept().await.unwrap();
        assert!(connection.authenticate(&fx.credentials, &Secret::new("invalid")).is_err());
        assert!(connection.authenticate(&fx.credentials, &bearer).is_ok());
        fx.credentials.close();
        assert!(matches!(
            connection.authenticate(&fx.credentials, &bearer),
            Err(CredentialError::Closed)
        ));
        drop(listener);
        assert!(!path.exists());
    });
}

#[test]
fn revocation_during_a_native_query_prevents_publication() {
    use devworks_core::gateway::AdmittedQuery;
    use devworks_core::ports::storage::ReadSnapshot;
    use devworks_core::ports::{QueryAnswer, QueryHandler};
    use devworks_core::protocol::results::DomainQueryReply;
    use devworks_core::protocol::{DomainErrorCode, DomainFault};
    use std::sync::{Mutex, mpsc};
    use std::time::Duration;

    struct PausedProjectRead {
        read: ProjectService,
        answer_ready: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl QueryHandler for PausedProjectRead {
        fn query(
            &self,
            ctx: &InvocationContext,
            query: &AdmittedQuery,
            snapshot: &dyn ReadSnapshot,
        ) -> Result<QueryAnswer, DomainFault> {
            let answer = self.read.query(ctx, query, snapshot)?;
            self.answer_ready.send(()).unwrap();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
            Ok(answer)
        }
    }

    let mut fx = Fixture::new();
    fx.configure(8149);
    let registered = fx.register();
    let active = fx.replace("enable", &registered, &[]);
    let token = fx
        .credentials
        .provision(&fx.human, &active.client_id, &active.revision)
        .unwrap();
    let ctx = fx
        .credentials
        .authenticate(Binding::LocalIpc, &token)
        .unwrap()
        .invocation(
            id("publication-race"),
            ProjectId(id("first")),
            Revision::new(1).unwrap(),
        );
    let (ready, arrived) = mpsc::channel();
    let (resume, release) = mpsc::channel();
    fx.cases.register(
        "projects.get",
        Some(Arc::new(PausedProjectRead {
            read: ProjectService::new(
                fx.h.ids.clone(),
                Arc::new(ControlService::new(fx.h.ids.clone())),
            ),
            answer_ready: ready,
            release: Mutex::new(release),
        })),
        None,
    );
    std::thread::scope(|scope| {
        let read = scope.spawn(|| {
            let authority = HostAuthority {
                store: &*fx.store, clock: &*fx.h.clock, protected_branches: &[],
            };
            let gateway = Gateway {
                store: &*fx.store, authority: &authority, ids: &*fx.h.ids,
            };
            fx.cases.query(
                &gateway, &ctx, "projects.get",
                br#"{"protocol":"1.0-draft.1","mode":"Query","queryId":"publication","body":{"kind":"projects.get","projectId":"first"}}"#,
            ).unwrap()
        });
        arrived
            .recv_timeout(Duration::from_secs(10))
            .expect("native answer materialized");
        fx.admin(
            "revocation", "mcp_server.set_client_access",
            json!({"action": {"kind":"Revoke", "clientId":active.client_id, "expectedRevision":active.revision}}),
        );
        resume.send(()).unwrap();
        assert!(matches!(
            read.join().unwrap(),
            DomainQueryReply::Rejected { fault, .. } if fault.code == DomainErrorCode::PermissionDenied
        ));
    });
}

#[cfg(windows)]
#[test]
fn incoming_named_pipe_rechecks_the_kernel_peer_for_its_message() {
    use crate::mcp_inbound::windows::PrivatePipe;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::ClientOptions;

    let fx = Fixture::new();
    fx.configure(8148);
    let registered = fx.register();
    let active = fx.replace("enable", &registered, &[]);
    let bearer = fx
        .credentials
        .provision(&fx.human, &active.client_id, &active.revision)
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut server = PrivatePipe::bind(&fx.store.info().store_id.0).unwrap();
        let mut client = ClientOptions::new().open(server.path()).unwrap();
        server.connect().await.unwrap();
        assert!(server.authenticate(&fx.credentials, &bearer).is_err());
        client.write_all(b"pair").await.unwrap();
        let mut hello = [0; 4];
        server.stream().read_exact(&mut hello).await.unwrap();
        assert_eq!(hello, *b"pair");
        assert!(
            server
                .authenticate(&fx.credentials, &Secret::new("invalid"))
                .is_err()
        );
        assert!(server.authenticate(&fx.credentials, &bearer).is_ok());
    });
}

#[test]
fn a_restored_store_epoch_requires_renewed_workspace_and_client_access() {
    use devworks_core::ports::secrets::{SecretError, SecretKey, SecretStore};

    struct Vault(Arc<SessionSecrets>);
    impl SecretStore for Vault {
        fn name(&self) -> &'static str {
            self.0.name()
        }
        fn put(&self, key: &SecretKey, value: &Secret) -> Result<(), SecretError> {
            self.0.put(key, value)
        }
        fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
            self.0.get(key)
        }
        fn delete(&self, key: &SecretKey) -> Result<bool, SecretError> {
            self.0.delete(key)
        }
        fn list(&self, namespace: &str) -> Result<Vec<Id>, SecretError> {
            self.0.list(namespace)
        }
    }

    let mut fx = Fixture::new();
    let protected = Arc::new(SessionSecrets::new());
    let credentials = |store: Arc<SqliteStore>| {
        Credentials::new(
            store,
            Box::new(Vault(protected.clone())),
            Arc::new(crate::OsEntropy),
        )
    };
    fx.credentials = credentials(fx.store.clone());
    fx.configure(8150);
    let registered = fx.register();
    let active = fx.replace("enable", &registered, &["knowledge.record"]);
    let old = fx
        .credentials
        .provision(&fx.human, &active.client_id, &active.revision)
        .unwrap();
    let before = fx.store.info();
    assert!(fx.credentials.authenticate(Binding::LocalIpc, &old).is_ok());

    // Exercise the app's real SQLite restore-epoch boundary. The workspace-restore command
    // owns restoring files and advancing this marker in the production recovery workflow.
    let Fixture {
        h,
        cases,
        credentials: prior_credentials,
        store,
        human: _,
    } = fx;
    drop(prior_credentials);
    drop(store);
    let database = h.root.path().join(super::DATABASE_FILE);
    let conn = rusqlite::Connection::open(database).unwrap();
    assert_eq!(
        conn.execute(
            "UPDATE store_meta SET restore_epoch = restore_epoch + 1",
            []
        )
        .unwrap(),
        1
    );
    drop(conn);
    let store = Arc::new(h.store());
    let after = store.info();
    assert_eq!(after.store_id, before.store_id);
    assert_eq!(after.restore_epoch, before.restore_epoch + 1);
    assert_eq!(protected.len(), 1);

    let human = HostIngress::new(&*store).context(
        ActorId(id("operator")),
        PrincipalKind::Human,
        id("restored-settings"),
        id("settings-retries"),
        Counter::new(after.restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    );
    let fx = Fixture {
        h,
        credentials: credentials(store.clone()),
        store,
        cases,
        human,
    };
    assert!(
        fx.credentials
            .authenticate(Binding::LocalIpc, &old)
            .is_err()
    );
    assert!(
        fx.credentials
            .provision(&fx.human, &active.client_id, &active.revision)
            .is_err()
    );
    fx.admin(
        "enable-restored-listener",
        "mcp_server.configure",
        json!({
            "expectedRevision":"2", "enabled":true, "bindings":["Stdio"],
        }),
    );
    assert!(
        fx.credentials
            .authenticate(Binding::LocalIpc, &old)
            .is_err()
    );
    let active = fx.replace("approve-restored-client", &active, &["knowledge.record"]);
    assert!(
        fx.credentials
            .authenticate(Binding::LocalIpc, &old)
            .is_err()
    );
    let current = fx
        .credentials
        .provision(&fx.human, &active.client_id, &active.revision)
        .unwrap();
    assert!(
        fx.credentials
            .authenticate(Binding::LocalIpc, &current)
            .is_ok()
    );
    assert!(
        fx.credentials
            .authenticate(Binding::LocalIpc, &old)
            .is_err()
    );
}
