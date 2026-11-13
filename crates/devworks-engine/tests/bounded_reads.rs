//! A read or admission request costs store work that follows what it asks for, not total
//! history: each test measures one request, grows unrelated history tenfold and measures again.
//! The store counts its own work (read snapshots and SQLite virtual-machine steps).
mod common;

use common::*;
use devworks_core::authority::HostAuthority;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::gateway::Gateway;
use devworks_core::ports::retrieval::NoVectorIndex;
use devworks_core::ports::storage::*;
use devworks_core::protocol::results::{
    DomainCommandReply, DomainDecision, DomainQueryReply, OperationResult,
};
use devworks_core::protocol::{ActorId, Counter, Id, Revision};
use devworks_core::services::UseCases;
use devworks_core::services::knowledge::KnowledgeService;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

struct App<'h> {
    h: &'h Harness,
    counted: CountingStore,
    store: Arc<dyn ApplicationStore>,
    cases: UseCases,
    n: AtomicU64,
}
impl<'h> App<'h> {
    fn boot(h: &'h Harness, cases: impl FnOnce(UseCases) -> UseCases) -> Self {
        let counted = h.open_counted();
        let store = counted.store.clone();
        let registered = ActorId(id("user-a"));
        write(&*store, move |tx| {
            tx.actors()
                .register(&registered, PrincipalKind::Human, "user-a")
        })
        .unwrap();
        Self {
            h,
            counted,
            store,
            cases: cases(UseCases::new()),
            n: AtomicU64::new(0),
        }
    }
    fn user(&self) -> InvocationContext {
        let info = self.store.info();
        HostIngress::new(&*self.store).context(
            ActorId(id("user-a")),
            PrincipalKind::Human,
            id("invocation-1"),
            id("ui-main"),
            Counter::new(info.restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
    }
    fn authority(&self) -> HostAuthority<'_> {
        HostAuthority {
            store: &*self.store,
            clock: &*self.h.clock,
            protected_branches: &[],
        }
    }
    fn cmd(&self, op: &str, revision: u32, mut body: Value) -> OperationResult {
        body["kind"] = json!(op);
        let key = format!("key-{}", self.n.fetch_add(1, Ordering::SeqCst));
        let frame = json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": key,
            "schemaRevision": revision, "body": body});
        let authority = self.authority();
        let gateway = Gateway {
            authority: &authority,
            store: &*self.store,
            ids: &*self.h.ids,
        };
        match self
            .cases
            .command(&gateway, &self.user(), op, frame.to_string().as_bytes())
            .expect("addressable")
        {
            DomainCommandReply::Recorded { receipt, .. } => match receipt.decision {
                DomainDecision::Applied { result } => result,
                other => panic!("expected applied, got {other:?}"),
            },
            other => panic!("expected a recorded decision, got {other:?}"),
        }
    }
    fn q(&self, op: &str, revision: u32, mut body: Value) -> OperationResult {
        body["kind"] = json!(op);
        let frame = json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": "q-1",
            "schemaRevision": revision, "body": body});
        let authority = self.authority();
        let gateway = Gateway {
            authority: &authority,
            store: &*self.store,
            ids: &*self.h.ids,
        };
        match self
            .cases
            .query(&gateway, &self.user(), op, frame.to_string().as_bytes())
            .expect("addressable")
        {
            DomainQueryReply::Result { result, .. } => result,
            other => panic!("{other:?}"),
        }
    }
    /// The store work of one query.
    fn measure(&self, op: &str, revision: u32, body: Value) -> StoreWork {
        self.counted.count(|| self.q(op, revision, body)).1
    }
}

fn session(store: &dyn ApplicationStore, name: &str) -> Id {
    let s = StoredSession {
        id: id(name),
        conversation_id: id(&format!("conversation-{name}")),
        agent_id: ActorId(id("host")),
        project_id: None,
        root_id: None,
        title: name.into(),
        context_epoch: 0,
        revision: 1,
        created_by: ActorId(id("host")),
        created_at_ms: T0,
        closed_at_ms: None,
        closed_by: None,
        next_sequence: 1,
        checkpoint_sequence: 0,
    };
    let out = s.id.clone();
    write(store, move |tx| tx.sessions().insert_session(&s)).unwrap();
    out
}

/// Knowledge `claim_wire`: reading one lesson costs the same however many recurrence events
/// and session deliveries it has, and however many other claims exist.
#[test]
fn a_knowledge_read_costs_the_same_behind_tenfold_history() {
    let h = Harness::new();
    let knowledge = Arc::new(KnowledgeService::new(
        h.ids.clone(),
        h.clock(),
        None,
        Arc::new(NoVectorIndex),
    ));
    let app = App::boot(&h, |cases| cases.with_knowledge(knowledge));
    let lesson = json!({"kind": "Lesson", "subjectId": "s", "predicate": "lesson",
        "value": "Run cargo fmt before committing", "confidence": 0.8, "evidenceIds": [],
        "tags": [], "lesson": {"source": "Prompt", "applicability": {"activities": [],
        "paths": [], "tools": [], "repositoryIds": []}}});
    let claim = match app.cmd(
        "knowledge.record",
        2,
        json!({"scope": {"kind": "Workspace"}, "batch": {"entities": [{"clientId": "s",
            "label": "Habits", "entityType": "topic", "aliases": []}], "evidence": [],
            "claims": [lesson]}}),
    ) {
        OperationResult::KnowledgeRecord { value } => value.claim_ids.0[0].clone(),
        other => panic!("{other:?}"),
    };
    // The reply lists a claim's challenges (bounded), so their number stays fixed; the open
    // count must still not be counted from them.
    let challenged = claim.clone();
    write(&*app.store, move |tx| {
        for (n, state) in ["Retired", "Upheld", "Open"].into_iter().enumerate() {
            tx.knowledge().put_knowledge_challenge(&StoredChallenge {
                id: id(&format!("challenge-{n}")),
                claim_id: challenged.clone(),
                state: state.into(),
                reason: "no longer applies".into(),
                evidence_ids: vec![],
                author: ActorId(id("user-a")),
                opened_at_ms: T0,
                resolved_at_ms: (state != "Open").then_some(T0),
                resolved_by: (state != "Open").then(|| ActorId(id("user-a"))),
                resolution: (state != "Open").then(|| "settled".into()),
            })?;
        }
        Ok(())
    })
    .unwrap();
    let mut grown = 0;
    let mut grow = |app: &App<'_>, to: usize| {
        for n in grown..to {
            let claim_id = claim.clone();
            let session = session(&*app.store, &format!("delivered-{n}"));
            write(&*app.store, move |tx| {
                let k = tx.knowledge();
                k.add_knowledge_lesson_event(&StoredLessonEvent {
                    claim_id: claim_id.clone(),
                    event_key: format!("Prompt|event-{n}"),
                    source: "Prompt".into(),
                    source_ref: Some(format!("event-{n}")),
                    session_id: None,
                    actor: ActorId(id("user-a")),
                    at_ms: T0,
                    write_id: id(&format!("write-{n}")),
                    merge_path: vec![],
                })?;
                k.record_knowledge_delivery(&session, &claim_id, 1, T0 + n as i64)
                    .map(|_| ())
            })
            .unwrap();
            app.cmd(
                "knowledge.record",
                2,
                json!({"scope": {"kind": "Workspace"}, "batch": {"entities": [{"clientId":
                    "t", "label": format!("Other {n}"), "entityType": "topic", "aliases": []}],
                    "evidence": [], "claims": [{"kind": "Fact", "subjectId": "t",
                    "predicate": "states", "value": format!("other fact {n}"),
                    "confidence": 0.7, "evidenceIds": [], "tags": []}]}}),
            );
        }
        grown = to;
    };
    let read = |app: &App<'_>| {
        app.measure(
            "knowledge.read",
            2,
            json!({"scope": {"kind": "Workspace"}, "claimId": claim}),
        )
    };
    grow(&app, 10);
    let small = read(&app);
    grow(&app, 100);
    let large = read(&app);
    assert_no_growth("knowledge read", small, large);
    // The maintained tallies match the history they summarize.
    let tally = common::read(&*app.store, |s| s.knowledge().knowledge_claim_tally(&claim));
    assert_eq!(
        (tally.lesson_events, tally.open_challenges, tally.deliveries.sessions),
        (101, 1, 100)
    );
}

/// MCP: reading one server's capabilities reads that server's catalog only. Three other
/// servers' catalogs grow from 10 to 100 tools; the read costs the same steps and pages.
#[test]
fn reading_one_mcp_server_costs_the_same_behind_tenfold_other_catalogs() {
    use devworks_core::ports::mcp::{RemoteCatalog, RemoteTool};
    use devworks_core::services::mcp::McpClientService;
    let h = Harness::new();
    let service = Arc::new(McpClientService::new(h.ids.clone()));
    let app = App::boot(&h, |cases| cases.with_mcp(service.clone()));
    let configure = |name: &str| match app.cmd(
        "mcp.configure",
        8,
        json!({"scope": {"kind": "Workspace"}, "name": name,
            "transport": {"kind": "Http", "endpoint": "https://mcp.example.test/mcp"}}),
    ) {
        OperationResult::McpConfigure { value } => value.id,
        other => panic!("{other:?}"),
    };
    let publish = |server: &Id, tools: usize, expected: i64| {
        let (service, server) = (service.clone(), server.clone());
        write(&*app.store, move |tx| {
            service.publish_remote_catalog(
                tx,
                &ConfigScope::Workspace,
                &server,
                &Revision::new(expected).unwrap(),
                RemoteCatalog {
                    tools: (0..tools)
                        .map(|n| RemoteTool {
                            name: format!("tool_{n}"),
                            description: "Look up one record by its key. ".repeat(8),
                            input_schema: json!({"type": "object",
                                "properties": {"key": {"type": "string",
                                    "description": "The record key. ".repeat(8)}}}),
                            output_schema: None,
                            read_only_hint: true,
                            title: None,
                            destructive_hint: None,
                        })
                        .collect(),
                    resources: vec![],
                    prompts: vec![],
                },
            )
        })
        .unwrap();
    };
    let read = configure("Read server");
    publish(&read, 5, 1);
    let others = ["Other one", "Other two", "Other three"].map(configure);
    for other in &others {
        publish(other, 10, 1);
    }
    let capabilities = |app: &App<'_>| {
        app.counted
            .count(|| {
                app.q(
                    "mcp.capabilities",
                    8,
                    json!({"scope": {"kind": "Workspace"}, "serverId": read,
                        "catalogRevision": "2", "page": {"limit": 10}}),
                )
            })
            .1
    };
    let small = capabilities(&app);
    for other in &others {
        publish(other, 100, 2);
    }
    let large = capabilities(&app);
    assert_no_growth("mcp capabilities", small, large);
    assert!(
        large.read_pages <= small.read_pages + small.read_pages / 4,
        "mcp capabilities read other servers' catalogs: {small:?} -> {large:?}"
    );
}
