//! Sessions, transcript batches and work lineage end to end on the real SQLite store: the
//! session lifecycle through the gateway with receipts, the session writer's atomic batches
//! (duplicate replay, stale/gap refusal, crash before and after commit), compaction epochs that
//! keep the original transcript, separate operation updates, bounded scoped history pages with
//! cursor expiry, lineage in both directions, corrections that preserve priors, reviewer/agent
//! scope, bounded briefings, the retained CodeGraph endpoint mapping and the recorded native
//! stream/transcript scenarios. Deterministic temporary stores and an injected clock; no
//! inference.
use super::{Harness, T0};
use crate::storage::sqlite::{MIGRATION_0007_SHA256, MIGRATIONS, SqliteStore};
use devworks_core::authority::HostAuthority;
use devworks_core::authority::service as auth;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::gateway::Gateway;
use devworks_core::ports::storage::contract::StoreHarness;
use devworks_core::ports::storage::*;
use devworks_core::protocol::provider::{
    ProviderEvent, TranscriptBatch, TranscriptBody, TranscriptPage,
};
use devworks_core::protocol::records::{
    AttributedGitLink, ContextSource, EvidenceLink, LineageRecord, ReceiptLookupState,
    SessionRecord, ToolInvocationOutcome, UsageItem,
};
use devworks_core::protocol::results::{
    DomainCommandReply, DomainDecision, DomainQueryReply, OperationResult,
};
use devworks_core::protocol::shared::{Event, EventRevisionKind, EventSubject};
use devworks_core::protocol::support::{HistoryPage, Revisioned};
use devworks_core::protocol::{
    ActorId, AttemptId, CommandReply, Counter, DomainErrorCode, DomainFault, Id, OperationId,
    ProjectId, Rejection, Revision, RootId,
};
use devworks_core::services::UseCases;
use devworks_core::services::history::{
    GitFacts, HistoryService, LinkKind, NewHistoryLink, RECEIPT_REPLY_UNAVAILABLE, record_link,
};
use devworks_core::services::sessions::transcript::{
    AppendPath, BatchOutcome, BatchReceipt, BatchRefusal, ContextSourceInput, SessionBatch,
    SessionWriter, SourceDisposition, append_session_batch, epoch_identity,
};
use devworks_core::services::sessions::turn::{
    TEXT_CHUNK, TurnAssembler, batch_of, interruption_marker,
};
use devworks_core::services::sessions::{
    BRIEFING_LIMIT, EPOCH_CHANGED, NATIVE_VIEWED, SESSION_CLOSED, SESSION_PRINCIPAL,
    SessionService, append_refusal,
};
use devworks_core::settings::catalog::AgentRole;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn actor(s: &str) -> ActorId {
    ActorId(id(s))
}
fn project(s: &str) -> ProjectId {
    ProjectId(id(s))
}
fn root(s: &str) -> RootId {
    RootId(id(s))
}
const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
fn art() -> Value {
    json!({"artifactId": "artifact-a", "versionId": "version-a", "sha256": DIGEST,
        "byteLength": "2", "mediaType": "application/json"})
}
fn art_b() -> Value {
    json!({"artifactId": "artifact-b", "versionId": "version-b", "sha256": DIGEST,
        "byteLength": "2", "mediaType": "application/json"})
}
fn binding() -> Value {
    json!({"connectionId": "connection-a", "accountRevision": "1",
        "endpointProfile": "OpenAiResponses", "modelId": "model-a", "contextEpoch": "0"})
}
fn call(native: &str, ordinal: u32) -> Value {
    json!({"call": {"attemptId": "attempt-a", "nativeCallId": native, "ordinal": ordinal.to_string()},
        "capabilityId": "core/read",
        "inputSchema": {"schemaId": "core/object", "revision": "1", "sha256": DIGEST},
        "arguments": art(), "nativeItem": art()})
}
fn key(native: &str, ordinal: u32) -> Value {
    json!({"attemptId": "attempt-a", "nativeCallId": native, "ordinal": ordinal.to_string()})
}
fn entry(entry_id: &str, seq: i64, body: Value) -> Value {
    json!({"entryId": entry_id, "sequence": seq.to_string(), "body": body})
}
fn tbatch(
    batch: &str,
    session: &str,
    epoch: i64,
    next: i64,
    entries: Vec<Value>,
) -> TranscriptBatch {
    serde_json::from_value(json!({"protocol": "1.0-draft.1", "batchId": batch,
        "sessionId": session, "contextEpoch": epoch.to_string(),
        "expectedNextSequence": next.to_string(), "entries": entries}))
    .unwrap()
}
fn text(t: &str) -> Value {
    json!({"kind": "UserMessage", "content": [{"kind": "Text", "text": t}]})
}
fn usage(request: &str) -> UsageItem {
    serde_json::from_value(json!({"requestId": request, "purpose": "Conversation",
        "modelId": "model-a", "inputTokens": "10", "outputTokens": "5", "cacheReadTokens": "4",
        "cacheWriteTokens": "0", "amountUsd": "0.01", "priceKind": "Reported",
        "priceOrigin": "ProviderReported", "incomplete": false}))
    .unwrap()
}

const ACTORS: [(&str, PrincipalKind); 9] = [
    ("coordinator-a", PrincipalKind::Agent),
    ("user-a", PrincipalKind::Human),
    ("user-b", PrincipalKind::Human),
    ("agent-a", PrincipalKind::Agent),
    ("agent-p", PrincipalKind::Agent),
    ("reviewer-a", PrincipalKind::Agent),
    ("agent-t", PrincipalKind::Agent),
    ("agent-b", PrincipalKind::Agent),
    ("package-a", PrincipalKind::Package),
];

struct Fx {
    h: Harness,
    store: Arc<SqliteStore>,
    cases: UseCases,
    n: Arc<AtomicU64>,
}

impl Fx {
    fn new() -> Self {
        let fx = Self::open(Harness::new(), Arc::new(AtomicU64::new(0)));
        fx.store
            .with_writer_connection(move |c| {
                super::super::conn::db(c.execute_batch(&format!(
                    "INSERT INTO projects VALUES('project-a','A','Active',1,1,1);
                     INSERT INTO projects VALUES('project-b','B','Active',1,1,1);
                     INSERT INTO repositories VALUES('repo-a','/repos/a',1);
                     INSERT INTO repositories VALUES('repo-b','/repos/b',1);
                     INSERT INTO project_repositories VALUES('project-a','repo-a');
                     INSERT INTO project_repositories VALUES('project-b','repo-b');
                     INSERT INTO work_items VALUES('root-a','project-a','feature',NULL,NULL,NULL,'Feature A',1,0,3,1,1,'{{}}');
                     INSERT INTO work_items VALUES('task-a','project-a','task','root-a',NULL,NULL,'Task A',1,0,1,1,1,'{{}}');
                     INSERT INTO work_items VALUES('root-b','project-b','feature',NULL,NULL,NULL,'Feature B',1,0,1,1,1,'{{}}');
                     INSERT INTO blobs(digest,byte_count,relative_path,state) VALUES('{DIGEST}',2,'artifacts/aa/{DIGEST}','Present');
                     INSERT INTO artifacts(id,origin_project_id,origin_work_id,title,revision,current_project_id,scope_revision,current_version_id,state,created_at) VALUES('artifact-a','project-a','root-a','Context',1,'project-a',1,NULL,'Live',1);
                     INSERT INTO artifact_versions VALUES('version-a','artifact-a',1,'{DIGEST}','application/json',1,NULL,'{{}}');
                     UPDATE artifacts SET current_version_id='version-a' WHERE id='artifact-a';
                     INSERT INTO artifacts(id,origin_project_id,origin_work_id,title,revision,current_project_id,scope_revision,current_version_id,state,created_at) VALUES('artifact-b','project-b',NULL,'Other',1,'project-b',1,NULL,'Live',1);
                     INSERT INTO artifact_versions VALUES('version-b','artifact-b',1,'{DIGEST}','application/json',1,NULL,'{{}}');"
                )))
            })
            .unwrap();
        fx.write(|tx| {
            for (a, k) in ACTORS {
                tx.actors().register(&actor(a), k, a)?;
            }
            Ok(())
        });
        let user = fx.user();
        for (who, kind, role, project, tools) in [
            (
                "agent-a",
                PrincipalKind::Agent,
                Some(AgentRole::Worker),
                None,
                None,
            ),
            (
                "agent-p",
                PrincipalKind::Agent,
                Some(AgentRole::Worker),
                Some("project-a"),
                None,
            ),
            (
                "reviewer-a",
                PrincipalKind::Agent,
                Some(AgentRole::CodeReviewer),
                None,
                None,
            ),
            (
                "agent-t",
                PrincipalKind::Agent,
                Some(AgentRole::Worker),
                None,
                Some(vec!["settings".to_owned()]),
            ),
            (
                "agent-b",
                PrincipalKind::Agent,
                Some(AgentRole::Worker),
                Some("project-b"),
                None,
            ),
            ("package-a", PrincipalKind::Package, None, None, None),
            (
                "coordinator-a",
                PrincipalKind::Agent,
                Some(AgentRole::Worker),
                None,
                None,
            ),
        ] {
            let by = user.clone();
            let binding = NewPrincipalBinding {
                actor: actor(who),
                kind,
                role,
                project_id: project.map(self::project),
                tool_families: tools,
                bound_by: actor("user-a"),
            };
            fx.write(move |tx| {
                auth::bind_principal(tx, &by, binding, None)
                    .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
            });
        }
        for (op, on) in [("op-b", "root-a"), ("op-c", "root-a"), ("op-x", "root-b")] {
            fx.write(move |tx| {
                tx.operations().record_intent(NewOperation {
                    id: OperationId(id(op)),
                    root_id: Some(root(on)),
                    assignment_id: None,
                    capability: "core.read".into(),
                    schema_version: 1,
                    args_digest: "d".into(),
                    payload: json!({}),
                })
            });
        }
        fx
    }

    fn open(h: Harness, n: Arc<AtomicU64>) -> Self {
        let store = Arc::new(h.store());
        let cases = UseCases::new()
            .with_sessions(Arc::new(
                SessionService::new(h.ids.clone(), store.clone())
                    .with_coordinator(actor("coordinator-a")),
            ))
            .with_history(Arc::new(HistoryService::new(h.ids.clone())));
        Self { h, store, cases, n }
    }
    /// A process restart: every handle is dropped, then the store is reopened.
    fn restart(self) -> Self {
        let Fx { h, store, cases, n } = self;
        drop((cases, store));
        Self::open(h, n)
    }

    fn ctx(&self, who: &str, kind: PrincipalKind, project: Option<&str>) -> InvocationContext {
        let info = self.store.info();
        HostIngress::new(&*self.store)
            .context(
                actor(who),
                kind,
                id("invocation-1"),
                id("ui-main"),
                Counter::new(info.restore_epoch).unwrap(),
                Revision::new(1).unwrap(),
            )
            .with_lineage(project.map(self::project), None, None)
    }
    fn user(&self) -> InvocationContext {
        self.ctx("user-a", PrincipalKind::Human, None)
    }
    /// An agent working on `root-a` in `project-a`, bound to `session`.
    fn worker(&self, who: &str, session: &str) -> InvocationContext {
        self.ctx(who, PrincipalKind::Agent, Some("project-a"))
            .with_lineage(Some(project("project-a")), Some(root("root-a")), None)
            .with_session(id(session))
    }
    fn write<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
    ) -> T {
        run_write(&*self.store, WriteOptions::default(), f).expect("write")
    }
    fn read<T>(&self, f: impl FnOnce(&dyn ReadSnapshot) -> Result<T, StorageError>) -> T {
        read_snapshot(&*self.store, &ReadOptions::default(), f).expect("read")
    }
    fn key(&self) -> String {
        format!("key-{}", self.n.fetch_add(1, Ordering::SeqCst))
    }
    fn authority(&self) -> HostAuthority<'_> {
        HostAuthority {
            store: &*self.store,
            clock: &*self.h.clock,
            protected_branches: &[],
        }
    }
    fn raw_command(&self, ctx: &InvocationContext, op: &str, frame: Value) -> DomainCommandReply {
        let authority = self.authority();
        let gateway = Gateway {
            authority: &authority,
            store: &*self.store,
            ids: &*self.h.ids,
        };
        self.cases
            .command(&gateway, ctx, op, frame.to_string().as_bytes())
            .expect("addressable")
    }
    fn command_keyed(
        &self,
        ctx: &InvocationContext,
        op: &str,
        key: &str,
        mut body: Value,
    ) -> DomainCommandReply {
        body["kind"] = json!(op);
        self.raw_command(
            ctx,
            op,
            json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": key, "body": body}),
        )
    }
    fn command(&self, ctx: &InvocationContext, op: &str, body: Value) -> DomainCommandReply {
        let key = self.key();
        self.command_keyed(ctx, op, &key, body)
    }
    fn raw_query(&self, ctx: &InvocationContext, op: &str, frame: Value) -> DomainQueryReply {
        let authority = self.authority();
        let gateway = Gateway {
            authority: &authority,
            store: &*self.store,
            ids: &*self.h.ids,
        };
        self.cases
            .query(&gateway, ctx, op, frame.to_string().as_bytes())
            .expect("addressable")
    }
    fn query(&self, ctx: &InvocationContext, op: &str, mut body: Value) -> DomainQueryReply {
        body["kind"] = json!(op);
        self.raw_query(
            ctx,
            op,
            json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": "q-1", "body": body}),
        )
    }
    fn start(&self, ctx: &InvocationContext, scope: Value, root: Option<&str>) -> SessionRecord {
        let mut body = json!({"scope": scope, "title": "Work chat"});
        if let Some(r) = root {
            body["rootId"] = json!(r);
        }
        match applied(self.command(ctx, "sessions.start", body)) {
            OperationResult::SessionsStart { value } => value,
            other => panic!("{other:?}"),
        }
    }
    fn work_session(&self) -> SessionRecord {
        self.start(
            &self.user(),
            json!({"kind": "Project", "projectId": "project-a"}),
            Some("root-a"),
        )
    }
    /// A session `session-a` conducted by `agent-a` on `root-a` (recorded scenarios name it).
    fn seed_session(&self, session: &str, agent: &str, root_id: Option<&str>) {
        let (session, agent, root_id) = (id(session), actor(agent), root_id.map(self::root));
        self.write(move |tx| {
            tx.sessions().insert_session(&StoredSession {
                conversation_id: id(&format!("conversation-{}", session.as_str())),
                id: session,
                agent_id: agent.clone(),
                project_id: Some(project("project-a")),
                root_id,
                title: "Seeded".into(),
                context_epoch: 0,
                revision: 1,
                created_by: agent,
                created_at_ms: T0,
                closed_at_ms: None,
                closed_by: None,
                next_sequence: 1,
                checkpoint_sequence: 0,
            })
        });
    }
    fn append(&self, ctx: &InvocationContext, batch: SessionBatch) -> BatchOutcome {
        SessionWriter {
            store: &*self.store,
        }
        .append(ctx, batch, WriteOptions::default())
        .expect("append")
    }
    fn session(&self, s: &str) -> StoredSession {
        let s = id(s);
        self.read(|r| r.sessions().session(&s)).expect("session")
    }
    fn transcript(
        &self,
        ctx: &InvocationContext,
        session: &str,
        after: Option<&str>,
    ) -> TranscriptPage {
        let mut frame = json!({"protocol": "1.0-draft.1", "queryId": "q-t", "sessionId": session,
            "limit": 50});
        if let Some(a) = after {
            frame["after"] = json!(a);
        }
        match result(self.raw_query(ctx, "sessions.transcript", frame)) {
            OperationResult::SessionsTranscript { value } => value,
            other => panic!("{other:?}"),
        }
    }
    fn events(
        &self,
        ctx: &InvocationContext,
        root_id: &str,
        after: Option<&str>,
    ) -> DomainQueryReply {
        let mut q = json!({"kind": "history.events", "rootId": root_id, "limit": 50});
        if let Some(a) = after {
            q["after"] = json!(a);
        }
        self.raw_query(
            ctx,
            "history.events",
            json!({"protocol": "1.0-draft.1", "queryId": "q-e", "query": q}),
        )
    }
    fn lineage(
        &self,
        ctx: &InvocationContext,
        root_id: &str,
        limit: u32,
        cursor: Option<&str>,
    ) -> DomainQueryReply {
        let mut page = json!({"limit": limit});
        if let Some(c) = cursor {
            page["cursor"] = json!(c);
        }
        self.query(
            ctx,
            "history.work_lineage",
            json!({"rootId": root_id, "page": page}),
        )
    }
}

fn recorded(reply: DomainCommandReply) -> (DomainDecision, bool) {
    match reply {
        CommandReply::Recorded {
            receipt, replayed, ..
        } => (receipt.decision, replayed),
        other => panic!("expected a recorded decision, got {other:?}"),
    }
}
fn applied(reply: DomainCommandReply) -> OperationResult {
    match recorded(reply).0 {
        DomainDecision::Applied { result } | DomainDecision::NoChange { result } => result,
        other => panic!("expected an applied decision, got {other:?}"),
    }
}
fn refused(reply: DomainCommandReply) -> DomainFault {
    match recorded(reply).0 {
        DomainDecision::Refused { fault } => fault,
        other => panic!("expected a refusal, got {other:?}"),
    }
}
fn rejected(reply: DomainCommandReply) -> Rejection {
    match reply {
        CommandReply::Rejected { code, .. } => code,
        other => panic!("expected a rejection, got {other:?}"),
    }
}
fn result(reply: DomainQueryReply) -> OperationResult {
    match reply {
        DomainQueryReply::Result { result, .. } => result,
        other => panic!("expected a result, got {other:?}"),
    }
}
fn fault(reply: DomainQueryReply) -> DomainFault {
    match reply {
        DomainQueryReply::Rejected { fault, .. } => fault,
        other => panic!("expected a fault, got {other:?}"),
    }
}
fn kind_of(f: &DomainFault) -> &str {
    f.target_kind.as_ref().map_or("", |k| k.0.as_str())
}
fn accepted(o: BatchOutcome) -> BatchReceipt {
    match o {
        BatchOutcome::Accepted(r) => r,
        other => panic!("expected an accepted batch, got {other:?}"),
    }
}
fn refusal(o: BatchOutcome) -> BatchRefusal {
    match o {
        BatchOutcome::Refused(r) => r,
        other => panic!("expected a refused batch, got {other:?}"),
    }
}
fn lineage_page(reply: DomainQueryReply) -> HistoryPage<LineageRecord> {
    match result(reply) {
        OperationResult::HistoryWorkLineage { value } => value,
        other => panic!("{other:?}"),
    }
}
fn kinds(t: &TranscriptPage) -> Vec<String> {
    t.entries
        .iter()
        .map(|e| {
            serde_json::to_value(&e.body).unwrap()["kind"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}
fn calls_and_response(session: &str) -> TranscriptBatch {
    tbatch(
        "batch-calls",
        session,
        0,
        1,
        vec![
            entry(
                "c-a",
                1,
                json!({"kind": "ToolCallRecorded", "toolCall": call("call:a", 0)}),
            ),
            entry(
                "c-b",
                2,
                json!({"kind": "ToolCallRecorded", "toolCall": call("call:b", 1)}),
            ),
            entry(
                "r-b",
                3,
                json!({"kind": "ToolResponseRecorded", "call": key("call:b", 1),
                    "outcome": {"kind": "ApprovalRequired", "approvalId": "approval-a", "operationId": "op-b"}}),
            ),
        ],
    )
}

// ---------------------------------------------------------------------------------------------
// Lifecycle

#[test]
fn sessions_start_and_end_through_the_gateway_with_receipts() {
    let fx = Fx::new();
    let user = fx.user();
    let start = json!({"scope": {"kind": "Project", "projectId": "project-a"}, "rootId": "root-a",
        "title": "Plan the feature"});
    let first = fx.command_keyed(&user, "sessions.start", "start-1", start.clone());
    let OperationResult::SessionsStart { value: started } = applied(first) else {
        panic!()
    };
    assert_eq!((started.revision.get(), started.closed), (1, false));
    assert_eq!(started.root_id, Some(root("root-a")));
    // A retried start (lost reply, restart) replays its receipt: no duplicate session.
    let fx = fx.restart();
    let (decision, replayed) =
        recorded(fx.command_keyed(&user, "sessions.start", "start-1", start));
    assert!(replayed);
    let DomainDecision::Applied {
        result: OperationResult::SessionsStart { value: again },
    } = decision
    else {
        panic!()
    };
    assert_eq!(again.id, started.id);
    let listed = match result(fx.query(
        &user,
        "sessions.list",
        json!({"scope": {"kind": "Project", "projectId": "project-a"}, "page": {"limit": 50}}),
    )) {
        OperationResult::SessionsList { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(listed.items.0.len(), 1);

    // End needs the current revision; ending twice is NoChange; appends after end are refused.
    let sid = started.id.as_str().to_owned();
    let stale = refused(fx.command(
        &user,
        "sessions.end",
        json!({"sessionId": sid, "expectedRevision": "9"}),
    ));
    assert_eq!(stale.code, DomainErrorCode::RevisionConflict);
    let OperationResult::SessionsEnd { value: ended } = applied(fx.command(
        &user,
        "sessions.end",
        json!({"sessionId": sid, "expectedRevision": "1"}),
    )) else {
        panic!()
    };
    assert_eq!((ended.revision.get(), ended.closed), (2, true));
    let (decision, _) = recorded(fx.command(
        &user,
        "sessions.end",
        json!({"sessionId": sid, "expectedRevision": "2"}),
    ));
    assert!(matches!(decision, DomainDecision::NoChange { .. }));
    let closed = refused(fx.raw_command(
        &user,
        "sessions.append_message",
        json!({"protocol": "1.0-draft.1", "requestKey": "msg-closed", "sessionId": sid,
            "expectedSessionRevision": "2", "content": [{"kind": "Text", "text": "late"}]}),
    ));
    assert_eq!(
        (closed.code, kind_of(&closed)),
        (DomainErrorCode::Conflict, SESSION_CLOSED)
    );
    let closed_only = match result(fx.query(
        &user,
        "sessions.list",
        json!({"scope": {"kind": "Project", "projectId": "project-a"}, "page": {"limit": 5},
            "closed": false}),
    )) {
        OperationResult::SessionsList { value } => value,
        other => panic!("{other:?}"),
    };
    assert!(closed_only.items.0.is_empty());
    // The session's start and end are lineage of its work.
    let page = lineage_page(fx.lineage(&user, "root-a", 50, None));
    let kinds: Vec<&str> = page.items.0.iter().map(|l| l.kind.0.as_str()).collect();
    assert!(
        kinds.contains(&"session") && kinds.contains(&"session_end"),
        "{kinds:?}"
    );
}

#[test]
fn append_message_saves_one_user_message_with_resolved_attachments() {
    let fx = Fx::new();
    let user = fx.user();
    let s = fx.work_session();
    let sid = s.id.as_str().to_owned();
    let frame = |key: &str, revision: &str, content: Value| {
        json!({"protocol": "1.0-draft.1", "requestKey": key, "sessionId": sid,
            "expectedSessionRevision": revision, "content": content})
    };
    let OperationResult::SessionsAppendMessage { value } = applied(fx.raw_command(
        &user,
        "sessions.append_message",
        frame(
            "m-1",
            "1",
            json!([{"kind": "Text", "text": "Hello"}, {"kind": "Attachment", "artifact": art()}]),
        ),
    )) else {
        panic!()
    };
    assert_eq!(value.revision.get(), 2);
    // A stale expected revision is a durable conflict; a reference with another digest, or one
    // the session's project cannot see, is NotFound and nothing is appended.
    let stale = refused(fx.raw_command(
        &user,
        "sessions.append_message",
        frame("m-2", "1", json!([{"kind": "Text", "text": "again"}])),
    ));
    assert_eq!(stale.code, DomainErrorCode::RevisionConflict);
    let mut wrong = art();
    wrong["byteLength"] = json!("3");
    for bad in [wrong, art_b()] {
        let f = refused(fx.raw_command(
            &user,
            "sessions.append_message",
            frame(
                &fx.key(),
                "2",
                json!([{"kind": "Attachment", "artifact": bad}]),
            ),
        ));
        assert_eq!(
            (f.code, kind_of(&f)),
            (DomainErrorCode::NotFound, "attachment")
        );
    }
    let page = fx.transcript(&user, &sid, None);
    assert_eq!(kinds(&page), ["UserMessage"]);
    assert_eq!(page.entries[0].sequence.get(), 1);
    assert!(!page.has_more);
    // A public message cannot carry a host notice, a tool result or an epoch change.
    let session = fx.session(&sid);
    let notice = tbatch(
        "notice",
        &sid,
        0,
        session.next_sequence,
        vec![entry(
            "n-1",
            session.next_sequence,
            json!({"kind": "HostNotice", "source": "screen", "artifact": art()}),
        )],
    );
    let ctx = user.clone();
    let out = fx.write(move |tx| {
        append_session_batch(
            tx,
            &ctx,
            &actor("user-a"),
            &SessionBatch::entries(notice),
            AppendPath::UserMessage,
        )
    });
    assert!(matches!(refusal(out), BatchRefusal::NotAllowed(_)));
}

// ---------------------------------------------------------------------------------------------
// The session writer

#[test]
fn a_batch_commits_entries_calls_responses_checkpoint_and_usage_together() {
    let fx = Fx::new();
    fx.seed_session("session-a", "agent-a", Some("root-a"));
    let worker = fx.worker("agent-a", "session-a");
    let mut batch = SessionBatch::entries(calls_and_response("session-a"));
    batch.checkpoint = Some(serde_json::from_value(art()).unwrap());
    batch.usage = vec![usage("request-a")];
    let receipt = accepted(fx.append(&worker, batch.clone()));
    assert_eq!(
        (
            receipt.first_sequence,
            receipt.last_sequence,
            receipt.next_sequence()
        ),
        (1, 3, 4)
    );
    let s = fx.session("session-a");
    assert_eq!(
        (s.next_sequence, s.checkpoint_sequence, s.revision),
        (4, 3, 1)
    );
    let sid = id("session-a");
    let (pending, waiting) = fx.read(|r| {
        Ok((
            r.sessions().pending_calls(&sid, 10)?,
            r.sessions().linked_operations(&sid, 10)?,
        ))
    });
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].native_call_id, "call:a");
    assert_eq!(waiting[0].operation_id, Some(OperationId(id("op-b"))));
    // Duplicate replay returns the original receipt and writes nothing.
    assert_eq!(
        fx.append(&worker, batch.clone()),
        BatchOutcome::Replayed(receipt.clone())
    );
    assert_eq!(fx.session("session-a").next_sequence, 4);
    // A changed payload under the same batch identity is refused.
    let mut changed = batch;
    changed.usage.clear();
    assert_eq!(
        refusal(fx.append(&worker, changed)),
        BatchRefusal::PayloadChanged
    );
    // Stale and gap sequences are typed refusals.
    let stale = tbatch("b-stale", "session-a", 0, 3, vec![entry("x", 3, text("x"))]);
    assert_eq!(
        refusal(fx.append(&worker, SessionBatch::entries(stale))),
        BatchRefusal::StaleSequence {
            expected: 4,
            presented: 3
        }
    );
    let gap = tbatch("b-gap", "session-a", 0, 6, vec![entry("y", 6, text("y"))]);
    assert_eq!(
        refusal(fx.append(&worker, SessionBatch::entries(gap))),
        BatchRefusal::SequenceGap {
            expected: 4,
            presented: 6
        }
    );
    // The usage observation is attributed to the session's work.
    let usage = match result(fx.query(
        &fx.user(),
        "history.tool_usage",
        json!({"rootId": "root-a", "page": {"limit": 10}}),
    )) {
        OperationResult::HistoryToolUsage { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(usage.items.0.len(), 1);
    assert_eq!(usage.items.0[0].request_id, id("request-a"));
}

#[test]
fn a_failed_tail_leaves_no_prefix() {
    let fx = Fx::new();
    fx.seed_session("session-a", "agent-a", Some("root-a"));
    let worker = fx.worker("agent-a", "session-a");
    // A valid call followed by a response to an unknown call, then an unknown reference.
    let orphan = tbatch(
        "bad-1",
        "session-a",
        0,
        1,
        vec![
            entry(
                "c-a",
                1,
                json!({"kind": "ToolCallRecorded", "toolCall": call("call:a", 0)}),
            ),
            entry(
                "r-x",
                2,
                json!({"kind": "ToolResponseRecorded", "call": key("call:x", 5),
                    "outcome": {"kind": "Failed", "code": "failed"}}),
            ),
        ],
    );
    assert_eq!(
        refusal(fx.append(&worker, SessionBatch::entries(orphan))),
        BatchRefusal::MissingCall
    );
    let mut missing = art();
    missing["versionId"] = json!("version-z");
    let unknown = tbatch(
        "bad-2",
        "session-a",
        0,
        1,
        vec![
            entry("u-a", 1, text("Hello")),
            entry(
                "n-a",
                2,
                json!({"kind": "NativeItem", "binding": binding(), "artifact": missing}),
            ),
        ],
    );
    assert_eq!(
        refusal(fx.append(&worker, SessionBatch::entries(unknown))),
        BatchRefusal::UnknownReference("native_item")
    );
    let sid = id("session-a");
    let (entries, pending, batch) = fx.read(|r| {
        Ok((
            r.sessions().entries(&sid, 0, 10)?,
            r.sessions().pending_calls(&sid, 10)?,
            r.sessions().batch(&id("session-a"), &id("bad-1"))?,
        ))
    });
    assert!(entries.is_empty() && pending.is_empty() && batch.is_none());
    assert_eq!(fx.session("session-a").next_sequence, 1);
}

#[test]
fn crash_before_and_after_commit() {
    let fx = Fx::new();
    fx.seed_session("session-a", "agent-a", Some("root-a"));
    let worker = fx.worker("agent-a", "session-a");
    let batch = SessionBatch::entries(calls_and_response("session-a"));
    // Crash before commit: the unit of work dies after appending; nothing remains.
    let (ctx, b) = (worker.clone(), batch.clone());
    let crashed = run_write(&*fx.store, WriteOptions::default(), move |tx| {
        accepted(append_session_batch(
            tx,
            &ctx,
            &actor("agent-a"),
            &b,
            AppendPath::Runtime,
        )?);
        Err::<(), _>(StorageError::unavailable(
            Unavailable::Io,
            "simulated crash",
        ))
    });
    assert!(crashed.is_err());
    let fx = fx.restart();
    assert_eq!(fx.session("session-a").next_sequence, 1);
    let receipt = accepted(fx.append(&worker, batch.clone()));
    // Crash after commit: the reopened store replays the original receipt; history is intact.
    let fx = fx.restart();
    assert_eq!(fx.append(&worker, batch), BatchOutcome::Replayed(receipt));
    let page = fx.transcript(&fx.user(), "session-a", None);
    assert_eq!(
        kinds(&page),
        [
            "ToolCallRecorded",
            "ToolCallRecorded",
            "ToolResponseRecorded"
        ]
    );
}

#[test]
fn compaction_adds_an_epoch_and_keeps_the_original_transcript() {
    let fx = Fx::new();
    fx.seed_session("session-a", "agent-a", Some("root-a"));
    let worker = fx.worker("agent-a", "session-a");
    let user = fx.user();
    accepted(fx.append(
        &worker,
        SessionBatch::entries(tbatch(
            "e0",
            "session-a",
            0,
            1,
            vec![entry("u-1", 1, text("one")), entry("u-2", 2, text("two"))],
        )),
    ));
    // A different epoch without an explicit boundary, and a boundary inside the epoch, refuse.
    let implicit = tbatch(
        "e1-bad",
        "session-a",
        1,
        3,
        vec![entry("u-3", 3, text("three"))],
    );
    assert!(matches!(
        refusal(fx.append(&worker, SessionBatch::entries(implicit))),
        BatchRefusal::EpochBoundaryRequired {
            current: 0,
            presented: 1
        }
    ));
    let boundary = json!({"kind": "ContextEpochChanged", "previousEpoch": "0", "nextEpoch": "1",
        "reason": "Compaction", "context": art()});
    let duplicate = tbatch(
        "e0-bad",
        "session-a",
        0,
        3,
        vec![entry("b-0", 3, boundary.clone())],
    );
    assert_eq!(
        refusal(fx.append(&worker, SessionBatch::entries(duplicate))),
        BatchRefusal::DuplicateBoundary
    );
    let mut compaction = SessionBatch::entries(tbatch(
        "e1",
        "session-a",
        1,
        3,
        vec![entry("b-1", 3, boundary)],
    ));
    compaction.context_sources = (0..3)
        .map(|i| ContextSourceInput {
            source_id: id(&format!("source-{i}")),
            kind: "instruction".into(),
            source_revision: 1,
            artifact: (i == 0).then(|| serde_json::from_value(art()).unwrap()),
            token_estimate: 100,
            disposition: if i == 2 {
                SourceDisposition::Omitted
            } else {
                SourceDisposition::Included
            },
        })
        .collect();
    accepted(fx.append(&worker, compaction));
    let s = fx.session("session-a");
    assert_eq!((s.context_epoch, s.revision), (1, 2));
    // The original transcript is still there, entry for entry.
    let page = fx.transcript(&user, "session-a", None);
    assert_eq!(
        kinds(&page),
        ["UserMessage", "UserMessage", "ContextEpochChanged"]
    );
    let sid = id("session-a");
    let epoch = fx.read(|r| r.sessions().epoch(&sid, 1)).expect("epoch");
    assert_eq!((epoch.previous_epoch, epoch.sequence), (0, 3));
    // An old-epoch batch is refused after the switch; late old-bound evidence is kept.
    let old = tbatch(
        "e0-late",
        "session-a",
        0,
        4,
        vec![entry("late", 4, text("late"))],
    );
    assert!(matches!(
        refusal(fx.append(&worker, SessionBatch::entries(old))),
        BatchRefusal::EpochBoundaryRequired { .. }
    ));
    let evidence = tbatch(
        "e1-evidence",
        "session-a",
        1,
        4,
        vec![entry(
            "old-native",
            4,
            json!({"kind": "NativeItem", "binding": binding(), "artifact": art()}),
        )],
    );
    accepted(fx.append(&worker, SessionBatch::entries(evidence)));
    // Briefings and sources are bound to the current epoch.
    let stale = fault(fx.query(
        &user,
        "sessions.context_sources",
        json!({"sessionId": "session-a", "expectedContextEpoch": "0", "page": {"limit": 10}}),
    ));
    assert_eq!(
        (stale.code, kind_of(&stale)),
        (DomainErrorCode::SourceChanged, EPOCH_CHANGED)
    );
    let sources = match result(fx.query(
        &user,
        "sessions.context_sources",
        json!({"sessionId": "session-a", "expectedContextEpoch": "1", "page": {"limit": 10}}),
    )) {
        OperationResult::SessionsContextSources { value } => value,
        other => panic!("{other:?}"),
    };
    let names: Vec<&str> = sources.items.0.iter().map(|s| s.kind.0.as_str()).collect();
    assert_eq!(names, ["instruction", "instruction", "instruction:omitted"]);
}

#[test]
fn operation_updates_are_separate_facts_and_responses_are_written_once() {
    let fx = Fx::new();
    fx.seed_session("session-a", "agent-a", Some("root-a"));
    let worker = fx.worker("agent-a", "session-a");
    accepted(fx.append(
        &worker,
        SessionBatch::entries(calls_and_response("session-a")),
    ));
    let second = tbatch(
        "again",
        "session-a",
        0,
        4,
        vec![entry(
            "r-b-2",
            4,
            json!({"kind": "ToolResponseRecorded", "call": key("call:b", 1),
                "outcome": {"kind": "Completed", "artifacts": []}}),
        )],
    );
    assert_eq!(
        refusal(fx.append(&worker, SessionBatch::entries(second))),
        BatchRefusal::SecondResponse
    );
    let update = |op: &str, b: &str| {
        tbatch(
            b,
            "session-a",
            0,
            4,
            vec![entry(
                b,
                4,
                json!({"kind": "OperationUpdate", "operationId": op, "revision": "2", "artifact": art()}),
            )],
        )
    };
    assert_eq!(
        refusal(fx.append(&worker, SessionBatch::entries(update("op-z", "u-z")))),
        BatchRefusal::UnknownOperation(OperationId(id("op-z")))
    );
    accepted(fx.append(&worker, SessionBatch::entries(update("op-b", "u-b"))));
    let page = fx.transcript(&fx.user(), "session-a", None);
    // The initial ApprovalRequired response is unchanged; the outcome is a later entry.
    let response = serde_json::to_value(&page.entries[2].body).unwrap();
    assert_eq!(response["outcome"]["kind"], "ApprovalRequired");
    assert_eq!(kinds(&page)[3], "OperationUpdate");
}

// ---------------------------------------------------------------------------------------------
// History

#[test]
fn history_pages_are_bounded_scoped_and_cursors_expire() {
    let fx = Fx::new();
    let user = fx.user();
    let agent = fx
        .ctx("agent-p", PrincipalKind::Agent, Some("project-a"))
        .with_lineage(Some(project("project-a")), Some(root("root-a")), None);
    // Commands decided in the work scope carry it into history.
    for _ in 0..3 {
        fx.start(
            &agent,
            json!({"kind": "Project", "projectId": "project-a"}),
            Some("root-a"),
        );
    }
    let first = match result(fx.events(&user, "root-a", None)) {
        OperationResult::HistoryEvents { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(first.items.0.len(), 3);
    assert!(
        first
            .items
            .0
            .iter()
            .all(|e: &Event| e.root_id == Some(root("root-a")) && e.actor_id == actor("agent-p"))
    );
    // Lineage in pages of two, in committed-sequence order.
    let mut cursor = None;
    let mut seen = Vec::new();
    loop {
        let page = lineage_page(fx.lineage(&user, "root-a", 2, cursor.as_deref()));
        assert!(page.items.0.len() <= 2);
        seen.extend(page.items.0.iter().map(|l| l.id.as_str().to_owned()));
        match page.next_cursor {
            Some(c) => cursor = Some(c.0),
            None => break,
        }
    }
    // task-a, three session links and three command decisions.
    assert_eq!(seen.len(), 7, "{seen:?}");
    let page = lineage_page(fx.lineage(&user, "root-a", 2, None));
    let c = page.next_cursor.unwrap().0;
    // Another principal, a changed root scope or a forged cursor cannot continue the list.
    let other = fx.ctx("user-b", PrincipalKind::Human, None);
    assert_eq!(
        fault(fx.lineage(&other, "root-a", 2, Some(&c))).code,
        DomainErrorCode::CursorExpired
    );
    let forged = format!("{}0", &c[..c.len() - 1]);
    assert_eq!(
        fault(fx.lineage(&user, "root-a", 2, Some(&forged))).code,
        DomainErrorCode::CursorExpired
    );
    fx.store
        .with_writer_connection(|c| {
            super::super::conn::db(c.execute(
                "UPDATE work_items SET scope_revision=2 WHERE id='root-a'",
                [],
            ))
        })
        .unwrap();
    assert_eq!(
        fault(fx.lineage(&user, "root-a", 2, Some(&c))).code,
        DomainErrorCode::CursorExpired
    );
    // A project-b agent does not learn that project-a work exists.
    let outsider = fx.ctx("agent-b", PrincipalKind::Agent, Some("project-b"));
    assert_eq!(
        fault(fx.lineage(&outsider, "root-a", 2, None)).code,
        DomainErrorCode::NotFound
    );
    assert_eq!(
        fault(fx.events(&outsider, "root-a", None)).code,
        DomainErrorCode::NotFound
    );
    // Revoked authority is refused at once.
    let by = user.clone();
    fx.write(move |tx| {
        let rev = tx
            .authority()
            .principal(&actor("agent-p"))?
            .unwrap()
            .revision;
        auth::revoke_principal(tx, &by, &actor("agent-p"), rev)
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    });
    assert_eq!(
        fault(fx.lineage(&agent, "root-a", 2, None)).code,
        DomainErrorCode::PermissionDenied
    );
}

#[test]
fn lineage_navigates_from_work_to_sessions_and_tool_actions_and_back() {
    let fx = Fx::new();
    let user = fx.user();
    fx.seed_session("session-a", "agent-a", Some("root-a"));
    // A work session started by an agent in that work links to its root, and the command that
    // started it is attached to the root; the seeded runtime session carries the calls.
    let agent = fx
        .ctx("agent-p", PrincipalKind::Agent, Some("project-a"))
        .with_lineage(Some(project("project-a")), Some(root("root-a")), None);
    let s = fx.start(
        &agent,
        json!({"kind": "Project", "projectId": "project-a"}),
        Some("root-a"),
    );
    let worker = fx.worker("agent-a", "session-a");
    accepted(fx.append(
        &worker,
        SessionBatch::entries(calls_and_response("session-a")),
    ));
    let page = lineage_page(fx.lineage(&user, "root-a", 50, None));
    let kinds: Vec<&str> = page.items.0.iter().map(|l| l.kind.0.as_str()).collect();
    assert_eq!(kinds[0], "task", "{kinds:?}");
    assert!(
        kinds.contains(&"session") && kinds.contains(&"command"),
        "{kinds:?}"
    );
    assert_eq!(kinds.iter().filter(|k| **k == "tool_call").count(), 2);
    assert_eq!(kinds.iter().filter(|k| **k == "tool_response").count(), 1);
    // From the task, its feature.
    let task = lineage_page(fx.lineage(&user, "task-a", 50, None));
    assert_eq!(task.items.0[0].kind.0, "parent_feature");
    assert_eq!(task.items.0[0].related_id, id("root-a"));
    // From the session (and from a tool call entry) back to the work.
    let back = s.id.clone();
    let links = fx.read(|r| r.sessions().links_to("session", back.as_str(), 10));
    assert_eq!(links[0].root_id, root("root-a"));
    let call_links = fx.read(|r| r.sessions().links_to("transcript_entry", "c-a", 10));
    assert_eq!(
        (call_links[0].root_id.clone(), call_links[0].kind.as_str()),
        (root("root-a"), "tool_call")
    );
    let OperationResult::SessionsGet { value } =
        result(fx.query(&user, "sessions.get", json!({"sessionId": s.id.as_str()})))
    else {
        panic!()
    };
    assert_eq!(value.root_id, Some(root("root-a")));
}

#[test]
fn corrections_preserve_prior_records() {
    let fx = Fx::new();
    let user = fx.user();
    let git = |hex: char, supersedes: Option<Id>| NewHistoryLink {
        root_id: root("root-a"),
        kind: LinkKind::PullRequest,
        git: Some(GitFacts {
            repository_id: id("repo-a"),
            worktree_id: None,
            branch: Some("feature/a".into()),
            pull_request_url: Some("https://example.test/pr/1".into()),
            head: serde_json::from_value(
                json!({"format": "Sha1", "hex": hex.to_string().repeat(40)}),
            )
            .unwrap(),
        }),
        artifact: None,
        target: None,
        summary: "Pull request opened".into(),
        supersedes,
    };
    let ctx = user.clone();
    let first = fx.write(move |tx| Ok(record_link(tx, &ctx, git('a', None))?.expect("recorded")));
    let ctx = user.clone();
    let prior = first.id.clone();
    let second =
        fx.write(move |tx| Ok(record_link(tx, &ctx, git('b', Some(prior)))?.expect("recorded")));
    let ctx = user.clone();
    let evidence = fx.write(move |tx| {
        Ok(record_link(
            tx,
            &ctx,
            NewHistoryLink {
                root_id: root("root-a"),
                kind: LinkKind::Evidence,
                git: None,
                artifact: Some(serde_json::from_value(art()).unwrap()),
                target: None,
                summary: "Test run".into(),
                supersedes: None,
            },
        )?
        .expect("recorded"))
    });
    let ctx = user.clone();
    let reversed = evidence.id.clone();
    fx.write(move |tx| {
        Ok(record_link(
            tx,
            &ctx,
            NewHistoryLink {
                root_id: root("root-a"),
                kind: LinkKind::Correction,
                git: None,
                artifact: None,
                target: None,
                summary: "Run was against the wrong head".into(),
                supersedes: Some(reversed),
            },
        )?
        .expect("recorded"))
    });
    // Evidence from another project cannot be linked.
    let ctx = user.clone();
    let foreign = fx.write(move |tx| {
        record_link(
            tx,
            &ctx,
            NewHistoryLink {
                root_id: root("root-a"),
                kind: LinkKind::Evidence,
                git: None,
                artifact: Some(serde_json::from_value(art_b()).unwrap()),
                target: None,
                summary: "Foreign".into(),
                supersedes: None,
            },
        )
    });
    assert!(foreign.is_err());
    let git_links = match result(fx.query(
        &user,
        "history.git_links",
        json!({"rootId": "root-a", "page": {"limit": 10}}),
    )) {
        OperationResult::HistoryGitLinks { value } => value,
        other => panic!("{other:?}"),
    };
    let heads: Vec<&str> = git_links
        .items
        .0
        .iter()
        .map(|g: &AttributedGitLink| g.head.hex.as_str())
        .collect();
    assert_eq!(heads, ["a".repeat(40), "b".repeat(40)]);
    let evidence_links = match result(fx.query(
        &user,
        "history.evidence_links",
        json!({"rootId": "root-a", "page": {"limit": 10}}),
    )) {
        OperationResult::HistoryEvidenceLinks { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(evidence_links.items.0.len(), 1);
    assert_eq!(evidence_links.items.0[0].id, evidence.id);
    let _: &EvidenceLink = &evidence_links.items.0[0];
    let lineage = lineage_page(fx.lineage(&user, "root-a", 50, None));
    let correction = lineage
        .items
        .0
        .iter()
        .find(|l| l.kind.0 == "correction")
        .unwrap();
    assert_eq!(correction.parent_id, Some(evidence.id.clone()));
    let replacement = lineage.items.0.iter().find(|l| l.id == second.id).unwrap();
    assert_eq!(replacement.parent_id, Some(first.id.clone()));
    assert!(lineage.items.0.iter().any(|l| l.id == first.id));
}

#[test]
fn operation_status_and_command_receipts_are_scoped() {
    let fx = Fx::new();
    let user = fx.user();
    let status = |ctx: &InvocationContext, op: &str| {
        fx.raw_query(
            ctx,
            "history.operation_status",
            json!({"protocol": "1.0-draft.1", "queryId": "q-o",
                "query": {"kind": "history.operation_status", "operationId": op}}),
        )
    };
    let OperationResult::HistoryOperationStatus { value } = result(status(&user, "op-b")) else {
        panic!()
    };
    assert_eq!(
        value.state,
        devworks_core::protocol::OperationState::IntentRecorded
    );
    let outsider = fx.ctx("agent-b", PrincipalKind::Agent, Some("project-b"));
    assert_eq!(
        fault(status(&outsider, "op-b")).code,
        DomainErrorCode::NotFound
    );

    let receipt = |ctx: &InvocationContext, key: &str| {
        fx.raw_query(
            ctx,
            "history.command_receipt",
            json!({"protocol": "1.0-draft.1", "queryId": "q-r",
                "query": {"kind": "history.command_receipt", "requestKey": key}}),
        )
    };
    let OperationResult::HistoryCommandReceipt { value } = result(receipt(&user, "never")) else {
        panic!()
    };
    assert!(matches!(value.state, ReceiptLookupState::NotFound));
    fx.command_keyed(
        &user,
        "sessions.start",
        "start-r",
        json!({"scope": {"kind": "Workspace"}, "title": "Home"}),
    );
    // The receipt exists, but the contract's Recorded state needs a durable reply artifact,
    // which receipts do not have: reported unavailable, never guessed.
    let f = fault(receipt(&user, "start-r"));
    assert_eq!(
        (f.code, kind_of(&f)),
        (DomainErrorCode::ViewUnavailable, RECEIPT_REPLY_UNAVAILABLE)
    );
    // Another principal's lookup of the same key is its own (absent) receipt.
    let other = fx.ctx("user-b", PrincipalKind::Human, None);
    let OperationResult::HistoryCommandReceipt { value } = result(receipt(&other, "start-r"))
    else {
        panic!()
    };
    assert!(matches!(value.state, ReceiptLookupState::NotFound));
}

// ---------------------------------------------------------------------------------------------
// Authority and scope

#[test]
fn reviewer_and_agent_scope_denials() {
    let fx = Fx::new();
    let s = fx.work_session();
    let sid = s.id.as_str().to_owned();
    let workspace = json!({"scope": {"kind": "Workspace"}, "title": "x"});
    // A code reviewer reads but never writes, under any profile.
    let reviewer = fx.ctx("reviewer-a", PrincipalKind::Agent, None);
    assert_eq!(
        rejected(fx.command(&reviewer, "sessions.start", workspace.clone())),
        Rejection::PermissionDenied
    );
    result(fx.query(&reviewer, "sessions.get", json!({"sessionId": sid})));
    // An agent without the sessions family selected is refused.
    let tools = fx.ctx("agent-t", PrincipalKind::Agent, None);
    let f = fault(fx.query(&tools, "sessions.get", json!({"sessionId": sid})));
    assert_eq!(kind_of(&f), "authority:ToolNotSelected");
    // A project-b agent cannot see or act on project-a sessions, or open a Home session.
    let outsider = fx.ctx("agent-b", PrincipalKind::Agent, Some("project-b"));
    assert_eq!(
        fault(fx.query(&outsider, "sessions.get", json!({"sessionId": sid}))).code,
        DomainErrorCode::NotFound
    );
    let f = fault(fx.query(
        &outsider,
        "sessions.list",
        json!({"scope": {"kind": "Project", "projectId": "project-a"}, "page": {"limit": 5}}),
    ));
    assert_eq!(f.code, DomainErrorCode::PermissionDenied);
    assert_eq!(
        refused(fx.command(&outsider, "sessions.start", workspace)).code,
        DomainErrorCode::PermissionDenied
    );
    let f = refused(fx.command(
        &outsider,
        "sessions.end",
        json!({"sessionId": sid, "expectedRevision": "1"}),
    ));
    assert_eq!(f.code, DomainErrorCode::NotFound);
    // The session writer accepts only the host runtime for the bound session's agent.
    fx.seed_session("session-a", "agent-a", Some("root-a"));
    let batch = || {
        SessionBatch::entries(tbatch(
            "w-1",
            "session-a",
            0,
            1,
            vec![entry("w", 1, text("w"))],
        ))
    };
    let package = fx
        .ctx("package-a", PrincipalKind::Package, Some("project-a"))
        .with_session(id("session-a"));
    assert_eq!(
        refusal(fx.append(&package, batch())),
        BatchRefusal::Unauthorized("HostOnly")
    );
    let foreign = fx.worker("agent-p", "session-a");
    assert_eq!(
        refusal(fx.append(&foreign, batch())),
        BatchRefusal::Unauthorized("NotSessionAgent")
    );
    let unbound = fx.worker("agent-a", "session-other");
    assert_eq!(
        refusal(fx.append(&unbound, batch())),
        BatchRefusal::Unauthorized("ForeignSession")
    );
    let worker = fx.worker("agent-a", "session-a");
    let receipt = accepted(fx.append(&worker, batch()));
    let by = fx.user();
    fx.write(move |tx| {
        let rev = tx
            .authority()
            .principal(&actor("agent-a"))?
            .unwrap()
            .revision;
        auth::revoke_principal(tx, &by, &actor("agent-a"), rev)
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    });
    let next = SessionBatch::entries(tbatch(
        "w-2",
        "session-a",
        0,
        2,
        vec![entry("w2", 2, text("w"))],
    ));
    assert_eq!(
        refusal(fx.append(&worker, next)),
        BatchRefusal::Unauthorized("PrincipalRevoked")
    );
    // A revoked writer may still recover its own committed receipt.
    assert_eq!(fx.append(&worker, batch()), BatchOutcome::Replayed(receipt));
}

#[test]
fn the_briefing_is_bounded_and_continues_through_context_sources() {
    let fx = Fx::new();
    fx.seed_session("session-a", "agent-a", Some("root-a"));
    let worker = fx.worker("agent-a", "session-a");
    let mut batch = SessionBatch::entries(calls_and_response("session-a"));
    batch.checkpoint = Some(serde_json::from_value(art()).unwrap());
    batch.context_sources = (0..40)
        .map(|i| ContextSourceInput {
            source_id: id(&format!("source-{i}")),
            kind: "retrieved".into(),
            source_revision: 1,
            artifact: None,
            token_estimate: 10,
            disposition: SourceDisposition::Included,
        })
        .collect();
    accepted(fx.append(&worker, batch));
    let user = fx.user();
    let brief = match result(fx.query(
        &user,
        "sessions.briefing",
        json!({"sessionId": "session-a", "expectedContextEpoch": "0"}),
    )) {
        OperationResult::SessionsBriefing { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(brief.items.0.len(), BRIEFING_LIMIT);
    let names: Vec<&str> = brief
        .items
        .0
        .iter()
        .map(|s: &ContextSource| s.kind.0.as_str())
        .collect();
    assert_eq!(
        &names[..4],
        [
            "work:feature",
            "checkpoint",
            "pending_call:core/read",
            "operation:ApprovalRequired"
        ]
    );
    let rest = match result(fx.query(
        &user,
        "sessions.context_sources",
        json!({"sessionId": "session-a", "expectedContextEpoch": "0",
            "page": {"limit": 50, "cursor": brief.next_cursor.unwrap().0}}),
    )) {
        OperationResult::SessionsContextSources { value } => value,
        other => panic!("{other:?}"),
    };
    // 28 recorded sources fit in the briefing; the remaining 12 continue here.
    assert_eq!(rest.items.0.len(), 12);
    assert!(!rest.has_more);
}

// ---------------------------------------------------------------------------------------------
// Retained CodeGraph entries and migration

#[test]
fn retained_codegraph_entries_map_to_registered_operations() {
    let fx = Fx::new();
    let plan: Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../docs/architecture/implementation-plan.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let slices = plan["slices"].as_array().unwrap();
    let slice = slices.iter().find(|s| s["key"] == "sessions").unwrap();
    let owned: Vec<&str> = slice["ownedOperations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o.as_str().unwrap())
        .collect();
    let entries: std::collections::HashMap<&str, &Value> = plan["sourceEntries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["id"].as_str().unwrap(), e))
        .collect();
    let ids = slice["sourceEntryIds"].as_array().unwrap();
    assert_eq!(ids.len(), 37);
    for entry_id in ids {
        let e = entries[entry_id.as_str().unwrap()];
        assert!(
            matches!(e["disposition"].as_str(), Some("native" | "adapt")),
            "{entry_id}"
        );
        for op in e["targetOperations"].as_array().unwrap() {
            let op = op.as_str().unwrap();
            if owned.contains(&op) {
                assert!(
                    fx.cases.has_query(op) || fx.cases.has_command(op),
                    "{op} not registered"
                );
            } else {
                // Owned by another item (run control, usage, work views): it is not advertised
                // here, and that item is recorded as its owner.
                assert!(!fx.cases.has_query(op) && !fx.cases.has_command(op), "{op}");
                assert!(
                    slices.iter().any(|s| s["key"] != "sessions"
                        && s["ownedOperations"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|o| o == op)),
                    "{op} has no owner"
                );
            }
        }
    }
    for op in owned {
        assert!(fx.cases.has_query(op) || fx.cases.has_command(op), "{op}");
    }
}

#[test]
fn the_session_migration_is_recorded_and_keeps_the_floors() {
    let m = MIGRATIONS
        .iter()
        .find(|m| m.version == 7)
        .expect("session migration");
    assert_eq!(
        m.checksum(),
        MIGRATION_0007_SHA256,
        "an applied migration was edited"
    );
    assert_eq!((m.minimum_reader, m.minimum_writer), (5, 5));
}

// ---------------------------------------------------------------------------------------------
// Recorded native scenarios (docs/architecture/contracts/provider-package-scenarios.json)

fn scenarios() -> Vec<Value> {
    serde_json::from_str(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../docs/architecture/contracts/provider-package-scenarios.json"),
        )
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn recorded_transcript_scenarios_replay_through_the_session_writer() {
    let cases: Vec<Value> = scenarios()
        .into_iter()
        .filter(|s| s["kind"] == "Transcript")
        .collect();
    assert_eq!(cases.len(), 4);
    for case in cases {
        let fx = Fx::new();
        fx.seed_session("session-a", "agent-a", Some("root-a"));
        let worker = fx.worker("agent-a", "session-a");
        let batches = case["batches"].as_array().unwrap();
        let expected = case["expected"].as_array().unwrap();
        let next = case["expectedNext"].as_array().unwrap();
        assert_eq!(batches.len(), expected.len());
        for (i, b) in batches.iter().enumerate() {
            let batch: TranscriptBatch = serde_json::from_value(b.clone()).unwrap();
            let outcome = fx.append(&worker, SessionBatch::entries(batch));
            let state = match &outcome {
                BatchOutcome::Accepted(_) => "Accepted",
                BatchOutcome::Replayed(_) => "Replayed",
                BatchOutcome::Refused(_) => "Rejected",
            };
            assert_eq!(state, expected[i], "{} step {i}", case["id"]);
            // Each recorded rejection is refused for its specific reason.
            let reason = match (case["id"].as_str().unwrap(), i) {
                ("parallel-wait-replay-and-later-operation-event", 2) => {
                    Some(BatchRefusal::SecondResponse)
                }
                ("invalid-tail-does-not-persist-prefix", 0) => Some(BatchRefusal::MissingCall),
                ("batch-key-payload-conflict", 1) => Some(BatchRefusal::PayloadChanged),
                ("epoch-boundary-and-old-native-evidence", 1) => {
                    Some(BatchRefusal::EpochBoundaryRequired {
                        current: 1,
                        presented: 0,
                    })
                }
                _ => None,
            };
            if let Some(reason) = reason {
                assert_eq!(
                    outcome,
                    BatchOutcome::Refused(reason),
                    "{} step {i}",
                    case["id"]
                );
            } else {
                assert_ne!(
                    state, "Rejected",
                    "{} step {i}: unexpected rejection",
                    case["id"]
                );
            }
            assert_eq!(
                fx.session("session-a").next_sequence,
                next[i].as_i64().unwrap(),
                "{} step {i}",
                case["id"]
            );
        }
        // What was accepted is exactly what the durable transcript holds, in order.
        let page = fx.transcript(&worker, "session-a", None);
        assert_eq!(
            page.entries.len() as i64,
            fx.session("session-a").next_sequence - 1
        );
    }
}

fn provider_event(sequence: i64, event: Value) -> ProviderEvent {
    serde_json::from_value(json!({"protocol": "1.0-draft.1", "requestId": "request-a",
        "attemptId": "attempt-a", "binding": binding(), "sequence": sequence.to_string(),
        "event": event}))
    .unwrap()
}
fn delta(block: u32, text: &str) -> Value {
    json!({"kind": "TextDelta", "blockIndex": block.to_string(), "text": text})
}
/// Appends what `turn` has not flushed yet as one batch at the session's next sequence.
fn flush(fx: &Fx, ctx: &InvocationContext, turn: &mut TurnAssembler, batch: &str) -> usize {
    append_bodies(fx, ctx, turn.take(), batch)
}
fn append_bodies(
    fx: &Fx,
    ctx: &InvocationContext,
    bodies: Vec<TranscriptBody>,
    batch: &str,
) -> usize {
    let n = bodies.len();
    if n > 0 {
        let next = fx.session("session-a").next_sequence;
        let mut k = 0;
        let b = batch_of(&id("session-a"), id(batch), 0, next, bodies, || {
            k += 1;
            id(&format!("{batch}-{k}"))
        });
        accepted(fx.append(ctx, SessionBatch::entries(b)));
    }
    n
}
fn texts(page: &TranscriptPage) -> Vec<(String, bool)> {
    page.entries
        .iter()
        .filter_map(|e| match &e.body {
            TranscriptBody::AssistantAttempt {
                visible_text,
                interrupted,
                ..
            } => Some((visible_text.clone(), *interrupted)),
            _ => None,
        })
        .collect()
}

#[test]
fn recorded_stream_scenarios_decide_what_becomes_durable() {
    use devworks_core::protocol::provider::TurnOutcome;
    let cases: Vec<Value> = scenarios()
        .into_iter()
        .filter(|s| s["kind"] == "Stream")
        .collect();
    assert_eq!(cases.len(), 8);
    for case in cases {
        let mut turn = TurnAssembler::new();
        let events = case["events"].as_array().unwrap();
        for (i, e) in events.iter().enumerate() {
            let event: ProviderEvent = serde_json::from_value(e.clone()).unwrap();
            let ok = turn.accept(&event).is_ok();
            assert_eq!(ok, case["expectedAccept"][i], "{} step {i}", case["id"]);
            assert_eq!(
                turn.tools_ready(),
                case["expectedReady"][i],
                "{} step {i}",
                case["id"]
            );
        }
        let fx = Fx::new();
        fx.seed_session("session-a", "agent-a", Some("root-a"));
        let worker = fx.worker("agent-a", "session-a");
        flush(&fx, &worker, &mut turn, "turn-1");
        // A stream that never reached its terminal event ends at EOF, interrupted.
        if turn.terminal().is_none() {
            let rest = turn.end_of_stream();
            append_bodies(&fx, &worker, rest, "eof");
        }
        let page = fx.transcript(&worker, "session-a", None);
        let calls: Vec<String> = page
            .entries
            .iter()
            .filter_map(|e| match &e.body {
                TranscriptBody::ToolCallRecorded { tool_call } => {
                    Some(tool_call.call.native_call_id.clone())
                }
                _ => None,
            })
            .collect();
        let id_ = case["id"].as_str().unwrap();
        if turn.tools_ready() {
            // Assembled in recorded ordinal order, whatever order they arrived in, and a
            // tool-only turn writes no assistant text.
            assert_eq!(calls, ["call:a", "call:b"], "{id_}");
            assert!(texts(&page).is_empty(), "{id_}");
        } else {
            assert!(
                calls.is_empty(),
                "{id_}: unready calls must not become durable"
            );
        }
        let expected: Vec<(String, bool)> = match id_ {
            "text-completion" => vec![("Hello".into(), false)],
            // Text was the last content of an interrupted turn: its last chunk is marked.
            "attempt-change-fenced" => vec![("Partial".into(), true)],
            // An interrupted turn without trailing text: one interruption marker.
            "incomplete-turn-retains-unready-call"
            | "eof-with-call-is-not-ready"
            | "duplicate-call-does-not-advance-stream" => vec![(String::new(), true)],
            _ => vec![],
        };
        assert_eq!(texts(&page), expected, "{id_}");
        if matches!(turn.terminal(), Some(TurnOutcome::Completed)) {
            assert!(texts(&page).iter().all(|(_, i)| !i), "{id_}");
        }
    }
}

/// Feeds `events` to a new assembler, taking after the event indexes in `flush_after`, and
/// returns every entry taken (in order), ending the stream at EOF when it has no terminal.
fn assemble(events: &[Value], flush_after: &[usize]) -> Vec<TranscriptBody> {
    let mut turn = TurnAssembler::new();
    let mut out = Vec::new();
    for (i, e) in events.iter().enumerate() {
        turn.accept(&provider_event(i as i64 + 1, e.clone()))
            .unwrap();
        if flush_after.contains(&i) {
            out.extend(turn.take());
        }
    }
    if turn.terminal().is_some() {
        out.extend(turn.take());
    } else {
        out.extend(turn.end_of_stream());
    }
    out
}
fn shape(bodies: &[TranscriptBody]) -> Vec<String> {
    bodies
        .iter()
        .map(|b| match b {
            TranscriptBody::AssistantAttempt {
                visible_text,
                interrupted,
                ..
            } => format!("text:{visible_text}:{interrupted}"),
            TranscriptBody::NativeItem { .. } => "native".into(),
            TranscriptBody::ToolCallRecorded { tool_call } => {
                format!("call:{}", tool_call.call.native_call_id)
            }
            other => format!("{other:?}"),
        })
        .collect()
}
fn native_event() -> Value {
    json!({"kind": "NativeContinuation", "binding": binding(), "artifact": art()})
}
fn terminal(outcome: &str) -> Value {
    json!({"kind": "Terminal", "outcome": outcome})
}

#[test]
fn the_same_stream_yields_the_same_entries_whatever_the_flush_timing() {
    // Interleaved blocks 0, 1, 0: each block change starts a new run, in arrival order.
    let interleaved = [
        delta(0, "a"),
        delta(1, "b"),
        delta(0, "c"),
        delta(0, "d"),
        terminal("Completed"),
    ];
    let expected = ["text:a:false", "text:b:false", "text:cd:false"];
    for flushes in [&[][..], &[0], &[1], &[2], &[0, 1, 2, 3]] {
        assert_eq!(
            shape(&assemble(&interleaved, flushes)),
            expected,
            "{flushes:?}"
        );
    }
    // Text after a native item comes after it, even within one block.
    let around_native = [
        delta(0, "a"),
        native_event(),
        delta(0, "c"),
        terminal("Completed"),
    ];
    for flushes in [&[][..], &[0], &[1], &[0, 1, 2]] {
        assert_eq!(
            shape(&assemble(&around_native, flushes)),
            ["text:a:false", "native", "text:c:false"],
            "{flushes:?}"
        );
    }
    // Interrupted turns: the last chunk is marked when text is last, a marker otherwise.
    for flushes in [&[][..], &[0], &[0, 1]] {
        assert_eq!(
            shape(&assemble(
                &[delta(0, "a"), native_event(), terminal("Incomplete")],
                flushes
            )),
            ["text:a:false", "native", "text::true"],
            "{flushes:?}"
        );
        assert_eq!(
            shape(&assemble(
                &[delta(0, "a"), delta(0, "b"), terminal("Cancelled")],
                flushes
            )),
            ["text:ab:true"],
            "{flushes:?}"
        );
    }
}

#[test]
fn empty_deltas_and_tool_only_turns_write_no_text() {
    assert!(assemble(&[delta(0, ""), terminal("Completed")], &[0]).is_empty());
    assert_eq!(
        shape(&assemble(
            &[
                delta(0, "a"),
                delta(0, ""),
                delta(1, ""),
                delta(0, "b"),
                terminal("Completed")
            ],
            &[1, 2]
        )),
        ["text:ab:false"]
    );
    let tool_only = [
        json!({"kind": "ToolCallComplete", "toolCall": call("call:a", 0)}),
        delta(0, ""),
        terminal("ToolCallsReady"),
    ];
    assert_eq!(shape(&assemble(&tool_only, &[0, 1])), ["call:call:a"]);
}

#[test]
fn a_crash_mid_stream_after_a_partial_flush_keeps_the_partial_and_marks_it_interrupted() {
    let fx = Fx::new();
    fx.seed_session("session-a", "agent-a", Some("root-a"));
    let worker = fx.worker("agent-a", "session-a");
    let mut turn = TurnAssembler::new();
    // A block change ends the first run, so it can be flushed while the stream runs.
    turn.accept(&provider_event(1, delta(0, "Hel"))).unwrap();
    turn.accept(&provider_event(2, delta(1, "lo"))).unwrap();
    assert_eq!(flush(&fx, &worker, &mut turn, "partial"), 1);
    // The process dies: the open run "lo" was only a preview and is not durable.
    drop(turn);
    let fx = fx.restart();
    let marker = interruption_marker(id("request-a"), AttemptId(id("attempt-a")));
    append_bodies(&fx, &worker, vec![marker], "recovered");
    let page = fx.transcript(&fx.user(), "session-a", None);
    assert_eq!(
        texts(&page),
        [("Hel".to_owned(), false), (String::new(), true)]
    );
}
#[test]
fn long_streamed_text_is_persisted_as_ordered_bounded_utf8_chunks() {
    let fx = Fx::new();
    fx.seed_session("session-a", "agent-a", Some("root-a"));
    let worker = fx.worker("agent-a", "session-a");
    let mut turn = TurnAssembler::new();
    // A two-byte prefix before three-byte characters puts byte 65536 inside a character, so
    // the split must step back to a character boundary.
    let text = format!("ab{}", "\u{20ac}".repeat(30_000));
    assert!(!text.is_char_boundary(TEXT_CHUNK));
    // Streamed as several bounded deltas of one content block.
    let parts = ["ab".to_owned(), "\u{20ac}".repeat(10_000)];
    let mut seq = 0;
    for part in [&parts[0], &parts[1], &parts[1], &parts[1]] {
        seq += 1;
        turn.accept(&provider_event(seq, delta(0, part))).unwrap();
    }
    // Before the batch, the streamed text is only a preview: nothing durable moved.
    assert_eq!(fx.session("session-a").next_sequence, 1);
    turn.accept(&provider_event(
        seq + 1,
        json!({"kind": "Terminal", "outcome": "Incomplete"}),
    ))
    .unwrap();
    flush(&fx, &worker, &mut turn, "turn-1");
    let page = fx.transcript(&fx.user(), "session-a", None);
    let chunks = texts(&page);
    assert_eq!(chunks.len(), 2);
    assert!(chunks.iter().all(|(t, _)| t.len() <= TEXT_CHUNK));
    assert_eq!((chunks[0].1, chunks[1].1), (false, true));
    assert_eq!(chunks[0].0.len(), TEXT_CHUNK - 2);
    let joined: String = chunks.into_iter().map(|(t, _)| t).collect();
    assert_eq!(joined, text);
}

#[test]
fn a_command_decision_is_attached_to_its_target_and_keeps_a_resolvable_project() {
    let fx = Fx::new();
    // A UI command (no lineage) that names its work target is attached to that work.
    fx.command(
        &fx.user(),
        "sessions.start",
        json!({"scope": {"kind": "Project", "projectId": "project-a"}, "rootId": "task-a",
            "title": "t"}),
    );
    // Without a target, the host lineage root; a lineage root that does not exist keeps the
    // invocation's (existing) project.
    let scoped = fx
        .user()
        .with_lineage(Some(project("project-a")), Some(root("root-a")), None);
    let workspace = json!({"scope": {"kind": "Project", "projectId": "project-a"}, "title": "t"});
    fx.command(&scoped, "sessions.start", workspace.clone());
    let dangling =
        fx.user()
            .with_lineage(Some(project("project-a")), Some(root("root-gone")), None);
    fx.command(&dangling, "sessions.start", workspace.clone());
    // A target outside the invocation's project is never attached to it.
    let elsewhere = fx
        .user()
        .with_lineage(Some(project("project-b")), None, None);
    fx.command(
        &elsewhere,
        "sessions.start",
        json!({"scope": {"kind": "Project", "projectId": "project-a"}, "rootId": "root-a",
            "title": "t"}),
    );
    let decided = fx.read(|r| {
        Ok(r.history()
            .page(&EventFilter::All, 0, 500)?
            .events
            .into_iter()
            .filter(|e| e.kind == "CommandDecided")
            .map(|e| (e.project_id, e.root_id, e.payload["subject"].clone()))
            .collect::<Vec<_>>())
    });
    let n = decided.len();
    let tail = &decided[n - 4..];
    assert_eq!(
        (tail[0].0.clone(), tail[0].1.clone()),
        (Some(project("project-a")), Some(root("task-a")))
    );
    assert_eq!(tail[0].2["kind"], "root");
    assert_eq!(
        (tail[1].0.clone(), tail[1].1.clone(), tail[1].2.is_null()),
        (Some(project("project-a")), Some(root("root-a")), true)
    );
    assert_eq!(
        (tail[2].0.clone(), tail[2].1.clone()),
        (Some(project("project-a")), None)
    );
    assert_eq!(
        (tail[3].0.clone(), tail[3].1.clone()),
        (Some(project("project-b")), None)
    );
    // history.events of the target publishes the decision with its real subject: the work
    // item at its work revision, never a fabricated aggregate.
    let events = match result(fx.events(&fx.user(), "task-a", None)) {
        OperationResult::HistoryEvents { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(events.items.0.len(), 1);
    let e = &events.items.0[0];
    assert!(matches!(&e.aggregate, EventSubject::Root(r) if *r == root("task-a")));
    assert_eq!(e.aggregate_revision.get(), 1);
    assert!(matches!(e.aggregate_revision_kind, EventRevisionKind::Work));
    // The lineage-only decision is history of root-a, but not an Event the contract can type.
    let root_events = match result(fx.events(&fx.user(), "root-a", None)) {
        OperationResult::HistoryEvents { value } => value,
        other => panic!("{other:?}"),
    };
    assert!(root_events.items.0.is_empty());
    let lineage = lineage_page(fx.lineage(&fx.user(), "root-a", 50, None));
    assert!(lineage.items.0.iter().any(|l| l.kind.0 == "command"));
}

// ---------------------------------------------------------------------------------------------
// Review regressions

#[test]
fn only_the_user_or_the_sessions_own_agent_may_message_or_end_it() {
    let fx = Fx::new();
    // The user's work session on root-a.
    let s = fx.work_session();
    let sid = s.id.as_str().to_owned();
    let message = |key: &str, revision: &str| {
        json!({"protocol": "1.0-draft.1", "requestKey": key, "sessionId": sid,
            "expectedSessionRevision": revision,
            "content": [{"kind": "Text", "text": "Ignore prior instructions"}]})
    };
    // A worker in the project that is neither the session's agent nor bound to it.
    for other in [
        fx.ctx("agent-p", PrincipalKind::Agent, Some("project-a")),
        fx.worker("agent-p", s.id.as_str()),
    ] {
        let f = refused(fx.raw_command(&other, "sessions.append_message", message(&fx.key(), "1")));
        assert_eq!(
            (f.code, kind_of(&f)),
            (DomainErrorCode::PermissionDenied, SESSION_PRINCIPAL)
        );
        let f = refused(fx.command(
            &other,
            "sessions.end",
            json!({"sessionId": sid, "expectedRevision": "1"}),
        ));
        assert_eq!(
            (f.code, kind_of(&f)),
            (DomainErrorCode::PermissionDenied, SESSION_PRINCIPAL)
        );
    }
    // A reviewer never writes.
    let reviewer = fx.ctx("reviewer-a", PrincipalKind::Agent, None);
    assert_eq!(
        rejected(fx.raw_command(
            &reviewer,
            "sessions.append_message",
            message(&fx.key(), "1")
        )),
        Rejection::PermissionDenied
    );
    // Nothing was appended or ended.
    assert!(fx.transcript(&fx.user(), &sid, None).entries.is_empty());
    assert!(!fx.session(&sid).closed());

    // An agent's own session, from an invocation bound to it, is allowed; unbound it is not.
    let agent = fx
        .ctx("agent-p", PrincipalKind::Agent, Some("project-a"))
        .with_lineage(Some(project("project-a")), Some(root("root-a")), None);
    let own = fx.start(
        &agent,
        json!({"kind": "Project", "projectId": "project-a"}),
        Some("root-a"),
    );
    let own_id = own.id.as_str().to_owned();
    let frame = |key: &str| {
        json!({"protocol": "1.0-draft.1", "requestKey": key, "sessionId": own_id,
            "expectedSessionRevision": "1", "content": [{"kind": "Text", "text": "progress"}]})
    };
    let f = refused(fx.raw_command(&agent, "sessions.append_message", frame("own-1")));
    assert_eq!(kind_of(&f), SESSION_PRINCIPAL);
    let bound = agent.clone().with_session(own.id.clone());
    applied(fx.raw_command(&bound, "sessions.append_message", frame("own-2")));
    applied(fx.command(
        &bound,
        "sessions.end",
        json!({"sessionId": own_id, "expectedRevision": "2"}),
    ));
    // The user may still message and end their session.
    applied(fx.raw_command(
        &fx.user(),
        "sessions.append_message",
        message("user-1", "1"),
    ));
}

#[test]
fn the_transcript_polls_from_a_tail_cursor() {
    let fx = Fx::new();
    let user = fx.user();
    let s = fx.work_session();
    let sid = s.id.as_str().to_owned();
    let send = |key: &str, rev: &str, text: &str| {
        applied(fx.raw_command(
            &user,
            "sessions.append_message",
            json!({"protocol": "1.0-draft.1", "requestKey": key, "sessionId": sid,
                "expectedSessionRevision": rev, "content": [{"kind": "Text", "text": text}]}),
        ));
    };
    send("p-1", "1", "one");
    let page = fx.transcript(&user, &sid, None);
    assert_eq!(page.entries.len(), 1);
    assert!(!page.has_more);
    // At the tail the page still carries its continuation: polling replays nothing.
    let tail = page.next_cursor.clone();
    assert!(fx.transcript(&user, &sid, Some(&tail)).entries.is_empty());
    send("p-2", "2", "two");
    let next = fx.transcript(&user, &sid, Some(&tail));
    assert_eq!(next.entries.len(), 1);
    assert_eq!(next.entries[0].sequence.get(), 2);
    // A cursor is bound to its reader.
    let other = fx.ctx("user-b", PrincipalKind::Human, None);
    let f = fault(fx.raw_query(
        &other,
        "sessions.transcript",
        json!({"protocol": "1.0-draft.1", "queryId": "q-t", "sessionId": sid, "after": tail,
            "limit": 5}),
    ));
    assert_eq!(f.code, DomainErrorCode::CursorExpired);
}

#[test]
fn native_items_reach_only_the_sessions_owner_and_every_view_is_audited() {
    let fx = Fx::new();
    fx.seed_session("session-a", "agent-a", Some("root-a"));
    let worker = fx.worker("agent-a", "session-a");
    let native = json!({"kind": "NativeItem", "binding": binding(), "artifact": art()});
    accepted(fx.append(
        &worker,
        SessionBatch::entries(tbatch(
            "n",
            "session-a",
            0,
            1,
            vec![entry("u", 1, text("hi")), entry("native", 2, native)],
        )),
    ));
    let audits = |fx: &Fx| {
        fx.read(|r| {
            Ok(r.history()
                .page(&EventFilter::All, 0, 500)?
                .events
                .into_iter()
                .filter(|e| e.kind == NATIVE_VIEWED)
                .map(|e| (e.actor, e.payload["sequences"].clone()))
                .collect::<Vec<_>>())
        })
    };
    // The user (not the session's creator or agent) sees the transcript without native items.
    let page = fx.transcript(&fx.user(), "session-a", None);
    assert_eq!(kinds(&page), ["UserMessage"]);
    assert!(!page.has_more);
    assert!(audits(&fx).is_empty());
    // The session's agent receives them, and the view is recorded.
    let page = fx.transcript(&worker, "session-a", None);
    assert_eq!(kinds(&page), ["UserMessage", "NativeItem"]);
    assert_eq!(audits(&fx), [(actor("agent-a"), json!([2]))]);
}

#[test]
fn operations_outside_the_sessions_project_are_unknown_to_it() {
    let fx = Fx::new();
    fx.seed_session("session-a", "agent-a", Some("root-a"));
    let worker = fx.worker("agent-a", "session-a");
    let update = tbatch(
        "u-x",
        "session-a",
        0,
        1,
        vec![entry(
            "u-x",
            1,
            json!({"kind": "OperationUpdate", "operationId": "op-x", "revision": "2", "artifact": art()}),
        )],
    );
    assert_eq!(
        refusal(fx.append(&worker, SessionBatch::entries(update))),
        BatchRefusal::UnknownOperation(OperationId(id("op-x")))
    );
    let waiting = tbatch(
        "w-x",
        "session-a",
        0,
        1,
        vec![
            entry(
                "c-a",
                1,
                json!({"kind": "ToolCallRecorded", "toolCall": call("call:a", 0)}),
            ),
            entry(
                "r-a",
                2,
                json!({"kind": "ToolResponseRecorded", "call": key("call:a", 0),
                    "outcome": {"kind": "OutcomeUnknown", "operationId": "op-x"}}),
            ),
        ],
    );
    assert_eq!(
        refusal(fx.append(&worker, SessionBatch::entries(waiting))),
        BatchRefusal::UnknownOperation(OperationId(id("op-x")))
    );
    // Even a row naming another project's operation never reaches the briefing.
    accepted(fx.append(
        &worker,
        SessionBatch::entries(calls_and_response("session-a")),
    ));
    fx.store
        .with_writer_connection(|c| {
            super::super::conn::db(c.execute(
                "UPDATE session_tool_calls SET operation_id='op-x' WHERE native_call_id='call:b'",
                [],
            ))
        })
        .unwrap();
    let brief = match result(fx.query(
        &fx.user(),
        "sessions.briefing",
        json!({"sessionId": "session-a", "expectedContextEpoch": "0"}),
    )) {
        OperationResult::SessionsBriefing { value } => value,
        other => panic!("{other:?}"),
    };
    assert!(brief.items.0.iter().all(|s| s.id != id("op-x")));
}

#[test]
fn an_epoch_boundary_waits_for_pending_tool_calls_and_has_its_own_identity() {
    let fx = Fx::new();
    fx.seed_session("session-a", "agent-a", Some("root-a"));
    let worker = fx.worker("agent-a", "session-a");
    accepted(fx.append(
        &worker,
        SessionBatch::entries(calls_and_response("session-a")),
    ));
    let boundary = json!({"kind": "ContextEpochChanged", "previousEpoch": "0", "nextEpoch": "1",
        "reason": "Compaction", "context": art()});
    let compact = |b: &str| {
        SessionBatch::entries(tbatch(
            b,
            "session-a",
            1,
            4,
            vec![entry(b, 4, boundary.clone())],
        ))
    };
    // call:a still has no response: compaction would cut an unresolved tool exchange.
    assert_eq!(
        refusal(fx.append(&worker, compact("e-1"))),
        BatchRefusal::UnresolvedToolCalls
    );
    accepted(fx.append(
        &worker,
        SessionBatch::entries(tbatch(
            "resp-a",
            "session-a",
            0,
            4,
            vec![entry(
                "r-a",
                4,
                json!({"kind": "ToolResponseRecorded", "call": key("call:a", 0),
                    "outcome": {"kind": "Failed", "code": "failed"}}),
            )],
        )),
    ));
    let compact = SessionBatch::entries(tbatch(
        "e-2",
        "session-a",
        1,
        5,
        vec![entry("e-2", 5, boundary)],
    ));
    accepted(fx.append(&worker, compact));
    let sid = id("session-a");
    let epoch = fx.read(|r| r.sessions().epoch(&sid, 1)).unwrap();
    let snapshot: devworks_core::protocol::provider::ArtifactRef =
        serde_json::from_value(art()).unwrap();
    assert_eq!(epoch.identity, epoch_identity(&sid, 1, &snapshot));
    assert_ne!(epoch.identity, epoch_identity(&sid, 2, &snapshot));
    assert_ne!(
        epoch.identity,
        epoch_identity(&id("session-b"), 1, &snapshot)
    );
}

#[test]
fn duplicate_entries_and_calls_are_refused_by_identity_and_by_ordinal() {
    let fx = Fx::new();
    fx.seed_session("session-a", "agent-a", Some("root-a"));
    let worker = fx.worker("agent-a", "session-a");
    accepted(fx.append(
        &worker,
        SessionBatch::entries(calls_and_response("session-a")),
    ));
    let at4 = |b: &str, e: &str, body: Value| {
        SessionBatch::entries(tbatch(b, "session-a", 0, 4, vec![entry(e, 4, body)]))
    };
    assert_eq!(
        refusal(fx.append(&worker, at4("d-entry", "c-a", text("again")))),
        BatchRefusal::DuplicateEntry(id("c-a"))
    );
    assert_eq!(
        refusal(fx.append(
            &worker,
            at4(
                "d-key",
                "c-a2",
                json!({"kind": "ToolCallRecorded", "toolCall": call("call:a", 7)})
            )
        )),
        BatchRefusal::DuplicateCall
    );
    assert_eq!(
        refusal(fx.append(
            &worker,
            at4(
                "d-ord",
                "c-z",
                json!({"kind": "ToolCallRecorded", "toolCall": call("call:z", 0)})
            )
        )),
        BatchRefusal::DuplicateCall
    );
}

#[test]
fn the_writer_enforces_project_scope_and_replays_after_close() {
    let fx = Fx::new();
    // A project-b agent recorded as the agent of a project-a session.
    fx.seed_session("session-a", "agent-b", Some("root-a"));
    let outsider = fx
        .ctx("agent-b", PrincipalKind::Agent, Some("project-b"))
        .with_session(id("session-a"));
    let batch = SessionBatch::entries(tbatch(
        "o-1",
        "session-a",
        0,
        1,
        vec![entry("o", 1, text("o"))],
    ));
    assert_eq!(
        refusal(fx.append(&outsider, batch)),
        BatchRefusal::Unauthorized("OutOfProjectScope")
    );
    fx.seed_session("session-c", "agent-a", Some("root-a"));
    let worker = fx.worker("agent-a", "session-c");
    let batch = SessionBatch::entries(tbatch(
        "c-1",
        "session-c",
        0,
        1,
        vec![entry("c", 1, text("c"))],
    ));
    let receipt = accepted(fx.append(&worker, batch.clone()));
    applied(fx.command(
        &fx.user(),
        "sessions.end",
        json!({"sessionId": "session-c", "expectedRevision": "1"}),
    ));
    // The closed session refuses new batches but its writer recovers the committed receipt.
    let late = SessionBatch::entries(tbatch(
        "c-2",
        "session-c",
        0,
        2,
        vec![entry("c2", 2, text("c"))],
    ));
    assert_eq!(
        refusal(fx.append(&worker, late)),
        BatchRefusal::SessionClosed
    );
    assert_eq!(fx.append(&worker, batch), BatchOutcome::Replayed(receipt));
}

#[test]
fn batch_identities_are_scoped_per_session_and_a_changed_payload_is_a_conflict() {
    let fx = Fx::new();
    fx.seed_session("session-a", "agent-a", Some("root-a"));
    fx.seed_session("session-c", "agent-a", Some("root-a"));
    for s in ["session-a", "session-c"] {
        let w = fx.worker("agent-a", s);
        accepted(fx.append(
            &w,
            SessionBatch::entries(tbatch("same", s, 0, 1, vec![entry("x", 1, text(s))])),
        ));
    }
    let f = append_refusal(&BatchRefusal::PayloadChanged);
    assert_eq!(
        (f.code, f.target_kind),
        (DomainErrorCode::Conflict, "transcript_batch")
    );
}

#[test]
fn git_links_name_repositories_of_the_works_project() {
    let fx = Fx::new();
    let user = fx.user();
    let ctx = user.clone();
    let foreign = fx.write(move |tx| {
        Ok(record_link(
            tx,
            &ctx,
            NewHistoryLink {
                root_id: root("root-a"),
                kind: LinkKind::Git,
                git: Some(GitFacts {
                    repository_id: id("repo-b"),
                    worktree_id: None,
                    branch: None,
                    pull_request_url: None,
                    head: serde_json::from_value(json!({"format": "Sha1", "hex": "c".repeat(40)}))
                        .unwrap(),
                }),
                artifact: None,
                target: None,
                summary: "Pushed".into(),
                supersedes: None,
            },
        )?
        .err())
    });
    let r = foreign.expect("refused");
    assert_eq!(
        (r.code, r.target_kind),
        (DomainErrorCode::NotFound, "repository")
    );
}

#[test]
fn workspace_scope_lists_home_sessions_and_home_is_shared_across_surfaces() {
    let fx = Fx::new();
    let home = fx.ctx("user-a", PrincipalKind::Human, None);
    let started = fx.start(&home, json!({"kind": "Workspace"}), None);
    fx.work_session();
    // The sidebar is another surface (another ingress namespace): it resolves the Home session
    // by scope, never by replaying the Home surface's request key.
    let info = fx.store.info();
    let sidebar = HostIngress::new(&*fx.store).context(
        actor("user-a"),
        PrincipalKind::Human,
        id("invocation-2"),
        id("ui-sidebar"),
        Counter::new(info.restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    );
    let open = match result(fx.query(
        &sidebar,
        "sessions.list",
        json!({"scope": {"kind": "Workspace"}, "page": {"limit": 10}, "closed": false}),
    )) {
        OperationResult::SessionsList { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(open.items.0.len(), 1, "workspace scope is Home only");
    assert_eq!(open.items.0[0].id, started.id);
    let sid = started.id.as_str().to_owned();
    applied(fx.raw_command(
        &sidebar,
        "sessions.append_message",
        json!({"protocol": "1.0-draft.1", "requestKey": "side-1", "sessionId": sid,
            "expectedSessionRevision": "1", "content": [{"kind": "Text", "text": "from the sidebar"}]}),
    ));
    assert_eq!(kinds(&fx.transcript(&home, &sid, None)), ["UserMessage"]);
    // The Home session is conducted by the workspace coordinator, whose runtime may append to
    // it; another agent may not.
    assert_eq!(fx.session(&sid).agent_id, actor("coordinator-a"));
    let coordinator = fx
        .ctx("coordinator-a", PrincipalKind::Agent, None)
        .with_session(started.id.clone());
    let reply = |b: &str| {
        SessionBatch::entries(tbatch(
            b,
            &sid,
            0,
            2,
            vec![entry(
                b,
                2,
                json!({"kind": "AssistantAttempt", "requestId": "request-a",
                "attemptId": "attempt-a", "visibleText": "Hello", "interrupted": false}),
            )],
        ))
    };
    let other = fx
        .ctx("agent-a", PrincipalKind::Agent, None)
        .with_session(started.id.clone());
    assert_eq!(
        refusal(fx.append(&other, reply("r-other"))),
        BatchRefusal::Unauthorized("NotSessionAgent")
    );
    accepted(fx.append(&coordinator, reply("r-1")));
}

#[test]
fn lineage_keys_are_stable_and_a_hierarchy_change_expires_the_cursor() {
    let fx = Fx::new();
    let user = fx.user();
    let scoped = user
        .clone()
        .with_lineage(Some(project("project-a")), Some(root("root-a")), None);
    let start = json!({"scope": {"kind": "Project", "projectId": "project-a"}, "title": "t"});
    fx.command(&scoped, "sessions.start", start.clone());
    fx.command(&scoped, "sessions.start", start.clone());
    // The first page holds the child task; the pager is past it.
    let first = lineage_page(fx.lineage(&user, "root-a", 1, None));
    assert_eq!(first.items.0[0].id, id("task-a"));
    let cursor = first.next_cursor.expect("more").0;
    // A command targeted at the child gives it history; its lineage position does not move,
    // so it is not seen again.
    fx.command(
        &user,
        "sessions.start",
        json!({"scope": {"kind": "Project", "projectId": "project-a"}, "rootId": "task-a",
            "title": "t"}),
    );
    let mut seen = vec![first.items.0[0].id.clone()];
    let mut next = Some(cursor);
    while let Some(c) = next {
        let page = lineage_page(fx.lineage(&user, "root-a", 1, Some(&c)));
        seen.extend(page.items.0.iter().map(|l| l.id.clone()));
        next = page.next_cursor.map(|c| c.0);
    }
    assert_eq!(
        seen.iter().filter(|i| **i == id("task-a")).count(),
        1,
        "{seen:?}"
    );
    assert_eq!(seen.len(), 3, "{seen:?}");
    // A child created while paging changes the hierarchy: the cursor expires visibly rather
    // than skipping it, and a fresh listing shows it once.
    let page = lineage_page(fx.lineage(&user, "root-a", 1, None));
    let cursor = page.next_cursor.expect("more").0;
    fx.store
        .with_writer_connection(|c| {
            super::super::conn::db(c.execute_batch(
                "INSERT INTO work_items VALUES('task-late','project-a','task','root-a',NULL,NULL,'Late',1,0,1,1,1,'{}');",
            ))
        })
        .unwrap();
    assert_eq!(
        fault(fx.lineage(&user, "root-a", 1, Some(&cursor))).code,
        DomainErrorCode::CursorExpired
    );
    let all = lineage_page(fx.lineage(&user, "root-a", 50, None));
    let ids: Vec<&str> = all.items.0.iter().map(|l| l.id.as_str()).collect();
    assert_eq!(
        ids.iter().filter(|i| **i == "task-late").count(),
        1,
        "{ids:?}"
    );
    // Summaries carry the attribution the record has no fields for.
    let command = all.items.0.iter().find(|l| l.kind.0 == "command").unwrap();
    assert!(
        command.summary.0.contains("by user-a at "),
        "{}",
        command.summary.0
    );
}
#[test]
fn retained_endpoints_behave_through_their_mapped_operations() {
    let fx = Fx::new();
    let user = fx.user();
    // start_session / WorkController.StartSession, end_session / EndSession.
    let agent = fx
        .ctx("agent-p", PrincipalKind::Agent, Some("project-a"))
        .with_lineage(Some(project("project-a")), Some(root("root-a")), None);
    let s = fx.start(
        &agent,
        json!({"kind": "Project", "projectId": "project-a"}),
        Some("root-a"),
    );
    // get_briefing / WorkController.Briefing.
    let brief = match result(fx.query(
        &agent,
        "sessions.briefing",
        json!({"sessionId": s.id.as_str(), "expectedContextEpoch": "0"}),
    )) {
        OperationResult::SessionsBriefing { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(brief.items.0[0].id, id("root-a"));
    // WorkViewsController.Sessions (project sessions) and AskController chats.
    let listed = match result(fx.query(
        &user,
        "sessions.list",
        json!({"scope": {"kind": "Project", "projectId": "project-a"}, "page": {"limit": 10}}),
    )) {
        OperationResult::SessionsList { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(listed.items.0[0].id, s.id);
    // WorkController.Changes / WorkViewsController.ItemChanges / the changes tool: the item's
    // decisions.
    let changes = match result(fx.events(&user, "root-a", None)) {
        OperationResult::HistoryEvents { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(changes.items.0.len(), 1);
    let bound = agent.clone().with_session(s.id.clone());
    applied(fx.command(
        &bound,
        "sessions.end",
        json!({"sessionId": s.id.as_str(), "expectedRevision": "1"}),
    ));
    // WorkViewsController.GitHubEvidence: Git/PR links of the item.
    let git = match result(fx.query(
        &user,
        "history.git_links",
        json!({"rootId": "root-a", "page": {"limit": 10}}),
    )) {
        OperationResult::HistoryGitLinks { value } => value,
        other => panic!("{other:?}"),
    };
    assert!(git.items.0.is_empty());
}

// ---------------------------------------------------------------------------------------------
// Schema revision 2: tail checkpoints, command aggregates, the project feed, attribution, tool
// invocations and briefing inputs

/// A domain query frame at `revision` (`None`: no `schemaRevision` member, i.e. revision 1).
fn at_revision(op: &str, mut body: Value, revision: Option<u32>) -> Value {
    body["kind"] = json!(op);
    let mut frame =
        json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": "q-r", "body": body});
    if let Some(r) = revision {
        frame["schemaRevision"] = json!(r);
    }
    frame
}
/// A `history.events` frame over `scope` (`{"rootId": ..}` or `{"projectId": ..}`).
fn events_frame(scope: Value, after: Option<&str>, limit: u32, revision: Option<u32>) -> Value {
    let mut q = json!({"kind": "history.events", "limit": limit});
    for (k, v) in scope.as_object().unwrap() {
        q[k] = v.clone();
    }
    if let Some(a) = after {
        q["after"] = json!(a);
    }
    let mut frame = json!({"protocol": "1.0-draft.1", "queryId": "q-e", "query": q});
    if let Some(r) = revision {
        frame["schemaRevision"] = json!(r);
    }
    frame
}
fn events_page(reply: DomainQueryReply) -> HistoryPage<Event> {
    match result(reply) {
        OperationResult::HistoryEvents { value } => value,
        other => panic!("{other:?}"),
    }
}
fn wire(r: &OperationResult) -> String {
    serde_json::to_string(r).unwrap()
}
/// The revision-1 projection of a revision-2 result.
fn projected(mut r: OperationResult) -> String {
    r.retain_revision(1);
    wire(&r)
}

impl Fx {
    fn at(
        &self,
        ctx: &InvocationContext,
        op: &str,
        body: Value,
        r: Option<u32>,
    ) -> DomainQueryReply {
        self.raw_query(ctx, op, at_revision(op, body, r))
    }
    fn feed(
        &self,
        ctx: &InvocationContext,
        scope: Value,
        after: Option<&str>,
        limit: u32,
        r: Option<u32>,
    ) -> DomainQueryReply {
        self.raw_query(ctx, "history.events", events_frame(scope, after, limit, r))
    }
    /// One decision attached to `root-a` only by lineage (no target), one targeting `task-a`,
    /// one project-b decision and one project-a decision without a root.
    fn decisions(&self) {
        let lineage =
            self.user()
                .with_lineage(Some(project("project-a")), Some(root("root-a")), None);
        self.command(
            &lineage,
            "sessions.start",
            json!({"scope": {"kind": "Project", "projectId": "project-a"}, "title": "t"}),
        );
        self.command(
            &self.user(),
            "sessions.start",
            json!({"scope": {"kind": "Project", "projectId": "project-a"}, "rootId": "task-a",
                "title": "t"}),
        );
        let b = self
            .user()
            .with_lineage(Some(project("project-b")), None, None);
        self.command(
            &b,
            "sessions.start",
            json!({"scope": {"kind": "Project", "projectId": "project-b"}, "title": "t"}),
        );
        let a = self
            .user()
            .with_lineage(Some(project("project-a")), None, None);
        self.command(
            &a,
            "sessions.start",
            json!({"scope": {"kind": "Project", "projectId": "project-a"}, "title": "t"}),
        );
    }
}

#[test]
fn revision_two_history_pages_carry_a_tail_checkpoint_that_polls_new_facts_only() {
    let fx = Fx::new();
    let user = fx.user();
    let agent = fx
        .ctx("agent-p", PrincipalKind::Agent, Some("project-a"))
        .with_lineage(Some(project("project-a")), Some(root("root-a")), None);
    for _ in 0..2 {
        fx.start(
            &agent,
            json!({"kind": "Project", "projectId": "project-a"}),
            Some("root-a"),
        );
    }
    // At the tail a revision-2 page still carries a continuation; revision 1 does not.
    let r1 = events_page(fx.feed(&user, json!({"rootId": "root-a"}), None, 50, None));
    assert!(!r1.has_more && r1.next_cursor.is_none() && r1.checkpoint.is_none());
    let tail = events_page(fx.feed(&user, json!({"rootId": "root-a"}), None, 50, Some(2)));
    assert_eq!(tail.items.0.len(), 2);
    assert!(!tail.has_more && tail.next_cursor.is_none());
    let checkpoint = tail.checkpoint.clone().unwrap().0;
    // Polling the checkpoint replays nothing, and keeps its position while nothing is new.
    let quiet = events_page(fx.feed(
        &user,
        json!({"rootId": "root-a"}),
        Some(&checkpoint),
        50,
        Some(2),
    ));
    assert!(quiet.items.0.is_empty());
    assert_eq!(quiet.checkpoint.as_ref().unwrap().0, checkpoint);
    fx.start(
        &agent,
        json!({"kind": "Project", "projectId": "project-a"}),
        Some("root-a"),
    );
    let new = events_page(fx.feed(
        &user,
        json!({"rootId": "root-a"}),
        Some(&checkpoint),
        50,
        Some(2),
    ));
    assert_eq!(new.items.0.len(), 1);
    assert!(new.items.0[0].sequence.get() > tail.items.0[1].sequence.get());
    // While there is more, the checkpoint is the next cursor.
    let first = events_page(fx.feed(&user, json!({"rootId": "root-a"}), None, 1, Some(2)));
    assert!(first.has_more);
    assert_eq!(first.checkpoint.unwrap().0, first.next_cursor.unwrap().0);
    // A revision-1 cursor does not continue a revision-2 list (it covers other facts).
    let r1_first = events_page(fx.feed(&user, json!({"rootId": "root-a"}), None, 1, None));
    let c1 = r1_first.next_cursor.unwrap().0;
    assert_eq!(
        fault(fx.feed(&user, json!({"rootId": "root-a"}), Some(&c1), 1, Some(2))).code,
        DomainErrorCode::CursorExpired
    );
    // Lineage, links and usage pages carry it too; revision 1 never does.
    for op in [
        "history.work_lineage",
        "history.git_links",
        "history.evidence_links",
        "history.tool_usage",
    ] {
        let body = json!({"rootId": "root-a", "page": {"limit": 50}});
        let r2 = serde_json::to_value(result(fx.at(&user, op, body.clone(), Some(2)))).unwrap();
        assert!(r2["value"]["checkpoint"].is_string(), "{op}: {r2}");
        let r1 = serde_json::to_value(result(fx.at(&user, op, body, None))).unwrap();
        assert!(r1["value"].get("checkpoint").is_none(), "{op}: {r1}");
        assert_eq!(r1["value"]["hasMore"], false);
    }
    // An empty history still has a checkpoint: the start, which then sees the first fact.
    let empty = events_page(fx.feed(&user, json!({"rootId": "root-b"}), None, 50, Some(2)));
    assert!(empty.items.0.is_empty());
    let start = empty.checkpoint.unwrap().0;
    let b = fx
        .user()
        .with_lineage(Some(project("project-b")), None, None);
    fx.command(
        &b,
        "sessions.start",
        json!({"scope": {"kind": "Project", "projectId": "project-b"}, "rootId": "root-b",
            "title": "t"}),
    );
    let first_b = events_page(fx.feed(
        &user,
        json!({"rootId": "root-b"}),
        Some(&start),
        50,
        Some(2),
    ));
    assert_eq!(first_b.items.0.len(), 1);
}

#[test]
fn revision_two_publishes_lineage_only_decisions_with_a_command_aggregate() {
    let fx = Fx::new();
    fx.decisions();
    let user = fx.user();
    // Revision 1: only decisions the recorded Subject can type (none on root-a).
    let r1 = result(fx.feed(&user, json!({"rootId": "root-a"}), None, 50, None));
    let OperationResult::HistoryEvents { value } = &r1 else {
        panic!()
    };
    assert!(value.items.0.is_empty());
    assert!(!wire(&r1).contains("\"command\"") && !wire(&r1).contains("checkpoint"));
    // Revision 2: the lineage-only decision, with the command as its aggregate.
    let r2 = events_page(fx.feed(&user, json!({"rootId": "root-a"}), None, 50, Some(2)));
    assert_eq!(r2.items.0.len(), 1);
    let e = &r2.items.0[0];
    let command = e.command_id.clone().unwrap();
    assert!(matches!(&e.aggregate, EventSubject::Command(c) if *c == command));
    assert!(matches!(
        e.aggregate_revision_kind,
        EventRevisionKind::Command
    ));
    assert_eq!(e.aggregate_revision.get(), 1);
    e.validate().unwrap();
    assert!(e.validate_revision(1).is_err() && e.validate_revision(2).is_ok());
    // A decision with a real subject keeps it at revision 2, identical to revision 1.
    let t1 = result(fx.feed(&user, json!({"rootId": "task-a"}), None, 50, None));
    let t2 = result(fx.feed(&user, json!({"rootId": "task-a"}), None, 50, Some(2)));
    assert_eq!(projected(t2), wire(&t1));
    let OperationResult::HistoryEvents { value } = t1 else {
        panic!()
    };
    assert!(matches!(&value.items.0[0].aggregate, EventSubject::Root(r) if *r == root("task-a")));
    // An unregistered revision is refused before the store is read.
    assert_eq!(
        fault(fx.feed(&user, json!({"rootId": "root-a"}), None, 50, Some(3))).code,
        DomainErrorCode::SchemaMismatch
    );
}

#[test]
fn the_project_feed_serves_every_decision_of_a_visible_project_in_commit_order() {
    let fx = Fx::new();
    fx.decisions();
    let user = fx.user();
    let feed = events_page(fx.feed(&user, json!({"projectId": "project-a"}), None, 50, Some(2)));
    // Every project-a decision: lineage-only on root-a, targeted at task-a, project-only.
    assert_eq!(feed.items.0.len(), 3);
    assert!(
        feed.items
            .0
            .iter()
            .all(|e| e.project_id == Some(project("project-a")))
    );
    let seqs: Vec<i64> = feed.items.0.iter().map(|e| e.sequence.get()).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
    assert!(
        feed.items
            .0
            .iter()
            .any(|e| e.root_id.is_none() && matches!(e.aggregate, EventSubject::Command(_)))
    );
    assert!(
        feed.items
            .0
            .iter()
            .any(|e| matches!(&e.aggregate, EventSubject::Root(r) if *r == root("task-a")))
    );
    let OperationResult::HistoryEvents { .. } =
        result(fx.feed(&user, json!({"projectId": "project-b"}), None, 50, Some(2)))
    else {
        panic!()
    };
    // Paging in ones covers the same facts; each cursor is bound to its project feed.
    let one = events_page(fx.feed(&user, json!({"projectId": "project-a"}), None, 1, Some(2)));
    let c = one.next_cursor.unwrap().0;
    let two = events_page(fx.feed(
        &user,
        json!({"projectId": "project-a"}),
        Some(&c),
        1,
        Some(2),
    ));
    assert_eq!(two.items.0[0].event_id, feed.items.0[1].event_id);
    for (who, scope) in [
        (fx.user(), json!({"projectId": "project-b"})),
        (fx.user(), json!({"rootId": "root-a"})),
        (
            fx.ctx("user-b", PrincipalKind::Human, None),
            json!({"projectId": "project-a"}),
        ),
    ] {
        assert_eq!(
            fault(fx.feed(&who, scope, Some(&c), 1, Some(2))).code,
            DomainErrorCode::CursorExpired
        );
    }
    // A change to who can see the project expires its cursors.
    fx.store
        .with_writer_connection(|c| {
            super::super::conn::db(c.execute(
                "UPDATE projects SET scope_revision=2 WHERE id='project-a'",
                [],
            ))
        })
        .unwrap();
    assert_eq!(
        fault(fx.feed(
            &user,
            json!({"projectId": "project-a"}),
            Some(&c),
            1,
            Some(2)
        ))
        .code,
        DomainErrorCode::CursorExpired
    );
    // Visibility: a project-b agent does not learn project-a exists; nor does anyone an
    // unknown project. A project-a agent reads its own project's feed.
    let outsider = fx.ctx("agent-b", PrincipalKind::Agent, Some("project-b"));
    for (who, p) in [(&outsider, "project-a"), (&user, "project-gone")] {
        let f = fault(fx.feed(who, json!({"projectId": p}), None, 50, Some(2)));
        assert_eq!(
            (f.code, kind_of(&f)),
            (DomainErrorCode::NotFound, "project")
        );
    }
    let insider = fx.ctx("agent-p", PrincipalKind::Agent, Some("project-a"));
    assert_eq!(
        events_page(fx.feed(
            &insider,
            json!({"projectId": "project-a"}),
            None,
            50,
            Some(2)
        ))
        .items
        .0
        .len(),
        3
    );
    // Revision 1 has no project feed, and a feed names exactly one scope.
    for (scope, r) in [
        (json!({"projectId": "project-a"}), None),
        (json!({"projectId": "project-a"}), Some(1)),
        (
            json!({"projectId": "project-a", "rootId": "root-a"}),
            Some(2),
        ),
        (json!({}), Some(2)),
    ] {
        assert_eq!(
            fault(fx.feed(&user, scope, None, 50, r)).code,
            DomainErrorCode::InvalidArgument
        );
    }
}

#[test]
fn revision_two_lineage_evidence_and_git_records_carry_structured_attribution() {
    let fx = Fx::new();
    let user = fx.user();
    let ctx = user.clone();
    fx.write(move |tx| {
        record_link(
            tx,
            &ctx,
            NewHistoryLink {
                root_id: root("root-a"),
                kind: LinkKind::Git,
                git: Some(GitFacts {
                    repository_id: id("repo-a"),
                    worktree_id: None,
                    branch: Some("main".into()),
                    pull_request_url: None,
                    head: serde_json::from_value(json!({"format": "Sha1", "hex": "c".repeat(40)}))
                        .unwrap(),
                }),
                artifact: None,
                target: None,
                summary: "Pushed".into(),
                supersedes: None,
            },
        )?
        .map_err(|r| StorageError::InvalidRecord(format!("{r:?}")))?;
        record_link(
            tx,
            &ctx,
            NewHistoryLink {
                root_id: root("root-a"),
                kind: LinkKind::Evidence,
                git: None,
                artifact: Some(serde_json::from_value(art()).unwrap()),
                target: None,
                summary: "Report".into(),
                supersedes: None,
            },
        )?
        .map_err(|r| StorageError::InvalidRecord(format!("{r:?}")))
    });
    let body = json!({"rootId": "root-a", "page": {"limit": 50}});
    let lineage = fx.at(&user, "history.work_lineage", body.clone(), Some(2));
    let OperationResult::HistoryWorkLineage { value } = result(lineage) else {
        panic!()
    };
    let work = &value.items.0[0];
    assert_eq!(work.kind.0, "task");
    // Work items record no creator; the time is the creation.
    assert!(work.actor_id.is_none() && work.repository_id.is_none() && work.revision.is_none());
    assert_eq!(work.occurred_at_ms.as_ref().unwrap().get(), 1);
    let git = value.items.0.iter().find(|l| l.kind.0 == "git").unwrap();
    assert_eq!(git.actor_id, Some(actor("user-a")));
    assert_eq!(git.repository_id, Some(id("repo-a")));
    assert_eq!(git.revision.as_ref().unwrap().0, "c".repeat(40));
    assert_eq!(git.occurred_at_ms.as_ref().unwrap().get(), T0);
    let evidence = value
        .items
        .0
        .iter()
        .find(|l| l.kind.0 == "evidence")
        .unwrap();
    assert_eq!(evidence.actor_id, Some(actor("user-a")));
    assert!(evidence.repository_id.is_none() && evidence.revision.is_none());
    for op in ["history.git_links", "history.evidence_links"] {
        let r2 = serde_json::to_value(result(fx.at(&user, op, body.clone(), Some(2)))).unwrap();
        let item = &r2["value"]["items"][0];
        assert_eq!(item["actorId"], "user-a", "{op}");
        assert_eq!(item["occurredAtMs"], T0.to_string(), "{op}");
    }
    // Revision 1 is byte-identical to the revision-2 page projected onto revision 1, and its
    // summaries keep the attribution text.
    for op in [
        "history.work_lineage",
        "history.git_links",
        "history.evidence_links",
    ] {
        let r1 = result(fx.at(&user, op, body.clone(), None));
        let r2 = result(fx.at(&user, op, body.clone(), Some(2)));
        assert_eq!(projected(r2), wire(&r1), "{op}");
        for member in ["actorId", "occurredAtMs", "\"revision\"", "checkpoint"] {
            assert!(!wire(&r1).contains(member), "{op} revision 1 has {member}");
        }
        // Git links always named their repository; lineage and evidence gain it at revision 2.
        assert_eq!(
            wire(&r1).contains("repositoryId"),
            op == "history.git_links",
            "{op}"
        );
    }
    assert!(git.summary.0.contains("by user-a"));
    for op in ["history.work_lineage", "history.git_links"] {
        assert_eq!(
            fault(fx.at(&user, op, body.clone(), Some(3))).code,
            DomainErrorCode::SchemaMismatch
        );
    }
}

#[test]
fn revision_two_tool_usage_serves_redacted_tool_invocations_in_commit_order() {
    let fx = Fx::new();
    fx.seed_session("session-a", "agent-a", Some("root-a"));
    let worker = fx.worker("agent-a", "session-a");
    let mut first = SessionBatch::entries(calls_and_response("session-a"));
    first.usage = vec![usage("request-a")];
    accepted(fx.append(&worker, first));
    // A later batch answers call:a with a failure whose code is credential-shaped.
    let secret = format!("ghp_{}", "A1b2C3d4E5".repeat(4));
    StoreHarness::set_now(&fx.h, T0 + 250);
    accepted(fx.append(
        &worker,
        SessionBatch::entries(tbatch(
            "batch-failure",
            "session-a",
            0,
            4,
            vec![entry(
                "r-a",
                4,
                json!({"kind": "ToolResponseRecorded", "call": key("call:a", 0),
                    "outcome": {"kind": "Failed", "code": secret}}),
            )],
        )),
    ));
    let user = fx.user();
    let body = json!({"rootId": "root-a", "page": {"limit": 50}});
    let r2 = result(fx.at(&user, "history.tool_usage", body.clone(), Some(2)));
    let OperationResult::HistoryToolUsage { value } = &r2 else {
        panic!()
    };
    assert_eq!(value.items.0.len(), 1);
    let tools = &value.tool_invocations.as_ref().unwrap().0;
    assert_eq!(tools.len(), 2);
    let (wait, failed) = (&tools[0], &tools[1]);
    assert_eq!(
        (wait.tool_name.0.as_str(), wait.call_id.as_str()),
        ("core/read", "c-b")
    );
    assert!(matches!(
        wait.outcome,
        ToolInvocationOutcome::ApprovalRequired
    ));
    assert_eq!(wait.operation_id, Some(OperationId(id("op-b"))));
    assert_eq!(wait.duration_ms.as_ref().unwrap().get(), 0);
    assert_eq!(wait.session_id, id("session-a"));
    assert_eq!(wait.actor_id, actor("agent-a"));
    assert!(matches!(failed.outcome, ToolInvocationOutcome::Failed));
    assert_eq!(failed.call_id, id("c-a"));
    assert_eq!(failed.duration_ms.as_ref().unwrap().get(), 250);
    // The credential-shaped error code never leaves the store; arguments are never included.
    let code = failed.error_code.as_ref().unwrap().0.clone();
    assert!(
        !code.contains(&secret) && code.contains("REDACTED"),
        "{code}"
    );
    let published = wire(&r2);
    assert!(!published.contains(&secret) && !published.contains("arguments"));
    // Revision 1: the inference usage alone, byte-identical to its recorded shape.
    let r1 = result(fx.at(&user, "history.tool_usage", body.clone(), None));
    assert!(!wire(&r1).contains("toolInvocations") && !wire(&r1).contains("checkpoint"));
    let OperationResult::HistoryToolUsage { value: v1 } = &r1 else {
        panic!()
    };
    assert_eq!(v1.items.0.len(), 1);
    assert_eq!(projected(r2.clone()), {
        let mut only = r2.clone();
        if let OperationResult::HistoryToolUsage { value } = &mut only {
            value.tool_invocations = None;
            value.checkpoint = None;
        }
        wire(&only)
    });
    // In pages of one, the facts arrive in committed-sequence order, each exactly once.
    let mut cursor: Option<String> = None;
    let mut order = Vec::new();
    loop {
        let mut page = json!({"limit": 1});
        if let Some(c) = &cursor {
            page["cursor"] = json!(c);
        }
        let OperationResult::HistoryToolUsage { value } = result(fx.at(
            &user,
            "history.tool_usage",
            json!({"rootId": "root-a", "page": page}),
            Some(2),
        )) else {
            panic!()
        };
        order.extend(
            value
                .items
                .0
                .iter()
                .map(|u| u.request_id.as_str().to_owned()),
        );
        order.extend(
            value
                .tool_invocations
                .unwrap()
                .0
                .iter()
                .map(|t| t.call_id.as_str().to_owned()),
        );
        match value.next_cursor {
            Some(c) => cursor = Some(c.0),
            None => break,
        }
    }
    assert_eq!(order, ["c-b", "request-a", "c-a"]);
    assert_eq!(
        fault(fx.at(&user, "history.tool_usage", body, Some(3))).code,
        DomainErrorCode::SchemaMismatch
    );
}

#[test]
fn revision_two_briefings_take_an_item_paths_and_a_token_budget() {
    let fx = Fx::new();
    fx.seed_session("session-a", "agent-a", Some("root-a"));
    let worker = fx.worker("agent-a", "session-a");
    let mut batch = SessionBatch::entries(calls_and_response("session-a"));
    batch.checkpoint = Some(serde_json::from_value(art()).unwrap());
    batch.context_sources = (0..6)
        .map(|i| ContextSourceInput {
            source_id: id(&format!("source-{i}")),
            kind: "retrieved".into(),
            source_revision: 1,
            artifact: None,
            token_estimate: 10,
            disposition: SourceDisposition::Included,
        })
        .collect();
    accepted(fx.append(&worker, batch));
    let user = fx.user();
    let base = json!({"sessionId": "session-a", "expectedContextEpoch": "0"});
    let brief = |extra: Value, r: Option<u32>| {
        let mut body = base.clone();
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        fx.at(&user, "sessions.briefing", body, r)
    };
    let page = |reply| match result(reply) {
        OperationResult::SessionsBriefing { value } => value,
        other => panic!("{other:?}"),
    };
    // Revision 1 and a plain revision-2 briefing are the same briefing.
    let plain = result(brief(json!({}), None));
    assert_eq!(wire(&result(brief(json!({}), Some(2)))), wire(&plain));
    let OperationResult::SessionsBriefing { value: full } = &plain else {
        panic!()
    };
    // work:feature (3), checkpoint (1), pending call, operation and six sources of 10.
    let total: i64 = full.items.0.iter().map(|s| s.token_estimate.get()).sum();
    assert_eq!(full.items.0.len(), 10);
    assert_eq!(total, 64);
    // The focus item follows the session's own work item.
    let focused = page(brief(
        json!({"item": "task-a", "paths": ["src/lib.rs"]}),
        Some(2),
    ));
    assert_eq!(focused.items.0[1].kind.0, "focus:task");
    assert_eq!(focused.items.0[1].id, id("task-a"));
    assert_eq!(focused.items.0.len(), 11);
    // Naming the session's own work adds nothing; work outside its project is unknown.
    assert_eq!(
        page(brief(json!({"item": "root-a"}), Some(2)))
            .items
            .0
            .len(),
        10
    );
    for item in ["root-b", "root-gone"] {
        let f = fault(brief(json!({"item": item}), Some(2)));
        assert_eq!((f.code, kind_of(&f)), (DomainErrorCode::NotFound, "root"));
    }
    // The budget keeps the longest prefix that fits, continuing the recorded sources after it.
    let capped = page(brief(json!({"budgetTokens": 30}), Some(2)));
    let spent: i64 = capped.items.0.iter().map(|s| s.token_estimate.get()).sum();
    assert!(spent <= 30, "{spent}");
    assert_eq!(capped.items.0.len(), 6);
    assert_eq!(spent, 24);
    let rest = match result(fx.query(
        &user,
        "sessions.context_sources",
        json!({"sessionId": "session-a", "expectedContextEpoch": "0",
            "page": {"limit": 50, "cursor": capped.next_cursor.unwrap().0}}),
    )) {
        OperationResult::SessionsContextSources { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(rest.items.0.len(), 4);
    // A budget smaller than the first source leaves the briefing empty, still continuable.
    let none = page(brief(json!({"budgetTokens": 1}), Some(2)));
    assert!(none.items.0.is_empty() && none.has_more);
    // A budget the briefing fits in changes nothing.
    assert_eq!(
        wire(&OperationResult::SessionsBriefing {
            value: page(brief(json!({"budgetTokens": 64}), Some(2)))
        }),
        wire(&plain)
    );
    // Revision 1 cannot send the new members; the bounds hold at revision 2.
    for (extra, r) in [
        (json!({"budgetTokens": 30}), None),
        (json!({"item": "task-a"}), Some(1)),
        (json!({"paths": ["src"]}), None),
        (json!({"budgetTokens": 0}), Some(2)),
        (json!({"budgetTokens": 1000001}), Some(2)),
        (json!({"paths": []}), Some(2)),
        (json!({"paths": ["/etc/passwd"]}), Some(2)),
        (json!({"paths": ["a/../b"]}), Some(2)),
        (json!({"paths": ["src", "src"]}), Some(2)),
    ] {
        assert_eq!(
            fault(brief(extra.clone(), r)).code,
            DomainErrorCode::InvalidArgument,
            "{extra} at {r:?}"
        );
    }
    assert_eq!(
        fault(brief(json!({}), Some(3))).code,
        DomainErrorCode::SchemaMismatch
    );
}
