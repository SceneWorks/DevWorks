//! Receipt reply artifacts and workspace-scoped artifacts end to end on the real SQLite store and
//! real managed files: a command's receipt commits its publication intent, the publisher writes
//! the immutable reply artifact after commit, and `history.command_receipt` then returns a
//! rules-valid `Recorded` observation whose artifact decodes to the recorded (redacted) reply.
//! Idempotent re-publication, a crash before publication and between the file and its metadata,
//! an in-boot publication failure, workspace visibility and denial, the deletion rules and the
//! pending state. Deterministic temporary stores and an injected clock; no inference.
use super::{Harness, T0};
use crate::fs::NativeFilesystem;
use crate::fs::artifacts::{NativeArtifactFiles, Step};
use crate::storage::sqlite::{MIGRATION_0008_SHA256, MIGRATIONS, Migration, SqliteStore};
use devworks_core::authority::HostAuthority;
use devworks_core::authority::service as auth;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::gateway::Gateway;
use devworks_core::ports::files::{
    ArtifactFiles, ArtifactSources, FileFault, PublishedBlob, ResolvedSource, SourceRefusal,
};
use devworks_core::ports::storage::*;
use devworks_core::protocol::data::{ArtifactSource, DataScope};
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::{
    ArtifactReadContent, ReceiptLookupState, ReceiptObservation,
};
use devworks_core::protocol::results::{
    DomainCommandReply, DomainDecision, DomainQueryReply, OperationResult,
};
use devworks_core::protocol::{
    ActorId, Check, CommandReply, Counter, DomainErrorCode, DomainFault, Id, ProjectId,
    RetryAdvice, Revision,
};
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::lifecycle::VersionResolution;
use devworks_core::services::artifacts::replies::{
    REPLY_AGGREGATE, REPLY_CONSUMER, REPLY_MEDIA_TYPE, ReceiptReplies, ReplyOutcome,
};
use devworks_core::services::artifacts::{
    ArtifactService, RECEIPT_RETAINED, SHARED_REFERENCES_REMAIN, versions_digest,
};
use devworks_core::services::history::{
    HistoryService, RECEIPT_REPLY_ABANDONED, RECEIPT_REPLY_HELD, RECEIPT_REPLY_UNAVAILABLE,
};
use devworks_core::services::sessions::SessionService;
use devworks_core::settings::catalog::AgentRole;
use serde_json::{Value, json};
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn actor(s: &str) -> ActorId {
    ActorId(id(s))
}
fn project(s: &str) -> ProjectId {
    ProjectId(id(s))
}

/// No host source handles: these tests publish only receipt replies.
struct NoSources;
impl ArtifactSources for NoSources {
    fn resolve(
        &self,
        _: &InvocationContext,
        _: &ArtifactSource,
        _: &ProjectId,
    ) -> Result<ResolvedSource, SourceRefusal> {
        Err(SourceRefusal::NotFound)
    }
}

/// The native adapter, with a publication that can be made to fail once (a full volume).
struct Flaky {
    inner: Arc<NativeArtifactFiles>,
    fail_publish: AtomicBool,
}
impl ArtifactFiles for Flaky {
    fn publish(
        &self,
        s: &str,
        src: &mut dyn Read,
        d: &str,
        n: u64,
        r: Option<&str>,
        g: &str,
    ) -> Result<PublishedBlob, FileFault> {
        if self.fail_publish.swap(false, Ordering::SeqCst) {
            return Err(FileFault::DiskFull);
        }
        self.inner.publish(s, src, d, n, r, g)
    }
    fn managed_path(&self, d: &str, g: &str) -> Result<String, FileFault> {
        self.inner.managed_path(d, g)
    }
    fn present(&self, d: &str, g: &str, n: u64) -> Result<bool, FileFault> {
        self.inner.present(d, g, n)
    }
    fn inspect(&self, d: &str, g: &str) -> Result<Option<u64>, FileFault> {
        self.inner.inspect(d, g)
    }
    fn read_range(&self, d: &str, g: &str, n: u64, o: u64, m: usize) -> Result<Vec<u8>, FileFault> {
        self.inner.read_range(d, g, n, o, m)
    }
    fn open(&self, d: &str, g: &str, n: u64) -> Result<Box<dyn Read + Send>, FileFault> {
        self.inner.open(d, g, n)
    }
    fn unlink(&self, d: &str, g: &str) -> Result<(), FileFault> {
        self.inner.unlink(d, g)
    }
    fn quarantine(&self, d: &str, g: &str) -> Result<(), FileFault> {
        self.inner.quarantine(d, g)
    }
    fn staged(&self) -> Result<Vec<String>, FileFault> {
        self.inner.staged()
    }
    fn remove_staged(&self, n: &str) -> Result<(), FileFault> {
        self.inner.remove_staged(n)
    }
    fn stored_files(&self) -> Result<Vec<(String, String)>, FileFault> {
        self.inner.stored_files()
    }
    fn write_derived(&self, k: &str, b: &[u8]) -> Result<(), FileFault> {
        self.inner.write_derived(k, b)
    }
    fn read_derived(&self, k: &str, m: usize) -> Result<Option<Vec<u8>>, FileFault> {
        self.inner.read_derived(k, m)
    }
    fn collect_derived(&self) -> Result<u64, FileFault> {
        self.inner.collect_derived()
    }
    fn export(&self, d: &str, g: &str, n: u64, dest: &Path, o: bool) -> Result<(), FileFault> {
        self.inner.export(d, g, n, dest, o)
    }
}

const ACTORS: [(&str, PrincipalKind); 8] = [
    ("user-a", PrincipalKind::Human),
    ("user-b", PrincipalKind::Human),
    ("coordinator-w", PrincipalKind::Agent),
    ("agent-a", PrincipalKind::Agent),
    ("agent-p", PrincipalKind::Agent),
    ("agent-q", PrincipalKind::Agent),
    ("agent-b", PrincipalKind::Agent),
    ("package-a", PrincipalKind::Package),
];

struct Fx {
    h: Harness,
    store: Arc<SqliteStore>,
    files: Arc<NativeArtifactFiles>,
    flaky: Arc<Flaky>,
    svc: Arc<ArtifactService>,
    replies: ReceiptReplies,
    cases: UseCases,
    n: Arc<AtomicU64>,
}

impl Fx {
    fn new() -> Self {
        let fx = Self::open(Harness::new(), Arc::new(AtomicU64::new(0)));
        fx.store
            .with_writer_connection(|c| {
                super::super::conn::db(c.execute_batch(
                    "INSERT INTO projects VALUES('project-a','A','Active',1,1,1);
                     INSERT INTO projects VALUES('project-b','B','Active',1,1,1);
                     INSERT INTO work_items VALUES('root-a','project-a','feature',NULL,NULL,NULL,'Feature A',1,0,1,1,1,'{}');",
                ))
            })
            .unwrap();
        fx.write(|tx| {
            for (a, k) in ACTORS {
                tx.actors().register(&actor(a), k, a)?;
            }
            Ok(())
        });
        let user = fx.user();
        for (who, kind, role, project) in [
            (
                "coordinator-w",
                PrincipalKind::Agent,
                Some(AgentRole::WorkspaceCoordinator),
                None,
            ),
            (
                "agent-a",
                PrincipalKind::Agent,
                Some(AgentRole::Worker),
                None,
            ),
            (
                "agent-p",
                PrincipalKind::Agent,
                Some(AgentRole::Worker),
                Some("project-a"),
            ),
            (
                "agent-q",
                PrincipalKind::Agent,
                Some(AgentRole::Worker),
                Some("project-a"),
            ),
            (
                "agent-b",
                PrincipalKind::Agent,
                Some(AgentRole::Worker),
                Some("project-b"),
            ),
            ("package-a", PrincipalKind::Package, None, None),
        ] {
            let by = user.clone();
            let binding = NewPrincipalBinding {
                actor: actor(who),
                kind,
                role,
                project_id: project.map(self::project),
                tool_families: None,
                bound_by: actor("user-a"),
            };
            fx.write(move |tx| {
                auth::bind_principal(tx, &by, binding, None)
                    .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
            });
        }
        fx
    }

    fn open(h: Harness, n: Arc<AtomicU64>) -> Self {
        let store = Arc::new(h.store());
        let files = Arc::new(NativeArtifactFiles::new(
            h.root.0.clone(),
            h.root.0.with_extension("cache"),
        ));
        let flaky = Arc::new(Flaky {
            inner: files.clone(),
            fail_publish: AtomicBool::new(false),
        });
        let svc = Arc::new(ArtifactService::new(
            flaky.clone(),
            Arc::new(NoSources),
            Arc::new(NativeFilesystem::new()),
            h.clock.clone(),
            h.ids.clone(),
        ));
        let cases = UseCases::new()
            .with_artifacts(svc.clone())
            .with_sessions(Arc::new(SessionService::new(h.ids.clone(), store.clone())))
            .with_history(Arc::new(HistoryService::new(h.ids.clone())));
        Self {
            h,
            store,
            files,
            flaky,
            replies: ReceiptReplies::new(svc.clone()),
            svc,
            cases,
            n,
        }
    }

    /// A process restart: every handle on the store is dropped, then it is reopened.
    fn restart(self) -> Self {
        let Fx {
            h,
            store,
            files,
            flaky,
            svc,
            replies,
            cases,
            n,
        } = self;
        drop((cases, replies, svc, flaky, files, store));
        Self::open(h, n)
    }

    fn authority(&self) -> HostAuthority<'_> {
        HostAuthority {
            store: &*self.store,
            clock: &*self.h.clock,
            protected_branches: &[],
        }
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
    fn agent(&self, who: &str, project: Option<&str>) -> InvocationContext {
        self.ctx(who, PrincipalKind::Agent, project)
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
    fn gateway_command(
        &self,
        ctx: &InvocationContext,
        op: &str,
        frame: Value,
    ) -> DomainCommandReply {
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
        self.gateway_command(
            ctx,
            op,
            json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": key, "body": body}),
        )
    }
    fn query(&self, ctx: &InvocationContext, op: &str, frame: Value) -> DomainQueryReply {
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
    /// A Home (workspace) session start under `key`.
    fn start_home(&self, ctx: &InvocationContext, key: &str, title: &str) -> DomainCommandReply {
        self.command_keyed(
            ctx,
            "sessions.start",
            key,
            json!({"scope": {"kind": "Workspace"}, "title": title}),
        )
    }
    /// A project-a session start under `key`.
    fn start_project(&self, ctx: &InvocationContext, key: &str) -> DomainCommandReply {
        self.command_keyed(
            ctx,
            "sessions.start",
            key,
            json!({"scope": {"kind": "Project", "projectId": "project-a"}, "title": "Work chat"}),
        )
    }
    fn receipt(&self, ctx: &InvocationContext, key: &str) -> DomainQueryReply {
        self.query(
            ctx,
            "history.command_receipt",
            json!({"protocol": "1.0-draft.1", "queryId": "q-r",
                "query": {"kind": "history.command_receipt", "requestKey": key}}),
        )
    }
    fn artifact_query(&self, ctx: &InvocationContext, op: &str, query: Value) -> DomainQueryReply {
        self.query(
            ctx,
            op,
            json!({"protocol": "1.0-draft.1", "queryId": "q-1", "query": query, "limit": 50}),
        )
    }
    fn read_reply(&self, ctx: &InvocationContext, r: &ArtifactRef) -> DomainQueryReply {
        self.artifact_query(
            ctx,
            "artifacts.read",
            json!({"kind": "Read", "artifactId": r.artifact_id, "versionId": r.version_id}),
        )
    }
    fn delete(&self, ctx: &InvocationContext, deletion: Value) -> DomainCommandReply {
        self.gateway_command(
            ctx,
            "artifacts.delete",
            json!({"protocol": "1.0-draft.1", "requestKey": self.key(), "deletion": deletion}),
        )
    }
    /// Claims every ready reply delivery as one publisher.
    fn claim(&self) -> Vec<QueueEntry> {
        self.write(|tx| {
            tx.queue().claim_ready(&ClaimRequest {
                consumer: REPLY_CONSUMER.into(),
                owner: Id::new("publisher-1").unwrap(),
                lease_ms: 60_000,
                limit: 100,
            })
        })
    }
    fn publish_all(&self) -> Vec<ReplyOutcome> {
        self.claim()
            .iter()
            .map(|e| self.replies.publish(&*self.store, e))
            .collect()
    }
    fn reply_deliveries(&self) -> QueueStatus {
        self.read(|s| s.queue().status(REPLY_CONSUMER))
    }
    fn count(&self, sql: &'static str) -> i64 {
        self.store
            .with_writer_connection(move |c| {
                super::super::conn::db(c.query_row(sql, [], |r| r.get(0)))
            })
            .unwrap()
    }
}

fn recorded(reply: &DomainCommandReply) -> &devworks_core::protocol::Receipt<DomainDecision> {
    match reply {
        CommandReply::Recorded { receipt, .. } => receipt,
        other => panic!("expected a recorded reply, got {other:?}"),
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
fn refused(reply: DomainCommandReply) -> DomainFault {
    match &recorded(&reply).decision {
        DomainDecision::Refused { fault } => fault.clone(),
        other => panic!("expected a refusal, got {other:?}"),
    }
}
fn kind_of(f: &DomainFault) -> &str {
    f.target_kind.as_ref().map_or("", |k| k.0.as_str())
}
fn observation(reply: DomainQueryReply) -> ReceiptObservation {
    match result(reply) {
        OperationResult::HistoryCommandReceipt { value } => value,
        other => panic!("{other:?}"),
    }
}
fn published(outcomes: Vec<ReplyOutcome>) -> Vec<ArtifactRef> {
    outcomes
        .into_iter()
        .map(|o| match o {
            ReplyOutcome::Published(r) => r,
            other => panic!("expected a publication, got {other:?}"),
        })
        .collect()
}
fn text(reply: DomainQueryReply) -> String {
    match result(reply) {
        OperationResult::ArtifactsRead {
            value: ArtifactReadContent::Text { content, .. },
        } => content.0,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_recorded_receipt_publishes_its_reply_and_then_observes_recorded() {
    let fx = Fx::new();
    let user = fx.user();
    let reply = fx.start_home(&user, "start-1", "Home");
    let receipt = recorded(&reply).clone();
    // The intent committed with the receipt; until it is published the view is pending, never
    // StatusUnknown (the receipt is committed) and never a Recorded state without its reply.
    assert_eq!(fx.reply_deliveries().ready, 1);
    let f = fault(fx.receipt(&user, "start-1"));
    assert_eq!(
        (f.code, kind_of(&f), f.target_id.clone(), f.retry),
        (
            DomainErrorCode::ViewUnavailable,
            RECEIPT_REPLY_UNAVAILABLE,
            Some(receipt.command_id.0.clone()),
            RetryAdvice::SameCommand
        )
    );

    let refs = published(fx.publish_all());
    assert_eq!(refs.len(), 1);
    let r = refs[0].clone();
    assert_eq!(r.media_type, REPLY_MEDIA_TYPE);
    let status = fx.reply_deliveries();
    assert_eq!((status.ready, status.claimed, status.completed), (0, 0, 1));

    let o = observation(fx.receipt(&user, "start-1"));
    o.check().expect("rules-valid observation");
    assert!(matches!(o.state, ReceiptLookupState::Recorded));
    assert_eq!(o.command_id.as_ref(), Some(&receipt.command_id));
    assert_eq!(
        o.committed_sequence.as_ref(),
        Some(&receipt.committed_sequence)
    );
    assert_eq!(o.operation_id, receipt.operation_id);
    let artifact = o.reply_artifact.clone().unwrap();
    assert_eq!(
        serde_json::to_value(&artifact).unwrap(),
        serde_json::to_value(&r).unwrap()
    );
    let schema = o.reply_schema.clone().unwrap();
    let descriptor = devworks_core::registry::find("sessions.start").unwrap();
    assert_eq!(schema.schema_id.as_str(), "devworks.reply/sessions.start");
    assert_eq!(
        schema.revision.get(),
        i64::from(descriptor.schema_revision.0)
    );

    // The artifact bytes are the canonical recorded reply and decode to it.
    let bytes = text(fx.read_reply(&user, &artifact));
    assert_eq!(
        sha256_hex(bytes.as_bytes()),
        String::from(artifact.sha256.clone())
    );
    let decoded: DomainCommandReply = serde_json::from_str(&bytes).unwrap();
    let mut expected = serde_json::to_value(&reply).unwrap();
    expected["replayed"] = json!(false);
    assert_eq!(serde_json::to_value(&decoded).unwrap(), expected);

    // A projectless receipt's reply is a workspace artifact owned by the receipt's principal,
    // retained with the receipt.
    let stored = fx
        .read(|s| s.artifacts().artifact(&artifact.artifact_id))
        .unwrap();
    assert_eq!(stored.scope, ArtifactScope::Workspace);
    assert_eq!(stored.origin_project_id, None);
    assert_eq!(stored.owner, Some(actor("user-a")));
    assert_eq!(stored.retention, ArtifactRetention::Receipt);
    let usage = fx.read(|s| {
        fx.svc
            .storage_status(s, &ArtifactScope::Workspace)
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    });
    // System-owned bytes are reported apart from the limit's usage.
    assert_eq!(
        (usage.used_bytes, usage.receipt_reply_bytes),
        (0, artifact.byte_length.get())
    );
    // Its publication is history of the receipt's own aggregate, attributed to the host.
    let events = fx.read(|s| {
        s.history().page(
            &EventFilter::Aggregate {
                kind: REPLY_AGGREGATE.into(),
                id: receipt.command_id.0.as_str().to_owned(),
            },
            0,
            10,
        )
    });
    assert_eq!(events.events.len(), 1);
    assert_eq!(events.events[0].kind, "ReceiptReplyPublished");
    assert_eq!(events.events[0].actor, actor("host"));
    assert_eq!(events.events[0].project_id, None);

    // A replayed command neither re-publishes nor changes the reply.
    let again = fx.start_home(&user, "start-1", "Home");
    assert!(matches!(
        again,
        CommandReply::Recorded { replayed: true, .. }
    ));
    assert!(fx.publish_all().is_empty());
    let o2 = observation(fx.receipt(&user, "start-1"));
    assert_eq!(
        serde_json::to_value(&o2).unwrap(),
        serde_json::to_value(&o).unwrap()
    );
}

#[test]
fn a_project_receipt_reply_is_project_owned_and_seen_only_by_its_principal() {
    let fx = Fx::new();
    let agent = fx.agent("agent-p", Some("project-a"));
    let reply = fx.start_project(&agent, "p-1");
    recorded(&reply);
    let r = published(fx.publish_all()).remove(0);
    let stored = fx.read(|s| s.artifacts().artifact(&r.artifact_id)).unwrap();
    assert_eq!(stored.scope, ArtifactScope::Project(project("project-a")));
    assert_eq!(stored.owner, Some(actor("agent-p")));
    let o = observation(fx.receipt(&agent, "p-1"));
    o.check().unwrap();
    // The receipt's principal, the user and the workspace coordinator read it.
    text(fx.read_reply(&agent, &r));
    text(fx.read_reply(&fx.user(), &r));
    text(fx.read_reply(&fx.agent("coordinator-w", None), &r));
    // Another agent of the same project, another project's agent and an unconfined worker do
    // not: existence is not disclosed.
    for other in [
        fx.agent("agent-q", Some("project-a")),
        fx.agent("agent-b", Some("project-b")),
        fx.agent("agent-a", None),
    ] {
        assert_eq!(
            fault(fx.read_reply(&other, &r)).code,
            DomainErrorCode::NotFound
        );
    }
    // Receipt replies are not project work outputs: the project listing omits them.
    match result(fx.artifact_query(
        &fx.user(),
        "artifacts.list",
        json!({"kind": "List", "projectId": "project-a"}),
    )) {
        OperationResult::ArtifactsList { value } => assert!(value.items.0.is_empty()),
        other => panic!("{other:?}"),
    }
    // System-owned reply bytes are not the project's work output: its limit measures none of
    // them, and they are reported separately.
    let status = fx.read(|s| {
        fx.svc
            .storage_status(s, &ArtifactScope::Project(project("project-a")))
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    });
    assert_eq!(
        (status.used_bytes, status.receipt_reply_bytes),
        (0, r.byte_length.get())
    );
    // Its publication is not in the project's history, where every project reader would see
    // another principal's receipt.
    let project_events = fx.read(|s| s.history().page(&EventFilter::All, 0, 500));
    assert!(
        !project_events.events.iter().any(|e| {
            e.project_id == Some(project("project-a"))
                && (e.aggregate_kind == "artifact" || e.aggregate_kind == REPLY_AGGREGATE)
        }),
        "{:?}",
        project_events.events
    );
}

#[test]
fn a_receipt_reply_is_observed_from_any_context_its_principal_may_act_in() {
    let fx = Fx::new();
    // The user records from the workspace and looks it up from inside a project.
    recorded(&fx.start_home(&fx.user(), "home-1", "Home"));
    // An unconfined agent records in project-a and looks it up from project-b and the workspace.
    recorded(&fx.start_project(&fx.agent("agent-a", Some("project-a")), "proj-1"));
    let refs = published(fx.publish_all());
    assert_eq!(refs.len(), 2);
    let user_in_project = fx.ctx("user-a", PrincipalKind::Human, Some("project-a"));
    observation(fx.receipt(&user_in_project, "home-1"))
        .check()
        .unwrap();
    for ctx in [
        fx.agent("agent-a", Some("project-b")),
        fx.agent("agent-a", None),
    ] {
        let o = observation(fx.receipt(&ctx, "proj-1"));
        o.check().unwrap();
        text(fx.read_reply(&ctx, &o.reply_artifact.unwrap()));
    }
}

#[test]
fn only_an_active_unconfined_workspace_coordinator_joins_the_audience() {
    let fx = Fx::new();
    let user = fx.user();
    fx.write(|tx| {
        tx.actors().register(
            &actor("coordinator-p"),
            PrincipalKind::Agent,
            "coordinator-p",
        )
    });
    let by = user.clone();
    fx.write(move |tx| {
        auth::bind_principal(
            tx,
            &by,
            NewPrincipalBinding {
                actor: actor("coordinator-p"),
                kind: PrincipalKind::Agent,
                role: Some(AgentRole::WorkspaceCoordinator),
                project_id: Some(project("project-a")),
                tool_families: None,
                bound_by: actor("user-a"),
            },
            None,
        )
        .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    });
    recorded(&fx.start_project(&fx.agent("agent-p", Some("project-a")), "p-1"));
    let r = published(fx.publish_all()).remove(0);
    let resolves = |ctx: &InvocationContext| {
        matches!(
            fx.read(|s| fx.svc.resolve_version(s, ctx, &r)),
            VersionResolution::Available(_)
        )
    };
    assert!(resolves(&fx.agent("coordinator-w", None)));
    // A coordinator confined to a project is not the workspace coordinator.
    assert!(!resolves(&fx.agent("coordinator-p", Some("project-a"))));
    // Nor is a revoked one.
    let by = user.clone();
    fx.write(move |tx| {
        auth::revoke_principal(tx, &by, &actor("coordinator-w"), 1)
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    });
    assert!(!resolves(&fx.agent("coordinator-w", None)));
}

#[test]
fn a_receipt_stored_unredacted_is_redacted_when_its_reply_is_rendered() {
    let fx = Fx::new();
    let user = fx.user();
    recorded(&fx.start_home(&user, "old-1", "Home"));
    // As an earlier build might have stored it: a secret in the recorded decision.
    let changed = fx
        .store
        .with_writer_connection(|c| {
            super::super::conn::db(c.execute(
                "UPDATE command_receipts SET result_json=replace(result_json,'\"Home\"','\"Home password=hunter2-plain\"')",
                [],
            ))
        })
        .unwrap();
    assert_eq!(changed, 1);
    let r = published(fx.publish_all()).remove(0);
    let bytes = text(fx.read_reply(&user, &r));
    assert!(!bytes.contains("hunter2"), "{bytes}");
    assert!(bytes.contains("[REDACTED]"), "{bytes}");
}

#[test]
fn an_untypable_receipt_is_held_and_its_observation_says_so() {
    let fx = Fx::new();
    let user = fx.user();
    recorded(&fx.start_home(&user, "rev-1", "Home"));
    recorded(&fx.start_home(&user, "op-1", "Home"));
    // Receipts this build cannot type: another schema revision, an operation it does not
    // register.
    fx.store
        .with_writer_connection(|c| {
            super::super::conn::db(c.execute_batch(
                "UPDATE command_receipts SET result_json=json_set(result_json,'$.schemaRevision',99) WHERE idempotency_key='rev-1';
                 UPDATE command_receipts SET command_kind='retired.operation' WHERE idempotency_key='op-1';",
            ))
        })
        .unwrap();
    let mut held = Vec::new();
    for e in fx.claim() {
        let ReplyOutcome::Hold(cause) = fx.replies.publish(&*fx.store, &e) else {
            panic!("expected a hold")
        };
        let (id, fence) = (e.id.clone(), e.fence().unwrap());
        fx.write(move |tx| tx.queue().hold(&id, &fence, &cause));
        held.push(e.id);
    }
    assert_eq!(held.len(), 2);
    assert_eq!(fx.count("SELECT count(*) FROM artifacts"), 0);
    let state = |key: &str| {
        let f = fault(fx.receipt(&user, key));
        (f.code, kind_of(&f).to_owned(), f.retry, f.target_id)
    };
    for key in ["rev-1", "op-1"] {
        let (code, kind, retry, target) = state(key);
        assert_eq!(
            (code, kind.as_str(), retry),
            (
                DomainErrorCode::ViewUnavailable,
                RECEIPT_REPLY_HELD,
                RetryAdvice::AfterBlockerClears
            )
        );
        assert!(held.contains(&target.unwrap()));
    }
    // Resolved by an operator, it is pending again; failed terminally it is held again; dismissed
    // it is abandoned and no retry is advised.
    let id = held[0].clone();
    let key = fx
        .read(|s| s.queue().entry(&id))
        .map(|e| e.payload["requestKey"].as_str().unwrap().to_owned())
        .unwrap();
    let i = id.clone();
    fx.write(move |tx| tx.queue().requeue(&i, 0));
    let (_, kind, retry, _) = state(&key);
    assert_eq!(
        (kind.as_str(), retry),
        (RECEIPT_REPLY_UNAVAILABLE, RetryAdvice::SameCommand)
    );
    let e = fx.claim().into_iter().find(|e| e.id == id).unwrap();
    let fence = e.fence().unwrap();
    let i = id.clone();
    fx.write(move |tx| {
        tx.queue()
            .fail(&i, &fence, "untypable", FailureDisposition::Terminal)
    });
    assert_eq!(state(&key).1, RECEIPT_REPLY_HELD);
    let i = id.clone();
    fx.write(move |tx| tx.queue().dismiss(&i, "operator decision"));
    let (code, kind, retry, _) = state(&key);
    assert_eq!(
        (code, kind.as_str(), retry),
        (
            DomainErrorCode::ViewUnavailable,
            RECEIPT_REPLY_ABANDONED,
            RetryAdvice::Never
        )
    );
}

#[test]
fn a_failed_rebuild_migration_leaves_nothing_and_restores_foreign_keys() {
    let h = Harness::new();
    let before: Vec<Migration> = MIGRATIONS
        .iter()
        .filter(|m| m.version <= 7)
        .copied()
        .collect();
    drop(SqliteStore::open_with(h.config(), &before).unwrap());
    // A rebuild that would leave a dangling reference, and one whose SQL fails part way.
    for sql in [
        "CREATE TABLE fixture_rebuild(id TEXT PRIMARY KEY) STRICT; INSERT INTO artifact_versions VALUES('dangling','no-artifact',1,'no-blob','text/plain',1,NULL,'{}');",
        "CREATE TABLE fixture_rebuild(id TEXT PRIMARY KEY) STRICT; INSERT INTO no_such_table VALUES(1);",
    ] {
        let mut migrations = before.clone();
        migrations.push(Migration {
            version: 8,
            minimum_reader: 8,
            minimum_writer: 8,
            sql,
            rebuilds_tables: true,
        });
        let conn = h.raw();
        let failed = super::super::migrations::prepare(
            &conn,
            &migrations,
            || "unused".to_owned(),
            T0,
            "test",
        );
        assert!(failed.is_err(), "{sql}");
        let (fk, tables, version): (i64, i64, i64) = conn
            .query_row(
                "SELECT (SELECT foreign_keys FROM pragma_foreign_keys),
                 (SELECT count(*) FROM sqlite_schema WHERE name='fixture_rebuild'),
                 (SELECT schema_version FROM store_meta)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((fk, tables, version), (1, 0, 7), "{sql}");
    }
    // The real migration still applies afterwards.
    drop(SqliteStore::open(h.config()).unwrap());
}

#[test]
fn workspace_receipt_replies_are_visible_to_their_audience_and_denied_to_others() {
    let fx = Fx::new();
    let user = fx.user();
    recorded(&fx.start_home(&user, "home-1", "Home"));
    let r = published(fx.publish_all()).remove(0);
    for audience in [
        fx.user(),
        fx.ctx("user-b", PrincipalKind::Human, None),
        fx.agent("coordinator-w", None),
    ] {
        text(fx.read_reply(&audience, &r));
        result(fx.artifact_query(
            &audience,
            "artifacts.preview",
            json!({"kind": "Preview", "artifactId": r.artifact_id, "versionId": r.version_id}),
        ));
        result(fx.artifact_query(
            &audience,
            "artifacts.export",
            json!({"kind": "Export", "artifactId": r.artifact_id, "versionId": r.version_id}),
        ));
    }
    for outsider in [
        fx.agent("agent-a", None),
        fx.agent("agent-p", Some("project-a")),
        fx.ctx("package-a", PrincipalKind::Package, None),
    ] {
        for (op, kind) in [
            ("artifacts.read", "Read"),
            ("artifacts.preview", "Preview"),
            ("artifacts.export", "Export"),
        ] {
            let f = fault(fx.artifact_query(
                &outsider,
                op,
                json!({"kind": kind, "artifactId": r.artifact_id, "versionId": r.version_id}),
            ));
            // Agents get NotFound from the artifact scope check (existence is not disclosed); a
            // package may already be refused by its authority policy.
            let expected = if outsider.principal_kind() == PrincipalKind::Package {
                matches!(
                    f.code,
                    DomainErrorCode::NotFound | DomainErrorCode::PermissionDenied
                )
            } else {
                f.code == DomainErrorCode::NotFound
            };
            assert!(expected, "{op}: {f:?}");
        }
    }
    // Another principal's lookup of the same key is its own (absent) receipt.
    let other = fx.ctx("user-b", PrincipalKind::Human, None);
    assert!(matches!(
        observation(fx.receipt(&other, "home-1")).state,
        ReceiptLookupState::NotFound
    ));
}

#[test]
fn a_caller_that_can_no_longer_see_its_reply_is_denied() {
    let fx = Fx::new();
    let agent = fx.agent("agent-p", Some("project-a"));
    recorded(&fx.start_project(&agent, "p-1"));
    published(fx.publish_all());
    // The same principal under another project scope (its binding moved to project-b): access
    // is rechecked before the old receipt is published.
    let by = fx.user();
    fx.write(move |tx| {
        auth::bind_principal(
            tx,
            &by,
            NewPrincipalBinding {
                actor: actor("agent-p"),
                kind: PrincipalKind::Agent,
                role: Some(AgentRole::Worker),
                project_id: Some(project("project-b")),
                tool_families: None,
                bound_by: actor("user-a"),
            },
            Some(1),
        )
        .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    });
    let moved = fx.agent("agent-p", Some("project-b"));
    let f = fault(fx.receipt(&moved, "p-1"));
    assert_eq!(
        (f.code, kind_of(&f)),
        (DomainErrorCode::PermissionDenied, RECEIPT_REPLY_UNAVAILABLE)
    );
}

#[test]
fn redaction_is_preserved_in_the_reply_bytes() {
    let fx = Fx::new();
    let user = fx.user();
    let reply = fx.start_home(&user, "secret-1", "deploy password=hunter2-correct-horse");
    let returned = serde_json::to_string(&reply).unwrap();
    assert!(!returned.contains("hunter2"), "{returned}");
    let r = published(fx.publish_all()).remove(0);
    let bytes = text(fx.read_reply(&user, &r));
    assert!(!bytes.contains("hunter2"), "{bytes}");
    assert!(bytes.contains("[REDACTED]"), "{bytes}");
    // Byte for byte the canonical form of the redacted reply the command returned.
    let mut value = serde_json::to_value(&reply).unwrap();
    value["replayed"] = json!(false);
    assert_eq!(bytes, devworks_core::jcs::canonicalize(&value).unwrap());
}

#[test]
fn re_publication_reuses_the_published_reply() {
    let fx = Fx::new();
    let user = fx.user();
    recorded(&fx.start_home(&user, "start-1", "Home"));
    let first = published(fx.publish_all()).remove(0);
    let artifacts = fx.count("SELECT count(*) FROM artifacts");
    let staging =
        fx.count("SELECT count(*) FROM operations WHERE capability='artifacts.stage_file'");
    // The same delivery runs again after its reply was recorded (for example a settlement that
    // an operator or a repair re-opened): it reuses the published reply and creates nothing.
    fx.store
        .with_writer_connection(|c| {
            super::super::conn::db(c.execute_batch(
                "UPDATE queue_entries SET state='Ready',owner_id=NULL,lease_until=NULL
                 WHERE consumer='receipt_replies'",
            ))
        })
        .unwrap();
    let outcomes = fx.publish_all();
    assert_eq!(outcomes.len(), 1);
    let ReplyOutcome::Existing(again) = &outcomes[0] else {
        panic!("{outcomes:?}")
    };
    assert_eq!(
        serde_json::to_value(again).unwrap(),
        serde_json::to_value(&first).unwrap()
    );
    assert_eq!(fx.count("SELECT count(*) FROM artifacts"), artifacts);
    assert_eq!(
        fx.count("SELECT count(*) FROM operations WHERE capability='artifacts.stage_file'"),
        staging
    );
    assert_eq!(fx.count("SELECT count(*) FROM receipt_replies"), 1);
    assert_eq!(fx.reply_deliveries().completed, 1);
    // The reply record is unique per receipt.
    let dup = fx.store.with_writer_connection(|c| {
        super::super::conn::db(c.execute_batch(
            "INSERT INTO receipt_replies SELECT actor_id,scope_key,idempotency_key,'other-command',
             artifact_id,version_id,schema_json,published_at FROM receipt_replies",
        ))
    });
    assert!(dup.is_err());
}

#[test]
fn a_delivery_that_does_not_name_its_receipts_decision_publishes_nothing() {
    let fx = Fx::new();
    let user = fx.user();
    recorded(&fx.start_home(&user, "start-1", "Home"));
    let intent = fx.claim().remove(0).payload;
    // A delivery carrying a real receipt's identity but another event (one a project's history
    // holds) cannot scope or publish that receipt's reply.
    fx.write(move |tx| {
        let event = devworks_core::protocol::EventId(Id::new("event-forged").unwrap());
        tx.history().append(NewEvent {
            id: event.clone(),
            actor: actor("user-a"),
            project_id: Some(project("project-a")),
            root_id: None,
            operation_id: None,
            aggregate_kind: "fixture".into(),
            aggregate_id: "forged".into(),
            aggregate_revision: 1,
            kind: "FixtureChanged".into(),
            payload: json!({}),
        })?;
        tx.queue().enqueue(NewQueueEntry {
            id: Id::new("forged-1").unwrap(),
            consumer: REPLY_CONSUMER.into(),
            event_id: event,
            root_id: None,
            operation_id: None,
            partition: None,
            priority: 0,
            ready_at_ms: T0,
            payload: intent,
        })?;
        Ok(())
    });
    let outcomes = fx.publish_all();
    assert_eq!(outcomes.len(), 1);
    assert!(
        matches!(&outcomes[0], ReplyOutcome::Poison(_)),
        "{outcomes:?}"
    );
    assert_eq!(fx.count("SELECT count(*) FROM artifacts"), 0);
    assert_eq!(fx.count("SELECT count(*) FROM receipt_replies"), 0);
}

#[test]
fn a_crash_before_publication_is_published_after_restart() {
    let fx = Fx::new();
    let user = fx.user();
    let reply = fx.start_home(&user, "start-1", "Home");
    // The process dies after the receipt committed, before its reply was published, and once
    // more while a publisher held the claim.
    let fx = fx.restart();
    assert_eq!(fx.claim().len(), 1);
    let fx = fx.restart();
    let recovered = fx.write(|tx| tx.queue().recover_claims(ClaimRecovery::PriorBoots, 100));
    assert_eq!(recovered.requeued.len(), 1);
    let r = published(fx.publish_all()).remove(0);
    let o = observation(fx.receipt(&fx.user(), "start-1"));
    o.check().unwrap();
    assert_eq!(o.command_id.as_ref(), Some(&recorded(&reply).command_id));
    assert_eq!(
        o.reply_artifact.map(|a| a.version_id),
        Some(r.version_id.clone())
    );
    assert_eq!(fx.count("SELECT count(*) FROM artifacts"), 1);
}

#[test]
fn a_crash_between_the_file_and_its_metadata_is_recovered_without_duplicates() {
    let fx = Fx::new();
    let user = fx.user();
    recorded(&fx.start_home(&user, "start-1", "Home"));
    let claimed = fx.claim();
    // The reply file is published, then the process dies before its metadata commits.
    *fx.files.crash_at.lock().unwrap() = Some(Step::Renamed);
    let crashed = catch_unwind(AssertUnwindSafe(|| {
        fx.replies.publish(&*fx.store, &claimed[0])
    }));
    assert!(crashed.is_err());
    assert_eq!(fx.count("SELECT count(*) FROM artifacts"), 0);
    assert_eq!(fx.count("SELECT count(*) FROM receipt_replies"), 0);
    assert_eq!(
        fx.files.stored_files().unwrap().len(),
        1,
        "an invisible orphan"
    );
    assert!(matches!(
        fault(fx.receipt(&user, "start-1")).code,
        DomainErrorCode::ViewUnavailable
    ));

    let fx = fx.restart();
    // Boot recovery: the artifact store fails the interrupted staging and collects its file;
    // the delivery claimed by the dead process is redelivered.
    let report = fx.svc.recover(&*fx.store).unwrap();
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(report.removed_orphans.len(), 1, "{report:?}");
    assert!(fx.files.stored_files().unwrap().is_empty());
    let recovered = fx.write(|tx| tx.queue().recover_claims(ClaimRecovery::PriorBoots, 100));
    assert_eq!(recovered.requeued.len(), 1);
    let r = published(fx.publish_all()).remove(0);
    assert_eq!(fx.count("SELECT count(*) FROM artifacts"), 1);
    assert_eq!(fx.count("SELECT count(*) FROM receipt_replies"), 1);
    assert_eq!(fx.files.stored_files().unwrap().len(), 1);
    let o = observation(fx.receipt(&fx.user(), "start-1"));
    assert_eq!(o.reply_artifact.map(|a| a.version_id), Some(r.version_id));
    // Recovery is idempotent once everything is published.
    let again = fx.svc.recover(&*fx.store).unwrap();
    assert!(
        again.failed.is_empty() && again.removed_orphans.is_empty(),
        "{again:?}"
    );
}

#[test]
fn a_failed_publication_in_boot_is_retried_without_residue() {
    let fx = Fx::new();
    let user = fx.user();
    recorded(&fx.start_home(&user, "start-1", "Home"));
    fx.flaky.fail_publish.store(true, Ordering::SeqCst);
    let claimed = fx.claim();
    let outcome = fx.replies.publish(&*fx.store, &claimed[0]);
    let ReplyOutcome::Retry(cause) = &outcome else {
        panic!("{outcome:?}")
    };
    assert!(cause.contains("DiskFull"), "{cause}");
    // The staging operation is already resolved and nothing is left behind.
    let states = fx.read(|s| {
        let ops = s.artifacts().unresolved_staging(None, 10)?;
        Ok(ops.len())
    });
    assert_eq!(states, 0);
    assert_eq!(
        fx.count("SELECT count(*) FROM operations WHERE capability='artifacts.stage_file' AND state='Failed'"),
        1
    );
    assert!(fx.files.stored_files().unwrap().is_empty());
    assert!(fx.files.staged().unwrap().is_empty());
    // The dispatcher's retry (durable backoff) redelivers it; the next attempt publishes.
    let (entry, fence) = (claimed[0].id.clone(), claimed[0].fence().unwrap());
    fx.write(move |tx| {
        tx.queue().fail(
            &entry,
            &fence,
            "retry",
            FailureDisposition::Retry {
                delay_ms: 0,
                max_attempts: 8,
            },
        )
    });
    published(fx.publish_all());
    assert_eq!(fx.count("SELECT count(*) FROM artifacts"), 1);
    observation(fx.receipt(&user, "start-1")).check().unwrap();
}

#[test]
fn a_stale_claim_publishes_nothing() {
    let fx = Fx::new();
    let user = fx.user();
    recorded(&fx.start_home(&user, "start-1", "Home"));
    let claimed = fx.claim();
    let stale = claimed[0].clone();
    // The claim is recovered (lease expired) and claimed again by another publisher.
    fx.h.clock.0.store(T0 + 120_000, Ordering::SeqCst);
    fx.write(|tx| tx.queue().recover_claims(ClaimRecovery::Expired, 100));
    let fresh = fx.claim();
    assert_eq!(fresh.len(), 1);
    assert!(matches!(
        fx.replies.publish(&*fx.store, &stale),
        ReplyOutcome::Stale
    ));
    assert_eq!(fx.count("SELECT count(*) FROM artifacts"), 0);
    assert!(
        fx.files.stored_files().unwrap().is_empty(),
        "the stale file was collected"
    );
    assert!(matches!(
        fx.replies.publish(&*fx.store, &fresh[0]),
        ReplyOutcome::Published(_)
    ));
}

#[test]
fn receipt_replies_are_retained_with_their_receipt() {
    let fx = Fx::new();
    let user = fx.user();
    recorded(&fx.start_home(&user, "start-1", "Home"));
    let r = published(fx.publish_all()).remove(0);
    let a = fx.read(|s| s.artifacts().artifact(&r.artifact_id)).unwrap();
    let versions = fx.read(|s| s.artifacts().versions(&r.artifact_id));
    for deletion in [
        json!({"kind": "DeleteVersion", "artifactId": r.artifact_id, "versionId": r.version_id,
            "expectedRevision": a.revision.to_string(),
            "expectedReferenceRevision": a.reference_revision.to_string()}),
        json!({"kind": "DeleteArtifact", "artifactId": r.artifact_id,
            "expectedRevision": a.revision.to_string(),
            "expectedReferenceRevision": a.reference_revision.to_string(),
            "expectedVersionsDigest": versions_digest(&versions)}),
    ] {
        let f = refused(fx.delete(&user, deletion));
        assert_eq!(
            (f.code, kind_of(&f)),
            (DomainErrorCode::Conflict, RECEIPT_RETAINED)
        );
    }
    // Nothing changed: the reply still reads and the receipt still observes it.
    text(fx.read_reply(&user, &r));
    observation(fx.receipt(&user, "start-1")).check().unwrap();
    let after = fx.read(|s| s.artifacts().artifact(&r.artifact_id)).unwrap();
    assert_eq!(after, a);
}

/// Seeds a workspace (Explicit) artifact owned by `owner`, as a workspace attachment would be.
fn seed_workspace_artifact(fx: &Fx, owner: &str) -> (Id, Id) {
    let bytes = b"workspace attachment";
    let digest = sha256_hex(bytes);
    let owner = actor(owner);
    let generation = "seed-generation".to_owned();
    let path = fx.files.managed_path(&digest, &generation).unwrap();
    let full = fx.h.root.0.join(&path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(&full, bytes).unwrap();
    fx.write(move |tx| {
        tx.artifacts()
            .put_blob(&digest, bytes.len() as i64, &generation, &path)?;
        let mut a = StoredArtifact {
            id: Id::new("workspace-artifact").unwrap(),
            origin_project_id: None,
            origin_work_id: None,
            title: "Attachment".into(),
            revision: 1,
            scope: ArtifactScope::Workspace,
            owner: Some(owner),
            retention: ArtifactRetention::Explicit,
            scope_revision: 1,
            reference_revision: 1,
            current_version_id: None,
            state: ArtifactState::Live,
            created_at_ms: T0,
            provenance: json!({}),
        };
        tx.artifacts().insert_artifact(&a)?;
        tx.artifacts().insert_version(&StoredVersion {
            id: Id::new("workspace-version").unwrap(),
            artifact_id: a.id.clone(),
            version_no: 1,
            blob_digest: digest,
            byte_count: bytes.len() as i64,
            blob_generation: generation,
            media_type: "text/plain".into(),
            created_at_ms: T0,
            deleted_at_ms: None,
            source: json!({}),
        })?;
        a.current_version_id = Some(Id::new("workspace-version").unwrap());
        tx.artifacts().update_artifact(&a)
    });
    (id("workspace-artifact"), id("workspace-version"))
}

#[test]
fn workspace_artifacts_follow_project_visibility_and_deletion_rules() {
    let fx = Fx::new();
    let (aid, vid) = seed_workspace_artifact(&fx, "user-a");
    let read = |ctx: &InvocationContext| {
        fx.artifact_query(
            ctx,
            "artifacts.read",
            json!({"kind": "Read", "artifactId": aid, "versionId": vid}),
        )
    };
    // Its audience reads it; an unconfined worker and a project-bound agent do not.
    assert_eq!(text(read(&fx.user())), "workspace attachment");
    text(read(&fx.agent("coordinator-w", None)));
    let agent_p = fx.agent("agent-p", Some("project-a"));
    assert_eq!(
        fault(read(&fx.agent("agent-a", None))).code,
        DomainErrorCode::NotFound
    );
    assert_eq!(fault(read(&agent_p)).code, DomainErrorCode::NotFound);
    // The read record names the workspace scope.
    let record = fx.read(|s| s.artifacts().artifact(&aid)).unwrap();
    assert_eq!(record.scope.project(), None);
    let list = |ctx: &InvocationContext| match result(fx.artifact_query(
        ctx,
        "artifacts.list",
        json!({"kind": "List", "projectId": "project-a"}),
    )) {
        OperationResult::ArtifactsList { value } => value.items.0,
        other => panic!("{other:?}"),
    };
    assert!(list(&fx.user()).is_empty(), "not a project-a artifact");

    // Shared with project-a (associated with its work), the project sees it like any shared
    // artifact, and its listing names the workspace scope.
    let (a2, _) = (aid.clone(), ());
    fx.write(move |tx| {
        tx.artifacts().add_share(&a2, &project("project-a"))?;
        let mut a = tx.artifacts().artifact(&a2)?.unwrap();
        a.scope_revision += 1;
        a.reference_revision += 1;
        tx.artifacts().update_artifact(&a)
    });
    text(read(&agent_p));
    let items = list(&agent_p);
    assert_eq!(items.len(), 1);
    assert!(matches!(items[0].scope, DataScope::Workspace));
    assert_eq!(
        fault(read(&fx.agent("agent-b", Some("project-b")))).code,
        DomainErrorCode::NotFound
    );

    // Deletion authorizes the whole reference set: a project-confined principal cannot delete
    // the workspace owner's artifact (nothing changes); the user can.
    let a = fx.read(|s| s.artifacts().artifact(&aid)).unwrap();
    let versions = fx.read(|s| s.artifacts().versions(&aid));
    let deletion = json!({"kind": "DeleteArtifact", "artifactId": aid,
        "expectedRevision": a.revision.to_string(),
        "expectedReferenceRevision": a.reference_revision.to_string(),
        "expectedVersionsDigest": versions_digest(&versions)});
    let f = refused(fx.delete(&agent_p, deletion.clone()));
    assert_eq!(
        (f.code, kind_of(&f)),
        (DomainErrorCode::Conflict, SHARED_REFERENCES_REMAIN)
    );
    assert_eq!(fx.read(|s| s.artifacts().artifact(&aid)).unwrap(), a);
    match &recorded(&fx.delete(&fx.user(), deletion)).decision {
        DomainDecision::Applied { .. } => {}
        other => panic!("{other:?}"),
    }
    let deleted = fx.read(|s| s.artifacts().artifact(&aid)).unwrap();
    assert_eq!(deleted.state, ArtifactState::Deleted);
    assert_eq!(fault(read(&fx.user())).code, DomainErrorCode::NotFound);
    // Tombstone before unlink: the blob waits for collection.
    let blob = fx
        .read(|s| s.artifacts().blob(&sha256_hex(b"workspace attachment")))
        .unwrap();
    assert_eq!(blob.state, BlobState::UnlinkPending);
}

#[test]
fn the_reply_migration_rebuilds_artifacts_and_backfills_earlier_receipts() {
    let m = MIGRATIONS.iter().find(|m| m.version == 8).unwrap();
    assert_eq!(m.checksum(), MIGRATION_0008_SHA256);
    assert!(m.rebuilds_tables);
    let highest = MIGRATIONS
        .iter()
        .filter(|m| m.version < 8)
        .map(|m| (m.minimum_reader, m.minimum_writer))
        .max()
        .unwrap();
    assert!(m.minimum_reader >= highest.0 && m.minimum_writer >= highest.1);
    assert_eq!((m.minimum_reader, m.minimum_writer), (8, 8));

    // A store written by build 7: one project artifact and one receipt without an intent.
    let h = Harness::new();
    let before: Vec<_> = MIGRATIONS
        .iter()
        .filter(|m| m.version <= 7)
        .copied()
        .collect();
    drop(SqliteStore::open_with(h.config(), &before).unwrap());
    let c = h.raw();
    c.execute_batch(
        "INSERT INTO actors VALUES('user-a','human','user-a');
         INSERT INTO projects VALUES('project-a','A','Active',1,1,1);
         INSERT INTO blobs(digest,byte_count,relative_path,state,generation) VALUES('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',1,'artifacts/x','Present','g');
         INSERT INTO artifacts(id,origin_project_id,origin_work_id,title,revision,current_project_id,scope_revision,current_version_id,state,created_at) VALUES('artifact-old','project-a',NULL,'Old',1,'project-a',1,NULL,'Live',1);
         INSERT INTO artifact_versions VALUES('version-old','artifact-old',1,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','text/plain',1,NULL,'{}');
         UPDATE artifacts SET current_version_id='version-old' WHERE id='artifact-old';
         INSERT INTO artifact_shares VALUES('artifact-old','project-a');
         INSERT INTO events(id,actor_id,project_id,aggregate_kind,aggregate_id,aggregate_revision,kind,created_at,payload_json) VALUES('event-old','user-a',NULL,'command','command-old',1,'CommandDecided',1,'{}');
         INSERT INTO command_receipts VALUES('user-a','scope/0/ui','key-old','sessions.start','d',NULL,'{\"receipt\":{\"commandId\":\"command-old\"}}',5);",
    )
    .unwrap();
    drop(c);
    let store = SqliteStore::open(h.config()).unwrap();
    let a = read_snapshot(&store, &ReadOptions::default(), |s| {
        s.artifacts().artifact(&id("artifact-old"))
    })
    .unwrap()
    .unwrap();
    assert_eq!(a.scope, ArtifactScope::Project(project("project-a")));
    assert_eq!(a.origin_project_id, Some(project("project-a")));
    assert_eq!((a.owner, a.retention), (None, ArtifactRetention::Explicit));
    assert_eq!(a.current_version_id, Some(id("version-old")));
    let shares = read_snapshot(&store, &ReadOptions::default(), |s| {
        s.artifacts().shares(&id("artifact-old"))
    })
    .unwrap();
    assert_eq!(shares, vec![project("project-a")]);
    let (consumer, payload, state): (String, String, String) = store
        .with_writer_connection(|c| {
            super::super::conn::db(c.query_row(
                "SELECT consumer,payload_json,state FROM queue_entries WHERE event_id='event-old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            ))
        })
        .unwrap();
    assert_eq!(
        (consumer.as_str(), state.as_str()),
        (REPLY_CONSUMER, "Ready")
    );
    let payload: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(
        payload,
        json!({"actorId": "user-a", "scopeKey": "scope/0/ui", "requestKey": "key-old",
            "commandId": "command-old"})
    );
    // Child tables still enforce their references to the rebuilt table.
    for orphan in [
        "INSERT INTO artifact_versions VALUES('version-x','no-artifact',1,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','text/plain',1,NULL,'{}')",
        "INSERT INTO artifact_shares VALUES('no-artifact','project-a')",
        "INSERT INTO documents VALUES('no-artifact','version-old',1,0)",
        "INSERT INTO artifact_tags VALUES('no-artifact','t')",
    ] {
        let refused =
            store.with_writer_connection(move |c| super::super::conn::db(c.execute_batch(orphan)));
        assert!(refused.is_err(), "{orphan}");
    }
    // The rebuilt table keeps every constraint: a workspace artifact needs its owner.
    let no_owner = store.with_writer_connection(|c| {
        super::super::conn::db(c.execute_batch(
            "INSERT INTO artifacts(id,title,revision,scope_revision,scope_kind) VALUES('bad','x',1,1,'Workspace')",
        ))
    });
    assert!(no_owner.is_err());
    let fk = store
        .with_writer_connection(|c| {
            super::super::conn::db(c.query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0)))
        })
        .unwrap();
    assert_eq!(fk, 1, "enforcement is restored after the rebuild");
}

#[test]
fn every_operation_has_a_qualified_reply_schema() {
    for d in devworks_core::registry::all() {
        if d.binding(devworks_core::registry::Mode::Command).is_none() {
            continue;
        }
        let s = devworks_core::services::artifacts::replies::reply_schema(d)
            .unwrap_or_else(|| panic!("{}", d.name));
        assert_eq!(s.schema_id.as_str(), format!("devworks.reply/{}", d.name));
    }
}

/// Per-command overhead of reply publication, run on demand:
/// `cargo test --release -p devworks-adapters --lib measure_reply_overhead -- --ignored
/// --nocapture`.
#[test]
#[ignore = "measurement, not a check"]
fn measure_reply_overhead() {
    const N: usize = 200;
    let fx = Fx::new();
    let user = fx.user();
    let t = std::time::Instant::now();
    for i in 0..N {
        recorded(&fx.start_home(&user, &format!("m-{i}"), "Home"));
    }
    let commands = t.elapsed();
    let t = std::time::Instant::now();
    let mut n = 0;
    loop {
        let batch = published(fx.publish_all()).len();
        if batch == 0 {
            break;
        }
        n += batch;
    }
    assert_eq!(n, N);
    let publish = t.elapsed();
    let files = fx.files.stored_files().unwrap().len();
    let bytes = fx.count("SELECT COALESCE(SUM(byte_count),0) FROM blobs");
    let t = std::time::Instant::now();
    fx.svc.recover(&*fx.store).unwrap();
    let recover = t.elapsed();
    println!(
        "commands: {:?}/command; publication: {:?}/reply; files: {files}; mean reply bytes: {}; \
         artifact recovery with {N} replies: {recover:?}",
        commands / N as u32,
        publish / N as u32,
        bytes / N as i64
    );
}
