//! Local diagnostics end to end on the real SQLite store: `workspace.record_diagnostic`,
//! `workspace.logs` and `workspace.diagnostics` through the gateway with receipts at both
//! schema revisions; attribution, host-reserved components and evidence visibility; redaction
//! and output bounds; filters, search, paging and tailing; the indexing log channels (with a
//! test double for the indexer); outcomes and retention across a restart; storage, queue and
//! process health with unavailable metrics never reported as zero; and migration 12. No
//! inference.
use super::{Harness, T0, through};
use crate::storage::sqlite::{MIGRATION_0012_SHA256, MIGRATIONS, SqliteStore};
use devworks_core::authority::HostAuthority;
use devworks_core::authority::redact::REDACTED;
use devworks_core::authority::service as auth;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::gateway::Gateway;
use devworks_core::ports::diagnostics::{
    Metric, StorageDatabase, StorageFiles, StorageHealthProbe,
};
use devworks_core::ports::storage::*;
use devworks_core::ports::{AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization};
use devworks_core::protocol::records::{
    DiagnosticEntry, DiagnosticErrorClass, DiagnosticOutcome, DiagnosticPage, Severity,
};
use devworks_core::protocol::results::{
    DomainCommandReply, DomainDecision, DomainQueryReply, OperationResult,
};
use devworks_core::protocol::{
    ActorId, CommandReply, Counter, DomainErrorCode, DomainFault, Id, OperationId, ProjectId,
    Rejection, Revision,
};
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::replies::REPLY_CONSUMER;
use devworks_core::services::diagnostics::{
    DiagnosticService, LogEntry, MESSAGE_LIMIT, OUTPUT_LIMIT, RESERVED_TARGET, RecoveryLink,
    channels,
};
use devworks_core::settings::catalog::AgentRole;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

macro_rules! json_of {
    ($v:expr) => {
        serde_json::to_string($v).unwrap()
    };
}

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn actor(s: &str) -> ActorId {
    ActorId(id(s))
}
fn project(s: &str) -> ProjectId {
    ProjectId(id(s))
}

/// A known secret the host registered with the diagnostics redactor.
const SECRET: &str = "hunter2-correct-horse-battery";
/// A credential-shaped token the redactor recognizes without registration.
const TOKEN: &str = "ghp_9fK2mQ8xZ4vT7bN1cR6yH3sL0pW5eJ2aD8gU";

/// A probe whose volume cannot report free space (an unavailable metric).
struct NoFreeSpace(Arc<SqliteStore>);
impl StorageHealthProbe for NoFreeSpace {
    fn files(&self) -> StorageFiles {
        StorageFiles {
            free_bytes: Metric::Unavailable("the volume did not report free space".into()),
            total_bytes: Metric::Unavailable("the volume did not report its size".into()),
            ..self.0.files()
        }
    }
    fn database(&self) -> Result<StorageDatabase, StorageError> {
        self.0.database()
    }
    fn integrity(&self) -> Result<IntegrityReport, StorageError> {
        self.0.integrity()
    }
}

/// Authorizes every request (the host grant a package's command would carry).
struct Granted;
impl AuthorityPort for Granted {
    fn authorize(
        &self,
        _: &InvocationContext,
        _: &AuthorityRequest<'_>,
    ) -> Result<Authorization, AuthorityDenial> {
        Ok(Authorization::direct())
    }
}

struct Fx {
    h: Harness,
    store: Arc<SqliteStore>,
    svc: Arc<DiagnosticService>,
    cases: UseCases,
    n: Arc<AtomicU64>,
}

impl Fx {
    fn new() -> Self {
        Self::with_probe(true)
    }
    fn with_probe(probe: bool) -> Self {
        let fx = Self::open(Harness::new(), Arc::new(AtomicU64::new(0)), probe);
        fx.store
            .with_writer_connection(|c| {
                super::super::conn::db(c.execute_batch(
                    "INSERT INTO projects VALUES('project-a','A','Active',1,1,1);
                     INSERT INTO projects VALUES('project-b','B','Active',1,1,1);",
                ))
            })
            .unwrap();
        fx.write(|tx| {
            for (a, k) in [
                ("user-a", PrincipalKind::Human),
                ("agent-a", PrincipalKind::Agent),
                ("agent-x", PrincipalKind::Agent),
                ("package-a", PrincipalKind::Package),
            ] {
                tx.actors().register(&actor(a), k, a)?;
            }
            Ok(())
        });
        let user = fx.ctx("user-a", PrincipalKind::Human, None);
        for (who, kind, role) in [
            ("agent-a", PrincipalKind::Agent, Some(AgentRole::Worker)),
            ("package-a", PrincipalKind::Package, None),
        ] {
            let by = user.clone();
            let binding = NewPrincipalBinding {
                actor: actor(who),
                kind,
                role,
                project_id: Some(project("project-a")),
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
    fn open(h: Harness, n: Arc<AtomicU64>, probe: bool) -> Self {
        let store = Arc::new(h.store());
        let probe: Option<Arc<dyn StorageHealthProbe>> =
            probe.then(|| store.clone() as Arc<dyn StorageHealthProbe>);
        let svc = Arc::new(DiagnosticService::new(
            store.clone(),
            h.ids.clone(),
            h.clock.clone(),
            probe,
        ));
        svc.register_secret(SECRET);
        let cases = UseCases::new().with_diagnostics(svc.clone());
        Self {
            h,
            store,
            svc,
            cases,
            n,
        }
    }
    /// A process restart: every handle is dropped, then the store is reopened (a new boot).
    fn restart(self) -> Self {
        let Fx {
            h,
            store,
            svc,
            cases,
            n,
        } = self;
        drop((cases, svc, store));
        Self::open(h, n, true)
    }
    fn ctx(&self, who: &str, kind: PrincipalKind, p: Option<&str>) -> InvocationContext {
        let info = self.store.info();
        HostIngress::new(&*self.store)
            .context(
                actor(who),
                kind,
                id(&format!(
                    "invocation-{}",
                    self.n.fetch_add(1, Ordering::SeqCst)
                )),
                id("ui-main"),
                Counter::new(info.restore_epoch).unwrap(),
                Revision::new(1).unwrap(),
            )
            .with_lineage(p.map(project), None, None)
    }
    fn user(&self) -> InvocationContext {
        self.ctx("user-a", PrincipalKind::Human, None)
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
    fn authority(&self) -> HostAuthority<'_> {
        HostAuthority {
            store: &*self.store,
            clock: &*self.h.clock,
            protected_branches: &[],
        }
    }
    fn command_keyed(
        &self,
        ctx: &InvocationContext,
        key: &str,
        mut body: Value,
        revision: Option<u32>,
    ) -> DomainCommandReply {
        body["kind"] = json!("workspace.record_diagnostic");
        let mut frame =
            json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": key, "body": body});
        if let Some(r) = revision {
            frame["schemaRevision"] = json!(r);
        }
        let authority = self.authority();
        let gateway = Gateway {
            authority: &authority,
            store: &*self.store,
            ids: &*self.h.ids,
        };
        self.cases
            .command(
                &gateway,
                ctx,
                "workspace.record_diagnostic",
                frame.to_string().as_bytes(),
            )
            .expect("addressable")
    }
    /// Records under a permissive authority port: a package's commands otherwise need a host
    /// grant, and this checks what the use case itself refuses.
    fn record_granted(&self, ctx: &InvocationContext, mut body: Value) -> DomainCommandReply {
        body["kind"] = json!("workspace.record_diagnostic");
        let key = format!("key-{}", self.n.fetch_add(1, Ordering::SeqCst));
        let frame = json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": key,
            "schemaRevision": 2, "body": body});
        let gateway = Gateway {
            authority: &Granted,
            store: &*self.store,
            ids: &*self.h.ids,
        };
        self.cases
            .command(
                &gateway,
                ctx,
                "workspace.record_diagnostic",
                frame.to_string().as_bytes(),
            )
            .expect("addressable")
    }
    fn record(&self, ctx: &InvocationContext, body: Value) -> DomainCommandReply {
        let key = format!("key-{}", self.n.fetch_add(1, Ordering::SeqCst));
        self.command_keyed(ctx, &key, body, Some(2))
    }
    fn query_at(
        &self,
        ctx: &InvocationContext,
        op: &str,
        mut body: Value,
        revision: Option<u32>,
    ) -> DomainQueryReply {
        body["kind"] = json!(op);
        let mut frame =
            json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": "q-1", "body": body});
        if let Some(r) = revision {
            frame["schemaRevision"] = json!(r);
        }
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
    fn logs(&self, ctx: &InvocationContext, filter: Value) -> DiagnosticPage {
        let mut body = json!({"page": {"limit": 100}, "afterSequence": "0"});
        for (k, v) in filter.as_object().unwrap() {
            body[k] = v.clone();
        }
        match result(self.query_at(ctx, "workspace.logs", body, Some(2))) {
            OperationResult::WorkspaceLogs { value } => value,
            other => panic!("{other:?}"),
        }
    }
    fn health(&self, ctx: &InvocationContext) -> Vec<DiagnosticEntry> {
        match result(self.query_at(
            ctx,
            "workspace.diagnostics",
            json!({"page": {"limit": 200}}),
            Some(2),
        )) {
            OperationResult::WorkspaceDiagnostics { value } => value.items.0,
            other => panic!("{other:?}"),
        }
    }
    fn log(&self, entry: LogEntry) -> i64 {
        self.svc.log(entry).expect("logged").sequence
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
fn entry_of(reply: DomainCommandReply) -> DiagnosticEntry {
    match recorded(reply).0 {
        DomainDecision::Applied {
            result: OperationResult::WorkspaceRecordDiagnostic { value },
        } => value,
        other => panic!("expected an applied record, got {other:?}"),
    }
}
fn refused(reply: DomainCommandReply) -> DomainFault {
    match recorded(reply).0 {
        DomainDecision::Refused { fault } => fault,
        other => panic!("expected a refusal, got {other:?}"),
    }
}
fn result(reply: DomainQueryReply) -> OperationResult {
    match reply {
        DomainQueryReply::Result { result, .. } => result,
        other => panic!("expected a result, got {other:?}"),
    }
}
fn query_fault(reply: DomainQueryReply) -> DomainFault {
    match reply {
        DomainQueryReply::Rejected { fault, .. } => fault,
        other => panic!("expected a fault, got {other:?}"),
    }
}
fn check<'a>(checks: &'a [DiagnosticEntry], component: &str) -> &'a DiagnosticEntry {
    checks
        .iter()
        .find(|c| c.component.0 == component)
        .unwrap_or_else(|| panic!("no {component} check in {checks:#?}"))
}
fn health(c: &DiagnosticEntry) -> String {
    serde_json::to_value(c.health.as_ref().expect("health"))
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}
fn metric(c: &DiagnosticEntry, name: &str) -> Value {
    let all = serde_json::to_value(c.metrics.as_ref().expect("metrics")).unwrap();
    all.as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("no metric {name} on {}", c.component.0))
}

// ---------------------------------------------------------------------------------------------
// workspace.record_diagnostic

#[test]
fn a_reported_diagnostic_is_attributed_redacted_bounded_and_replayed_from_its_receipt() {
    let fx = Fx::new();
    let user = fx.user();
    let stack = format!(
        "Authorization: Bearer {TOKEN}\n{}\nat render (app.js:1:1) password={SECRET}\n",
        "frame\n".repeat(8_000)
    );
    let body = json!({
        "component": channels::CLIENT, "code": "unhandled_error", "severity": "Error",
        "message": format!("Unhandled error while rendering; token {TOKEN} and {SECRET}"),
        "operation": "ui.render", "stage": "board", "outcome": "Failed",
        "errorClass": "Internal", "output": stack,
    });
    let key = "client-error-1";
    let first = entry_of(fx.command_keyed(&user, key, body.clone(), Some(2)));
    let text = json_of!(&first);
    assert!(!text.contains(TOKEN), "{text}");
    assert!(!text.contains(SECRET), "{text}");
    assert!(first.message.0.contains(REDACTED));
    assert!(first.redactions.as_ref().unwrap().get() >= 3);
    // Attributed by the host: the actor, the invocation correlation, origin Reported.
    assert_eq!(first.actor_id.as_ref(), Some(&actor("user-a")));
    assert_eq!(
        json_of!(first.origin.as_ref().unwrap()),
        "\"Reported\"",
        "{first:?}"
    );
    assert_eq!(
        first.correlation_id.as_ref().map(|c| c.0.as_str()),
        Some(user.invocation_id().as_str())
    );
    // Bounded: the output keeps its tail and counts the rest.
    let output = first.output.as_ref().unwrap();
    assert!(output.0.len() <= OUTPUT_LIMIT);
    assert!(
        output.0.ends_with("password=[REDACTED]\n"),
        "{}",
        &output.0[output.0.len() - 60..]
    );
    assert!(first.output_omitted_bytes.as_ref().unwrap().get() > 0);
    // A retry with the same key replays the receipt; nothing is recorded twice.
    let (again, replayed) = recorded(fx.command_keyed(&user, key, body, Some(2)));
    assert!(replayed);
    assert!(matches!(again, DomainDecision::Applied { .. }));
    let page = fx.logs(&user, json!({}));
    assert_eq!(page.items.0.len(), 1);
    assert_eq!(page.items.0[0].id, first.id);
    // Kept apart from model history: no history event carries the diagnostic text.
    let events = fx.read(|s| s.history().page(&EventFilter::All, 0, 500));
    for e in &events.events {
        let payload = e.payload.to_string();
        assert!(
            !payload.contains("Unhandled error while rendering"),
            "{payload}"
        );
    }
    // A long message is bounded too, with its omitted size stated.
    let long = entry_of(fx.record(
        &user,
        json!({"component": "client", "code": "long", "severity": "Info",
            "message": "x".repeat(MESSAGE_LIMIT * 2)}),
    ));
    assert!(long.message.0.len() < MESSAGE_LIMIT + 64);
    assert!(long.message.0.contains("bytes omitted]"));
}

#[test]
fn reporters_cannot_impersonate_host_health_and_evidence_must_be_visible() {
    let fx = Fx::new();
    let package = fx.ctx("package-a", PrincipalKind::Package, Some("project-a"));
    let agent = fx.ctx("agent-a", PrincipalKind::Agent, Some("project-a"));
    for (who, component) in [
        (&package, "storage.disk"),
        (&package, "health.readiness"),
        (&agent, "queue.receipt-replies"),
        (&agent, "indexing.repository"),
        (&agent, "provider"),
    ] {
        let fault = refused(fx.record_granted(
            who,
            json!({"component": component, "code": "healthy", "severity": "Info",
                "message": "All good"}),
        ));
        assert_eq!(fault.code, DomainErrorCode::PermissionDenied, "{component}");
        assert_eq!(
            fault.target_kind.as_ref().map(|k| k.0.as_str()),
            Some(RESERVED_TARGET)
        );
    }
    // A package reports under its own component; the record is its own, never the host's.
    let own = entry_of(fx.record_granted(
        &package,
        json!({"component": "package.formatter", "code": "crashed", "severity": "Warning",
            "message": "The formatter helper exited early", "outcome": "Retryable",
            "errorClass": "Dependency", "attempt": "2"}),
    ));
    assert_eq!(own.actor_id.as_ref(), Some(&actor("package-a")));
    assert_eq!(json_of!(own.origin.as_ref().unwrap()), "\"Reported\"");
    assert_eq!(own.project_id.as_ref(), Some(&project("project-a")));
    // An unbound agent may not record or read.
    let stranger = fx.ctx("agent-x", PrincipalKind::Agent, Some("project-a"));
    match fx.record(
        &stranger,
        json!({"component": "agent.notes", "code": "x", "severity": "Info", "message": "m"}),
    ) {
        CommandReply::Rejected { code, .. } => assert!(matches!(code, Rejection::PermissionDenied)),
        other => assert_eq!(refused(other).code, DomainErrorCode::PermissionDenied),
    }
    // Evidence that names no visible artifact version is refused.
    let fault = refused(fx.record(
        &fx.user(),
        json!({"component": "client", "code": "x", "severity": "Error", "message": "m",
            "evidence": {"artifactId": "artifact-missing", "versionId": "version-missing",
                "sha256": "a".repeat(64), "byteLength": "1", "mediaType": "text/plain"}}),
    ));
    assert_eq!(fault.code, DomainErrorCode::NotFound);
    assert_eq!(
        fault.target_kind.as_ref().map(|k| k.0.as_str()),
        Some("artifact")
    );
    // Inconsistent lifecycle or progress is refused, and nothing was recorded for any refusal.
    for bad in [
        json!({"startedAtMs": "5", "finishedAtMs": "4"}),
        json!({"progressCompleted": "6", "progressTotal": "5"}),
    ] {
        let mut body =
            json!({"component": "client", "code": "x", "severity": "Info", "message": "m"});
        for (k, v) in bad.as_object().unwrap() {
            body[k] = v.clone();
        }
        assert_eq!(
            refused(fx.record(&fx.user(), body)).code,
            DomainErrorCode::InvalidArgument
        );
    }
    let all = fx.logs(&fx.user(), json!({}));
    assert_eq!(all.items.0.len(), 1, "{:?}", all.items.0);
}

#[test]
fn revision_one_callers_keep_the_recorded_shapes() {
    let fx = Fx::new();
    let user = fx.user();
    let reply = fx.command_keyed(
        &user,
        "r1",
        json!({"component": "client", "code": "x", "severity": "Info", "message": "plain"}),
        None,
    );
    let e = entry_of(reply);
    let text = json_of!(&e);
    for gated in [
        "sequence",
        "origin",
        "actorId",
        "redactions",
        "recordedAtMs",
    ] {
        assert!(!text.contains(gated), "{gated} in {text}");
    }
    // Revision-2 members are refused at revision 1.
    let frame = json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": "q-1",
        "body": {"kind": "workspace.logs", "page": {"limit": 5}, "afterSequence": "0",
            "severity": "Error"}});
    let authority = fx.authority();
    let gateway = Gateway {
        authority: &authority,
        store: &*fx.store,
        ids: &*fx.h.ids,
    };
    let refused_r1 = fx.cases.query(
        &gateway,
        &user,
        "workspace.logs",
        frame.to_string().as_bytes(),
    );
    assert!(
        !matches!(refused_r1, Ok(DomainQueryReply::Result { .. })),
        "{refused_r1:?}"
    );
    let page = match result(fx.query_at(
        &user,
        "workspace.diagnostics",
        json!({"page": {"limit": 50}}),
        None,
    )) {
        OperationResult::WorkspaceDiagnostics { value } => value,
        other => panic!("{other:?}"),
    };
    let text = json_of!(&page);
    assert!(!text.contains("\"health\""), "{text}");
    assert!(!text.contains("\"metrics\""), "{text}");
    assert!(!text.contains("latestSequence"), "{text}");
}

// ---------------------------------------------------------------------------------------------
// workspace.logs

#[test]
fn logs_filter_search_page_newest_first_and_tail_after_a_sequence() {
    let fx = Fx::new();
    let user = fx.user();
    let op = OperationId(id("op-index-1"));
    let mut seqs = Vec::new();
    for (i, (component, severity, outcome, stage)) in [
        (
            "indexing.repository",
            Severity::Info,
            DiagnosticOutcome::Running,
            "discover",
        ),
        (
            "indexing.repository",
            Severity::Warning,
            DiagnosticOutcome::Running,
            "parse",
        ),
        (
            "indexing.repository",
            Severity::Error,
            DiagnosticOutcome::Failed,
            "parse",
        ),
        (
            "provider",
            Severity::Error,
            DiagnosticOutcome::Retryable,
            "request",
        ),
        (
            "client",
            Severity::Info,
            DiagnosticOutcome::Succeeded,
            "render",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        fx.h.clock.0.store(T0 + i as i64 * 1000, Ordering::SeqCst);
        seqs.push(fx.log(LogEntry {
            operation: Some("indexing.repository".into()),
            operation_id: (component == "indexing.repository").then(|| op.clone()),
            stage: Some(stage.into()),
            outcome: Some(outcome),
            project_id: (i == 4).then(|| project("project-b")),
            target_id: Some(id(&format!("target-{i}"))),
            ..LogEntry::new(
                component,
                "c",
                severity,
                format!("record {i} TimeOut detail"),
            )
        }));
    }
    assert!(seqs.windows(2).all(|w| w[0] < w[1]));
    // The provider failure seam: an error on the provider channel degrades provider health.
    let provider = fx.health(&user);
    let provider = check(&provider, channels::PROVIDER);
    assert_eq!(health(provider), "Degraded");
    assert_eq!(metric(provider, "failuresLast24h")["value"], "1");
    assert!(json_of!(provider.recovery.as_ref().unwrap()).contains("router.connections"));
    let n = |p: DiagnosticPage| p.items.0.len();
    assert_eq!(n(fx.logs(&user, json!({"component": "indexing"}))), 3);
    assert_eq!(
        n(fx.logs(&user, json!({"component": "indexing.repository"}))),
        3
    );
    assert_eq!(n(fx.logs(&user, json!({"component": "index"}))), 0);
    assert_eq!(n(fx.logs(&user, json!({"severity": "Warning"}))), 3);
    assert_eq!(n(fx.logs(&user, json!({"severity": "Error"}))), 2);
    assert_eq!(n(fx.logs(&user, json!({"outcome": "Failed"}))), 1);
    assert_eq!(n(fx.logs(&user, json!({"operationId": "op-index-1"}))), 3);
    assert_eq!(n(fx.logs(&user, json!({"stage": "parse"}))), 2);
    assert_eq!(n(fx.logs(&user, json!({"targetId": "target-3"}))), 1);
    assert_eq!(n(fx.logs(&user, json!({"projectId": "project-b"}))), 1);
    assert_eq!(n(fx.logs(&user, json!({"search": "timeout"}))), 5);
    assert_eq!(n(fx.logs(&user, json!({"search": "record 3"}))), 1);
    assert_eq!(
        n(fx.logs(
            &user,
            json!({"sinceMs": (T0 + 1000).to_string(), "untilMs": (T0 + 3000).to_string()})
        )),
        3
    );
    // Start after end is refused (the retained 400).
    let fault = query_fault(fx.query_at(
        &user,
        "workspace.logs",
        json!({"page": {"limit": 5}, "afterSequence": "0", "sinceMs": "5", "untilMs": "4"}),
        Some(2),
    ));
    assert_eq!(fault.code, DomainErrorCode::InvalidArgument);
    // Newest first, paged two at a time with a cursor bound to the filter.
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut page = json!({"limit": 2});
        if let Some(c) = &cursor {
            page["cursor"] = json!(c);
        }
        let p = match result(fx.query_at(
            &user,
            "workspace.logs",
            json!({"page": page, "afterSequence": "0", "order": "Descending"}),
            Some(2),
        )) {
            OperationResult::WorkspaceLogs { value } => value,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            p.latest_sequence.as_ref().unwrap().get(),
            *seqs.last().unwrap()
        );
        seen.extend(p.items.0.iter().map(|e| e.sequence.as_ref().unwrap().get()));
        match p.next_cursor {
            Some(c) => cursor = Some(c.0),
            None => break,
        }
    }
    let mut expected = seqs.clone();
    expected.reverse();
    assert_eq!(seen, expected);
    // A cursor from another filter is stale.
    let first = match result(fx.query_at(
        &user,
        "workspace.logs",
        json!({"page": {"limit": 1}, "afterSequence": "0"}),
        Some(2),
    )) {
        OperationResult::WorkspaceLogs { value } => value,
        other => panic!("{other:?}"),
    };
    let stale = query_fault(fx.query_at(
        &user,
        "workspace.logs",
        json!({"page": {"limit": 1, "cursor": first.next_cursor.unwrap().0},
            "afterSequence": "0", "severity": "Error"}),
        Some(2),
    ));
    assert_eq!(stale.code, DomainErrorCode::CursorExpired);
    // Tailing: only what came after the last sequence seen, and the live hint moved.
    let watched = fx.svc.watch().latest();
    assert_eq!(watched, *seqs.last().unwrap());
    let next = fx.log(LogEntry::new("client", "later", Severity::Info, "later"));
    assert!(fx.svc.watch().latest() >= next);
    let tail = fx.logs(
        &user,
        json!({"afterSequence": seqs.last().unwrap().to_string()}),
    );
    assert_eq!(tail.items.0.len(), 1);
    assert_eq!(tail.items.0[0].sequence.as_ref().unwrap().get(), next);
    // A project-confined agent sees its project's and workspace records, never another
    // project's.
    let agent = fx.ctx("agent-a", PrincipalKind::Agent, Some("project-a"));
    let visible = fx.logs(&agent, json!({}));
    assert!(visible.items.0.iter().all(|e| {
        e.project_id
            .as_ref()
            .is_none_or(|p| p == &project("project-a"))
    }));
    assert_eq!(visible.items.0.len(), 5);
    let stranger = fx.ctx("agent-x", PrincipalKind::Agent, Some("project-a"));
    let denied = query_fault(fx.query_at(
        &stranger,
        "workspace.logs",
        json!({"page": {"limit": 5}, "afterSequence": "0"}),
        Some(2),
    ));
    assert_eq!(denied.code, DomainErrorCode::PermissionDenied);
    // Copy for the clipboard is plain, redacted text.
    let copied = fx.svc.copy_text(&visible.items.0);
    assert!(copied.contains("record 0 TimeOut detail"));
}

// ---------------------------------------------------------------------------------------------
// LOG-01: indexing runs through their log channel (indexing itself is a later item)

/// Test double of the indexer: drives three runs through the channels indexing will write to.
struct FakeIndexer<'a> {
    svc: &'a DiagnosticService,
}
impl FakeIndexer<'_> {
    fn repository_run_fails(&self) {
        let run = self
            .svc
            .run(channels::REPOSITORY_INDEXING, "indexing.index")
            .operation_id(OperationId(id("op-repo-1")))
            .target("repository", id("repo-a"))
            .source_revision("0123abcdef")
            .attempt(1)
            .project(project("project-a"))
            .started_at(T0);
        run.stage(
            "discover",
            Severity::Info,
            "Found 120 files",
            Some((0, 120)),
        )
        .unwrap();
        run.stage(
            "parse",
            Severity::Warning,
            "src/huge.rs exceeds the parse budget and was skipped",
            Some((60, 120)),
        )
        .unwrap();
        run.finish(
            "parse",
            DiagnosticOutcome::Failed,
            Some(DiagnosticErrorClass::Timeout),
            "The TypeScript language server did not answer within 30 s while parsing src/web/app.ts",
            Some(&format!(
                "tsserver: request 42 timed out\nenv GITHUB_TOKEN={TOKEN}\n{}",
                "trace line\n".repeat(4_000)
            )),
            Some((61, 120)),
            vec![
                RecoveryLink::new("indexing.reindex", "Index the repository again")
                    .on("repository", id("repo-a")),
                RecoveryLink::new("indexing.status", "Show the repository's index status")
                    .on("repository", id("repo-a")),
            ],
        )
        .unwrap();
    }
    fn schema_run_is_partial(&self) {
        let run = self
            .svc
            .run(channels::SCHEMA_INDEXING, "schemas.index")
            .target("database_source", id("db-a"))
            .attempt(3);
        run.finish(
            "introspect",
            DiagnosticOutcome::Partial,
            Some(DiagnosticErrorClass::Permission),
            "3 of 40 tables could not be introspected: permission denied",
            Some("id | email\n1 | alice@example.com\n2 | bob@example.com\n"),
            Some((37, 40)),
            Vec::new(),
        )
        .unwrap();
    }
    fn semantic_run_succeeds(&self) {
        self.svc
            .run(channels::SEMANTIC_INDEXING, "indexing.index")
            .target("repository", id("repo-a"))
            .finish(
                "embed",
                DiagnosticOutcome::Succeeded,
                None,
                "Embedded 1 204 chunks",
                None,
                Some((1204, 1204)),
                Vec::new(),
            )
            .unwrap();
    }
}

#[test]
fn indexing_runs_are_diagnosable_through_their_log_channels() {
    let fx = Fx::new();
    let user = fx.user();
    let indexer = FakeIndexer { svc: &fx.svc };
    indexer.repository_run_fails();
    indexer.schema_run_is_partial();
    indexer.semantic_run_succeeds();
    // The run's whole history correlates on its operation: stages, progress, attempt, source
    // revision, the failure's class, the helper output (redacted and bounded) and recovery.
    let run = fx.logs(&user, json!({"operationId": "op-repo-1"})).items.0;
    assert_eq!(run.len(), 3);
    let stages: Vec<&str> = run
        .iter()
        .map(|e| e.stage.as_ref().unwrap().0.as_str())
        .collect();
    assert_eq!(stages, ["discover", "parse", "parse"]);
    let last = &run[2];
    assert_eq!(json_of!(last.outcome.as_ref().unwrap()), "\"Failed\"");
    assert_eq!(json_of!(last.error_class.as_ref().unwrap()), "\"Timeout\"");
    assert_eq!(last.attempt.as_ref().unwrap().get(), 1);
    assert_eq!(last.source_revision.as_ref().unwrap().0, "0123abcdef");
    assert_eq!(last.target_id.as_ref(), Some(&id("repo-a")));
    assert_eq!(
        (
            last.progress_completed.as_ref().unwrap().get(),
            last.progress_total.as_ref().unwrap().get()
        ),
        (61, 120)
    );
    assert_eq!(last.started_at_ms.as_ref().unwrap().get(), T0);
    assert!(last.finished_at_ms.is_some());
    let output = &last.output.as_ref().unwrap().0;
    assert!(!output.contains(TOKEN));
    assert!(output.len() <= OUTPUT_LIMIT);
    assert!(last.output_omitted_bytes.as_ref().unwrap().get() > 0);
    let recovery = json_of!(last.recovery.as_ref().unwrap());
    assert!(recovery.contains("indexing.reindex"), "{recovery}");
    assert!(last.message.0.contains("did not answer within 30 s"));
    // The warning in between stays visible.
    assert_eq!(json_of!(&run[1].severity), "\"Warning\"");
    // Database-schema helper output may hold source rows: it is never retained.
    let schema = fx
        .logs(&user, json!({"component": channels::SCHEMA_INDEXING}))
        .items
        .0;
    assert_eq!(schema.len(), 1);
    assert!(schema[0].output.is_none());
    assert!(schema[0].output_omitted_bytes.as_ref().unwrap().get() > 0);
    assert!(!json_of!(&schema[0]).contains("alice@example.com"));
    // Health shows each channel's latest run with its detail, never a bare failed badge.
    let checks = fx.health(&user);
    let repo = check(&checks, channels::REPOSITORY_INDEXING);
    assert_eq!(health(repo), "Failed");
    assert!(repo.message.0.contains("repo-a"), "{}", repo.message.0);
    assert!(repo.message.0.contains("parse"), "{}", repo.message.0);
    assert!(repo.message.0.contains("TypeScript language server"));
    assert_eq!(metric(repo, "failuresLast24h")["value"], "1");
    assert!(json_of!(repo.recovery.as_ref().unwrap()).contains("indexing.reindex"));
    assert_eq!(
        health(check(&checks, channels::SCHEMA_INDEXING)),
        "Degraded"
    );
    assert_eq!(
        health(check(&checks, channels::SEMANTIC_INDEXING)),
        "Healthy"
    );
}

// ---------------------------------------------------------------------------------------------
// LOG-03: outcomes, restart and retention

#[test]
fn outcomes_stay_distinct_and_logs_survive_a_restart_until_retention_removes_them() {
    let fx = Fx::new();
    let outcomes = [
        DiagnosticOutcome::Running,
        DiagnosticOutcome::Succeeded,
        DiagnosticOutcome::Partial,
        DiagnosticOutcome::Stale,
        DiagnosticOutcome::Cancelled,
        DiagnosticOutcome::Retryable,
        DiagnosticOutcome::Failed,
    ];
    for o in outcomes.clone() {
        fx.log(LogEntry {
            outcome: Some(o.clone()),
            ..LogEntry::new("client", "run", Severity::Info, json_of!(&o))
        });
    }
    let fx = fx.restart();
    let user = fx.user();
    let page = fx.logs(&user, json!({}));
    let names: Vec<String> = page
        .items
        .0
        .iter()
        .map(|e| json_of!(e.outcome.as_ref().unwrap()))
        .collect();
    assert_eq!(
        names,
        outcomes.iter().map(|o| json_of!(o)).collect::<Vec<_>>(),
        "every outcome survives the restart and stays distinct"
    );
    for o in outcomes.iter() {
        assert_eq!(
            fx.logs(&user, json!({"outcome": json_of!(o).trim_matches('"')}))
                .items
                .0
                .len(),
            1
        );
    }
    let latest = page.latest_sequence.as_ref().unwrap().get();
    // Within the default retention (30 days) a pass removes nothing.
    fx.h.clock.0.store(T0 + 29 * 86_400_000, Ordering::SeqCst);
    let pruned = fx.svc.prune().unwrap();
    assert_eq!((pruned.expired, pruned.over_cap), (0, 0));
    assert_eq!(fx.logs(&user, json!({})).items.0.len(), 7);
    // Past it they go, the pass records what it removed, and sequences are never reused.
    fx.h.clock.0.store(T0 + 31 * 86_400_000, Ordering::SeqCst);
    let pruned = fx.svc.prune().unwrap();
    assert_eq!(pruned.expired, 7);
    let left = fx.logs(&user, json!({})).items.0;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].component.0, channels::RETENTION);
    assert!(left[0].sequence.as_ref().unwrap().get() > latest);
    assert!(fx.svc.monitor().retention.is_some());
    // The record cap removes the oldest beyond it (repository semantics).
    fx.write(|tx| {
        for i in 0..5 {
            tx.diagnostics().append_diagnostic(&StoredDiagnostic {
                sequence: 0,
                id: Id::new(format!("cap-{i}")).unwrap(),
                recorded_at_ms: T0 + 31 * 86_400_000,
                severity: "Info".into(),
                component: "client".into(),
                code: "c".into(),
                message: format!("cap {i}"),
                origin: "Host".into(),
                actor: ActorId(Id::new("host").unwrap()),
                correlation_id: None,
                project_id: None,
                operation: None,
                operation_id: None,
                target_kind: None,
                target_id: None,
                stage: None,
                source_revision: None,
                attempt: None,
                started_at_ms: None,
                finished_at_ms: None,
                progress_completed: None,
                progress_total: None,
                outcome: None,
                error_class: None,
                output: None,
                output_omitted_bytes: 0,
                redactions: 0,
                evidence_json: None,
                metrics_json: "[]".into(),
                recovery_json: "[]".into(),
            })?;
        }
        tx.diagnostics().prune_diagnostics(0, 3)
    });
    let kept: Vec<String> = fx
        .logs(&user, json!({}))
        .items
        .0
        .iter()
        .map(|e| e.message.0.clone())
        .collect();
    assert_eq!(kept, ["cap 2", "cap 3", "cap 4"]);
}

// ---------------------------------------------------------------------------------------------
// OPS-01: health

#[test]
fn an_unavailable_metric_is_reported_unavailable_never_zero() {
    // No storage probe and no runtime: storage, queue, integrity, write probe, indexing and
    // providers have nothing measured, and say so.
    let fx = Fx::with_probe(false);
    let user = fx.user();
    let checks = fx.health(&user);
    for c in [
        "storage.migrations",
        "storage.wal",
        "storage.disk",
        "storage.integrity",
        "storage.write",
        "queue",
        channels::REPOSITORY_INDEXING,
        channels::PROVIDER,
    ] {
        assert_eq!(health(check(&checks, c)), "Unavailable", "{c}");
    }
    let provider = check(&checks, channels::PROVIDER);
    let failures = metric(provider, "failuresLast24h");
    assert_eq!(failures["availability"], "Unavailable");
    assert!(failures.get("value").is_none(), "{failures}");
    assert!(
        failures["reason"]
            .as_str()
            .unwrap()
            .contains("no provider attempt")
    );
    // Readiness does not call unmeasured essentials healthy.
    assert_eq!(health(check(&checks, "health.readiness")), "Degraded");
    // With a probe whose volume reports no free space: the disk metric is unavailable, not 0.
    let fx = Fx::new();
    let probe = Arc::new(NoFreeSpace(fx.store.clone()));
    let svc = Arc::new(DiagnosticService::new(
        fx.store.clone(),
        fx.h.ids.clone(),
        fx.h.clock.clone(),
        Some(probe),
    ));
    let entries: Vec<DiagnosticEntry> = svc
        .health(&fx.user())
        .unwrap()
        .iter()
        .map(|c| devworks_core::services::diagnostics::health::wire(c, &actor("host"), T0).unwrap())
        .collect();
    let disk = check(&entries, "storage.disk");
    assert_eq!(health(disk), "Unavailable");
    let free = metric(disk, "freeBytes");
    assert_eq!(free["availability"], "Unavailable");
    assert!(free.get("value").is_none());
    assert!(
        !json_of!(disk).contains("\"value\":\"0\""),
        "{}",
        json_of!(disk)
    );
    // A real zero stays a measured zero (no integrity problems once checked).
    svc.refresh_monitor();
    let entries: Vec<DiagnosticEntry> = svc
        .health(&fx.user())
        .unwrap()
        .iter()
        .map(|c| devworks_core::services::diagnostics::health::wire(c, &actor("host"), T0).unwrap())
        .collect();
    let integrity = check(&entries, "storage.integrity");
    assert_eq!(health(integrity), "Healthy");
    assert_eq!(metric(integrity, "problems")["availability"], "Available");
    assert_eq!(metric(integrity, "problems")["value"], "0");
}

#[test]
fn storage_health_reports_the_ledger_journal_disk_integrity_and_missing_schema_objects() {
    let fx = Fx::new();
    let user = fx.user();
    let before = fx.health(&user);
    assert_eq!(
        health(check(&before, "storage.integrity")),
        "Unavailable",
        "not checked yet in this boot"
    );
    fx.svc.refresh_monitor();
    let checks = fx.health(&user);
    let migrations = check(&checks, "storage.migrations");
    assert_eq!(health(migrations), "Healthy");
    let version = MIGRATIONS.last().unwrap().version.to_string();
    assert_eq!(metric(migrations, "storeSchemaVersion")["value"], version);
    assert_eq!(metric(migrations, "buildSchemaVersion")["value"], version);
    let wal = check(&checks, "storage.wal");
    assert_eq!(health(wal), "Healthy", "{}", wal.message.0);
    assert_eq!(metric(wal, "walBytes")["availability"], "Available");
    assert_eq!(metric(wal, "databaseBytes")["availability"], "Available");
    assert_eq!(metric(wal, "pageSize")["availability"], "Available");
    let disk = check(&checks, "storage.disk");
    #[cfg(any(unix, windows))]
    assert_eq!(metric(disk, "freeBytes")["availability"], "Available");
    assert_eq!(health(check(&checks, "storage.integrity")), "Healthy");
    assert_eq!(health(check(&checks, "storage.write")), "Healthy");
    let schema = check(&checks, "storage.schema");
    assert_eq!(health(schema), "Healthy", "{}", schema.message.0);
    assert_eq!(metric(schema, "missingObjects")["value"], "0");
    assert_eq!(health(check(&checks, "access.local")), "Healthy");
    assert!(
        check(&checks, "access.local")
            .message
            .0
            .contains("no issued MCP server tokens")
    );
    // A missing index is the retained db-health warning, named.
    fx.store
        .with_writer_connection(|c| {
            super::super::conn::db(c.execute_batch("DROP INDEX diagnostic_records_recorded"))
        })
        .unwrap();
    let checks = fx.health(&user);
    let schema = check(&checks, "storage.schema");
    assert_eq!(health(schema), "Degraded");
    assert!(schema.message.0.contains("diagnostic_records_recorded"));
    assert_eq!(metric(schema, "missingObjects")["value"], "1");
}

#[test]
fn queue_and_owned_process_health_come_from_the_queue_service_and_process_records() {
    let fx = Fx::new();
    // An owned process of this boot, still running when the process dies.
    fx.write(|tx| {
        tx.resources().insert_process(NewProcess {
            id: Id::new("proc-1").unwrap(),
            platform: "unix".into(),
            program: "tsserver".into(),
            argv_sha256: "a".repeat(64),
            operation_id: None,
            lease_id: None,
            actor: ActorId(Id::new("host").unwrap()),
            session_id: None,
            project_id: Some(ProjectId(Id::new("project-a").unwrap())),
            root_id: None,
            assignment_id: None,
        })
    });
    let fx = fx.restart();
    fx.svc.set_consumers(vec!["indexer".into()]);
    let user = fx.user();
    let checks = fx.health(&user);
    let processes = check(&checks, "process.owned");
    assert_eq!(health(processes), "Degraded");
    assert_eq!(metric(processes, "orphaned")["value"], "1");
    let links = json_of!(processes.recovery.as_ref().unwrap());
    assert!(
        links.contains("process.cancel") && links.contains("proc-1"),
        "{links}"
    );
    // Each consumer is reported from queue.consumer_status's counters.
    let indexer = check(&checks, "queue.indexer");
    assert_eq!(health(indexer), "Healthy");
    assert_eq!(metric(indexer, "failed")["value"], "0");
    check(&checks, &format!("queue.{REPLY_CONSUMER}"));
    assert_eq!(health(check(&checks, "health.readiness")), "Degraded");
    // A component filter narrows the checks.
    let only = match result(fx.query_at(
        &user,
        "workspace.diagnostics",
        json!({"page": {"limit": 50}, "component": "storage"}),
        Some(2),
    )) {
        OperationResult::WorkspaceDiagnostics { value } => value.items.0,
        other => panic!("{other:?}"),
    };
    assert!(!only.is_empty());
    assert!(only.iter().all(|c| c.component.0.starts_with("storage.")));
}

// ---------------------------------------------------------------------------------------------
// Migration 12

#[test]
fn migration_12_adds_the_diagnostic_log_to_a_version_10_store() {
    let m = MIGRATIONS
        .iter()
        .find(|m| m.version == 12)
        .expect("migration 12");
    assert_eq!(
        m.checksum(),
        MIGRATION_0012_SHA256,
        "an applied migration was edited"
    );
    assert_eq!((m.minimum_reader, m.minimum_writer), (8, 9));
    assert!(!m.rebuilds_tables);
    // Independent of migration 11: it names no table but its own.
    assert!(!m.sql.contains("REFERENCES"));
    let h = Harness::new();
    drop(SqliteStore::open_with(h.config(), &through(10)).unwrap());
    let s = h.store();
    let report = s.compatibility().unwrap();
    assert_eq!(
        (
            report.store_schema_version,
            report.minimum_reader,
            report.minimum_writer
        ),
        (MIGRATIONS.last().unwrap().version, 8, 9)
    );
    let s = Arc::new(s);
    let svc = DiagnosticService::new(s.clone(), h.ids.clone(), h.clock.clone(), None);
    let first = svc
        .log(LogEntry::new(
            "client",
            "upgraded",
            Severity::Info,
            "after upgrade",
        ))
        .unwrap();
    assert_eq!(first.sequence, 1);
    // The store refuses records that break its invariants.
    let bad = StoredDiagnostic {
        id: Id::new("bad-1").unwrap(),
        progress_completed: Some(5),
        progress_total: Some(4),
        ..first.clone()
    };
    assert!(
        run_write(&*s, WriteOptions::default(), move |tx| tx
            .diagnostics()
            .append_diagnostic(&bad))
        .is_err()
    );
    let dup = first.clone();
    assert!(matches!(
        run_write(&*s, WriteOptions::default(), move |tx| tx
            .diagnostics()
            .append_diagnostic(&dup)),
        Err(StorageError::Duplicate { .. })
    ));
}
