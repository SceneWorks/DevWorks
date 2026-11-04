//! Unified Knowledge end to end on the real SQLite store (DW-30): the 17 `knowledge.*`
//! operations through the gateway and host authority, with receipts, both schema revisions,
//! scoped retrieval (project and general Knowledge), lifecycle, provenance and decisions, the
//! lesson rules (user words take effect at once, inferred lessons on recurrence, promote,
//! challenge, retire, reject, addresses, delivery), cursors bound to scope revisions, previewed
//! permissioned deletion, scope moves, user import through a real saved artifact, and migration
//! 18. Nothing runs inference; retrieval is lexical and reports it.
use super::Harness;
use crate::fs::NativeFilesystem;
use crate::fs::artifacts::NativeArtifactFiles;
use crate::storage::sqlite::{MIGRATION_0018_SHA256, MIGRATIONS, SqliteStore};
use devworks_core::authority::HostAuthority;
use devworks_core::authority::reads::ReadRoot;
use devworks_core::authority::service as auth;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::gateway::Gateway;
use devworks_core::ports::files::{ArtifactSources, ResolvedSource, SourceOrigin, SourceRefusal};
use devworks_core::ports::retrieval::NoVectorIndex;
use devworks_core::ports::storage::*;
use devworks_core::protocol::data::ArtifactSource;
use devworks_core::protocol::records::{
    ArtifactRecord, Diagnostic, KnowledgeBundle, KnowledgeClaim, KnowledgeWriteReceipt,
    MutationSummary, SettingAssignment,
};
use devworks_core::protocol::results::{
    DomainCommandReply, DomainDecision, DomainQueryReply, OperationResult,
};
use devworks_core::protocol::support::Page;
use devworks_core::protocol::{
    ActorId, CommandReply, Counter, DomainErrorCode, DomainFault, Id, ProjectId, Revision,
};
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::knowledge::import::{IMPORT_SCHEMA_ID, import_schema_digest};
use devworks_core::services::knowledge::{KnowledgeService, THRESHOLD_KEY};
use devworks_core::settings::catalog::{AgentRole, descriptor};
use devworks_core::settings::model;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn actor(s: &str) -> ActorId {
    ActorId(id(s))
}
fn project(s: &str) -> ProjectId {
    ProjectId(id(s))
}
fn ws() -> Value {
    json!({"kind": "Workspace"})
}
fn proj(p: &str) -> Value {
    json!({"kind": "Project", "projectId": p})
}

/// Upload tickets mapped to files beneath one registered read root.
struct Sources {
    root: ReadRoot,
    handles: Mutex<HashMap<String, String>>,
}
impl ArtifactSources for Sources {
    fn resolve(
        &self,
        _: &InvocationContext,
        source: &ArtifactSource,
        _: Option<&ProjectId>,
    ) -> Result<ResolvedSource, SourceRefusal> {
        let ArtifactSource::Upload { ticket_id } = source else {
            return Err(SourceRefusal::NotFound);
        };
        let handles = self.handles.lock().unwrap();
        let relative = handles
            .get(ticket_id.as_str())
            .ok_or(SourceRefusal::NotFound)?;
        Ok(ResolvedSource {
            root: self.root.clone(),
            relative: relative.clone(),
            origin: SourceOrigin::default(),
        })
    }
}

struct Fx {
    h: Harness,
    store: Arc<SqliteStore>,
    cases: UseCases,
    knowledge: Arc<KnowledgeService>,
    sources: Arc<Sources>,
    n: AtomicU64,
}

impl Fx {
    /// Two projects, the user, an agent confined to each project and an unconfined agent.
    fn new() -> Self {
        let h = Harness::new();
        let src = h.root.0.with_extension("sources");
        let _ = std::fs::remove_dir_all(&src);
        std::fs::create_dir_all(&src).unwrap();
        let sources = Arc::new(Sources {
            root: ReadRoot {
                identity: crate::fs::identity_of(&std::fs::metadata(&src).unwrap()),
                location: src,
            },
            handles: Mutex::new(HashMap::new()),
        });
        let store = Arc::new(h.store());
        let files = Arc::new(NativeArtifactFiles::new(
            h.root.0.clone(),
            h.root.0.with_extension("cache"),
        ));
        let artifacts = Arc::new(ArtifactService::new(
            files.clone(),
            sources.clone(),
            Arc::new(NativeFilesystem::new()),
            h.clock.clone(),
            h.ids.clone(),
        ));
        let knowledge = Arc::new(KnowledgeService::new(
            h.ids.clone(),
            h.clock.clone(),
            Some(files),
            Arc::new(NoVectorIndex),
        ));
        let cases = UseCases::new()
            .with_artifacts(artifacts)
            .with_knowledge(knowledge.clone());
        let fx = Self {
            h,
            store,
            cases,
            knowledge,
            sources,
            n: AtomicU64::new(0),
        };
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
                ("agent-b", PrincipalKind::Agent),
                ("agent-w", PrincipalKind::Agent),
            ] {
                tx.actors().register(&actor(a), k, a)?;
            }
            Ok(())
        });
        for (who, p) in [
            ("agent-a", Some("project-a")),
            ("agent-b", Some("project-b")),
            ("agent-w", None),
        ] {
            let by = fx.user();
            let binding = NewPrincipalBinding {
                actor: actor(who),
                kind: PrincipalKind::Agent,
                role: Some(AgentRole::Worker),
                project_id: p.map(project),
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
    fn ctx(&self, who: &str, kind: PrincipalKind, p: Option<&str>) -> InvocationContext {
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
            .with_lineage(p.map(project), None, None)
    }
    fn user(&self) -> InvocationContext {
        self.ctx("user-a", PrincipalKind::Human, None)
    }
    fn agent(&self, who: &str, p: Option<&str>) -> InvocationContext {
        self.ctx(who, PrincipalKind::Agent, p)
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
    fn command_at(
        &self,
        ctx: &InvocationContext,
        op: &str,
        mut body: Value,
        revision: Option<u32>,
    ) -> DomainCommandReply {
        body["kind"] = json!(op);
        let key = format!("key-{}", self.n.fetch_add(1, Ordering::SeqCst));
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
            .command(&gateway, ctx, op, frame.to_string().as_bytes())
            .expect("addressable")
    }
    /// A command at the newest schema revision the operation registers.
    fn cmd(&self, ctx: &InvocationContext, op: &str, body: Value) -> DomainCommandReply {
        self.command_at(ctx, op, body, Some(newest(op)))
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
    fn q(&self, ctx: &InvocationContext, op: &str, body: Value) -> DomainQueryReply {
        self.query_at(ctx, op, body, Some(newest(op)))
    }

    // Knowledge helpers

    fn record(&self, ctx: &InvocationContext, scope: Value, batch: Value) -> DomainCommandReply {
        self.cmd(
            ctx,
            "knowledge.record",
            json!({"scope": scope, "batch": batch}),
        )
    }
    /// Records one claim about a new (or reused) entity; returns the receipt.
    fn fact(
        &self,
        ctx: &InvocationContext,
        scope: Value,
        subject: &str,
        claim: Value,
    ) -> KnowledgeWriteReceipt {
        let batch = json!({
            "entities": [{"clientId": "s", "label": subject, "entityType": "topic", "aliases": []}],
            "claims": [claim], "evidence": []});
        receipt(self.record(ctx, scope, batch))
    }
    fn claim_id(&self, ctx: &InvocationContext, scope: Value, subject: &str, claim: Value) -> Id {
        self.fact(ctx, scope, subject, claim).claim_ids.0[0].clone()
    }
    fn search(
        &self,
        ctx: &InvocationContext,
        scope: Value,
        text: &str,
        extra: Value,
    ) -> Page<KnowledgeClaim> {
        let mut body = json!({"scope": scope, "text": text, "tags": [], "kinds": [],
            "page": {"limit": 50}, "entityLimit": 50, "claimLimit": 50});
        if let Value::Object(m) = extra {
            for (k, v) in m {
                body[k] = v;
            }
        }
        match result(self.q(ctx, "knowledge.search", body)) {
            OperationResult::KnowledgeSearch { value } => value,
            other => panic!("{other:?}"),
        }
    }
    fn read_claim(&self, ctx: &InvocationContext, scope: Value, claim: &Id) -> KnowledgeBundle {
        match result(self.q(
            ctx,
            "knowledge.read",
            json!({"scope": scope, "claimId": claim}),
        )) {
            OperationResult::KnowledgeRead { value } => value,
            other => panic!("{other:?}"),
        }
    }
    fn claim(&self, ctx: &InvocationContext, scope: Value, claim: &Id) -> KnowledgeClaim {
        self.read_claim(ctx, scope, claim)
            .claims
            .0
            .into_iter()
            .find(|c| c.id == *claim)
            .unwrap()
    }
    fn revision(&self, claim: &Id) -> i64 {
        let c = claim.clone();
        self.read(move |s| s.knowledge().knowledge_claim(&c))
            .unwrap()
            .revision
    }
    fn diagnostics(&self, ctx: &InvocationContext, body: Value) -> (Vec<Diagnostic>, i64) {
        match self.q(ctx, "knowledge.diagnostics", body) {
            DomainQueryReply::Result {
                result: OperationResult::KnowledgeDiagnostics { value },
                stamp,
                ..
            } => (value.items.0, stamp.scope_revision.get()),
            other => panic!("{other:?}"),
        }
    }
    fn lessons(&self, ctx: &InvocationContext, scope: Value, extra: Value) -> Vec<KnowledgeClaim> {
        let mut body = json!({"scope": scope, "tags": ["lesson"], "page": {"limit": 50}});
        if let Value::Object(m) = extra {
            for (k, v) in m {
                body[k] = v;
            }
        }
        match result(self.q(ctx, "knowledge.tagged_lessons", body)) {
            OperationResult::KnowledgeTaggedLessons { value } => value.items.0,
            other => panic!("{other:?}"),
        }
    }
    fn set_threshold(&self, value: i64) {
        self.write(move |tx| {
            let current = model::current_layer(tx.configuration(), &ConfigScope::Workspace)?;
            let schema =
                serde_json::to_value(descriptor(THRESHOLD_KEY).unwrap().schema_ref()).unwrap();
            let a: SettingAssignment = serde_json::from_value(
                json!({"key": THRESHOLD_KEY, "value": {"schema": schema, "value": value}}),
            )
            .unwrap();
            let plan = model::plan(&current, &[a]);
            assert!(plan.is_valid());
            let body = model::body(
                &plan,
                &actor("user-a"),
                tx.now_ms(),
                json!({"kind": "test"}),
            );
            tx.configuration().append_revision(
                &ConfigScope::Workspace,
                model::NAMESPACE,
                current.revision,
                model::BODY_SCHEMA_VERSION,
                body,
            )?;
            Ok(())
        });
    }
}

fn newest(op: &str) -> u32 {
    devworks_core::registry::find(op)
        .and_then(|d| d.revisions.iter().map(|r| r.0).max())
        .unwrap_or(1)
}
fn decision(reply: DomainCommandReply) -> DomainDecision {
    match reply {
        CommandReply::Recorded { receipt, .. } => receipt.decision,
        other => panic!("expected a recorded decision, got {other:?}"),
    }
}
fn applied(reply: DomainCommandReply) -> OperationResult {
    match decision(reply) {
        DomainDecision::Applied { result } => result,
        other => panic!("expected applied, got {other:?}"),
    }
}
fn unchanged(reply: DomainCommandReply) -> OperationResult {
    match decision(reply) {
        DomainDecision::NoChange { result } => result,
        other => panic!("expected no change, got {other:?}"),
    }
}
fn refused(reply: DomainCommandReply) -> DomainFault {
    match decision(reply) {
        DomainDecision::Refused { fault } => fault,
        other => panic!("expected a refusal, got {other:?}"),
    }
}
fn receipt(reply: DomainCommandReply) -> KnowledgeWriteReceipt {
    match applied(reply) {
        OperationResult::KnowledgeRecord { value }
        | OperationResult::KnowledgeRevise { value }
        | OperationResult::KnowledgeRelate { value }
        | OperationResult::KnowledgeTag { value }
        | OperationResult::KnowledgeVerify { value }
        | OperationResult::KnowledgeRetract { value }
        | OperationResult::KnowledgeScope { value }
        | OperationResult::KnowledgePromote { value }
        | OperationResult::KnowledgeChallenge { value }
        | OperationResult::KnowledgeImportUserData { value } => value,
        other => panic!("{other:?}"),
    }
}
fn summary(reply: DomainCommandReply) -> MutationSummary {
    match applied(reply) {
        OperationResult::KnowledgeDeleteBySource { value }
        | OperationResult::KnowledgeDeleteSelected { value } => value,
        other => panic!("{other:?}"),
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
fn draft(kind: &str, value: &str) -> Value {
    json!({"kind": kind, "subjectId": "s", "predicate": "states", "value": value,
        "confidence": 0.7, "evidenceIds": [], "tags": []})
}
fn with(mut v: Value, extra: Value) -> Value {
    if let Value::Object(m) = extra {
        for (k, x) in m {
            v[k] = x;
        }
    }
    v
}
fn wire<T: std::fmt::Debug>(v: &T) -> String {
    format!("{v:?}")
}
#[allow(dead_code)]
fn wire_json(v: Value) -> String {
    serde_json::to_value(v)
        .unwrap()
        .as_str()
        .unwrap_or_default()
        .to_owned()
}
fn ids(page: &Page<KnowledgeClaim>) -> Vec<String> {
    page.items
        .0
        .iter()
        .map(|c| c.id.as_str().to_owned())
        .collect()
}
fn lesson_draft(source: &str, reference: Option<&str>, words: Option<&str>) -> Value {
    let mut l = json!({"source": source, "applicability": {"activities": ["implementing"],
        "paths": [], "tools": [], "repositoryIds": []}});
    if let Some(r) = reference {
        l["sourceRef"] = json!(r);
    }
    if let Some(w) = words {
        l["quotedWords"] = json!(w);
    }
    l
}

#[test]
fn migration_18_is_additive_with_its_recorded_checksum() {
    let m = MIGRATIONS
        .iter()
        .find(|m| m.version == 18)
        .expect("migration 18");
    assert_eq!(
        m.checksum(),
        MIGRATION_0018_SHA256,
        "an applied migration was edited"
    );
    assert_eq!((m.minimum_reader, m.minimum_writer), (8, 14));
    assert!(!m.rebuilds_tables);
    for statement in m.sql.split(';') {
        let s = statement.to_uppercase();
        assert!(
            !s.contains("DROP ") && !s.contains("ALTER TABLE") && !s.contains("DELETE FROM"),
            "migration 18 must only add: {statement}"
        );
    }
    let fx = Fx::new();
    let n: i64 = fx
        .store
        .with_writer_connection(|c| {
            super::super::conn::db(c.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name LIKE 'knowledge\\_%' ESCAPE '\\' \
                 AND type='table'",
                [],
                |r| r.get(0),
            ))
        })
        .unwrap();
    // Migration 1's five placeholders plus this migration's tables and FTS shadow tables.
    assert!(n >= 5 + 15, "{n}");
}

#[test]
fn record_search_and_read_keep_provenance_revisions_and_unicode_matching() {
    let fx = Fx::new();
    let user = fx.user();
    let batch = json!({
        "entities": [{"clientId": "e1", "label": "École Router", "entityType": "Component",
            "aliases": ["router"]}],
        "evidence": [{"id": "ev1", "source": {"kind": "Code", "sourceRef": "repo:devworks/src/router.rs"},
            "snippet": "fn route()", "sourceRevision": "commit-1"}],
        "claims": [with(json!({"kind": "Fact", "subjectId": "e1", "predicate": "uses",
            "value": "ΑΘΉΝΑ Tiers are round-robin", "confidence": 0.9, "evidenceIds": ["ev1"],
            "tags": ["Routing", "routing"]}), json!({"entryKind": "Architecture"}))]});
    let r = receipt(fx.record(&user, proj("project-a"), batch));
    assert_eq!(r.claim_ids.0.len(), 1);
    assert_eq!(r.entity_ids.0.len(), 1);
    let claim = r.claim_ids.0[0].clone();
    // Unicode case folding is symmetric: upper-case and diacritic-free words match.
    for text in ["αθήνα TIERS", "Αθήνα", "ecole", "ÉCOLE", "école"] {
        let page = fx.search(&user, proj("project-a"), text, json!({}));
        assert_eq!(ids(&page), [claim.as_str()], "{text}");
    }
    // Tags are folded and the category tag is added (KNOW-08).
    let c = fx.claim(&user, proj("project-a"), &claim);
    let tags: Vec<&str> = c.tags.0.iter().map(|t| t.0.as_str()).collect();
    assert_eq!(tags, ["knowledge", "routing"]);
    assert_eq!(wire(c.entry_kind.as_ref().unwrap()), "Architecture");
    assert_eq!(
        wire(&c.kind),
        "Fact",
        "the revision-1 kind is the projection"
    );
    assert_eq!(c.author_id.as_ref().unwrap().0.as_str(), "user-a");
    assert_eq!(wire(c.origin.as_ref().unwrap()), "User");
    assert_eq!(c.autonomous, Some(false));
    let page = fx.search(&user, proj("project-a"), " ", json!({"tags": ["ROUTING"]}));
    assert_eq!(ids(&page), [claim.as_str()]);
    let bundle = fx.read_claim(&user, proj("project-a"), &claim);
    assert_eq!(bundle.entities.0[0].label.0, "École Router");
    assert_eq!(bundle.evidence.0[0].snippet.0, "fn route()");
    let revisions = bundle.revisions.unwrap().0;
    assert_eq!(revisions.len(), 1);
    assert_eq!(revisions[0].change.0, "Recorded");
    // Revision 1 readers see the recorded shape only.
    let r1 = match result(fx.query_at(
        &user,
        "knowledge.read",
        json!({"scope": proj("project-a"), "claimId": claim}),
        None,
    )) {
        OperationResult::KnowledgeRead { value } => value,
        other => panic!("{other:?}"),
    };
    let v = serde_json::to_value(&r1).unwrap();
    assert!(v.get("revisions").is_none() && v["claims"][0].get("lifecycle").is_none());
    // Retrieval says it is lexical only; nothing claims vector search ran.
    let (d, _) = fx.diagnostics(&user, json!({"scope": proj("project-a")}));
    let retrieval = d
        .iter()
        .find(|x| x.component.0 == "knowledge.retrieval")
        .unwrap();
    assert_eq!(retrieval.code.0, "LexicalOnly");
    // The write's durable receipt.
    let status = match result(fx.q(
        &user,
        "knowledge.write_status",
        json!({"writeId": r.write_id}),
    )) {
        OperationResult::KnowledgeWriteStatus { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(status.claim_ids.0, r.claim_ids.0);
}

#[test]
fn project_knowledge_stays_scoped_and_general_knowledge_reaches_every_project() {
    let fx = Fx::new();
    let user = fx.user();
    let a = fx.agent("agent-a", Some("project-a"));
    let b = fx.agent("agent-b", Some("project-b"));
    let secret = fx.claim_id(
        &a,
        proj("project-a"),
        "Alpha",
        draft("Fact", "alpha secret plan"),
    );
    let general = fx.claim_id(&user, ws(), "Rust", draft("Fact", "alpha general rule"));
    // Project B sees general Knowledge and never project A's.
    assert_eq!(
        ids(&fx.search(&b, proj("project-b"), "alpha", json!({}))),
        [general.as_str()]
    );
    let mut both: Vec<String> = ids(&fx.search(&a, proj("project-a"), "alpha", json!({})));
    both.sort();
    let mut expected = vec![general.as_str().to_owned(), secret.as_str().to_owned()];
    expected.sort();
    assert_eq!(both, expected);
    // Naming another project's claim is NotFound (existence is not disclosed).
    let f = query_fault(fx.q(
        &b,
        "knowledge.read",
        json!({"scope": proj("project-b"), "claimId": secret}),
    ));
    assert_eq!(f.code, DomainErrorCode::NotFound);
    // A confined agent cannot address another project or write general Knowledge.
    let f = query_fault(fx.q(
        &b,
        "knowledge.read",
        json!({"scope": proj("project-a"), "claimId": secret}),
    ));
    assert_eq!(f.code, DomainErrorCode::PermissionDenied);
    // Its host binding confines it even when an invocation names no project.
    let unscoped = fx.agent("agent-b", None);
    match fx.q(
        &unscoped,
        "knowledge.read",
        json!({"scope": proj("project-a"), "claimId": secret}),
    ) {
        DomainQueryReply::Rejected { fault, .. } => {
            assert_eq!(fault.code, DomainErrorCode::PermissionDenied)
        }
        other => panic!("project A was readable from project B's binding: {other:?}"),
    }
    let f = refused(fx.record(
        &b,
        ws(),
        json!({"entities": [{"clientId": "s", "label": "x",
        "entityType": "t", "aliases": []}], "claims": [], "evidence": []}),
    ));
    assert_eq!(f.code, DomainErrorCode::PermissionDenied);
    // A project-A entity cannot be referenced from project B (cross-scope identifier).
    let subject = fx.read_claim(&a, proj("project-a"), &secret).entities.0[0]
        .id
        .clone();
    let f = refused(fx.record(
        &b,
        proj("project-b"),
        json!({"entities": [], "evidence": [],
        "claims": [with(draft("Fact", "leak"), json!({"subjectId": subject}))]}),
    ));
    assert_eq!(f.code, DomainErrorCode::NotFound);
    // The unconfined agent and the user write general Knowledge.
    let w = fx.agent("agent-w", None);
    fx.claim_id(&w, ws(), "Rust", draft("Fact", "unconfined general"));
    // A project that does not exist is NotFound, not an empty scope.
    let f = query_fault(fx.q(
        &user,
        "knowledge.diagnostics",
        json!({"scope": proj("project-z")}),
    ));
    assert_eq!(f.code, DomainErrorCode::NotFound);
    // Retrieval for context assembly (the CONTEXT seam) is scoped the same way.
    let rule = fx.claim_id(
        &user,
        proj("project-a"),
        "Fmt",
        with(
            draft("Lesson", "run fmt"),
            json!({"lesson": lesson_draft("Prompt", None, None)}),
        ),
    );
    let rules_b = fx.read(|s| fx.knowledge.rules_for(s, Some(&project("project-b")), None));
    assert!(rules_b.iter().all(|r| r.id != rule));
    let rules_a = fx.read(|s| {
        fx.knowledge
            .rules_for(s, Some(&project("project-a")), Some("implementing"))
    });
    assert!(rules_a.iter().any(|r| r.id == rule));
}

#[test]
fn lifecycle_revisions_supersession_verification_and_staleness() {
    let fx = Fx::new();
    let user = fx.user();
    let batch = json!({
        "entities": [{"clientId": "s", "label": "Router", "entityType": "c", "aliases": []}],
        "evidence": [{"id": "ev", "source": {"kind": "Code", "sourceRef": "repo:a/router.rs"},
            "snippet": "x", "sourceRevision": "c1"}],
        "claims": [with(draft("Fact", "tiers rotate"), json!({"evidenceIds": ["ev"]}))]});
    let claim = receipt(fx.record(&user, proj("project-a"), batch))
        .claim_ids
        .0[0]
        .clone();
    // A revise at a stale revision is a RevisionConflict and changes nothing.
    let f = refused(fx.cmd(
        &user,
        "knowledge.revise",
        json!({"scope": proj("project-a"),
        "claimId": claim, "expectedRevision": "7", "replacement": draft("Fact", "x")}),
    ));
    assert_eq!(f.code, DomainErrorCode::RevisionConflict);
    // A source change observed later makes the claim stale until it is verified again.
    let stale = fx.write({
        let k = fx.knowledge.clone();
        move |tx| k.observe_source(tx, "repo:a/router.rs", "c2")
    });
    assert_eq!(stale, vec![claim.clone()]);
    let c = fx.claim(&user, proj("project-a"), &claim);
    assert_eq!(c.stale, Some(true));
    assert_eq!(
        fx.search(&user, proj("project-a"), " ", json!({"stale": true}))
            .items
            .0
            .len(),
        1
    );
    let rev = fx.revision(&claim);
    receipt(fx.cmd(
        &user,
        "knowledge.verify",
        json!({"scope": proj("project-a"), "claimId": claim,
        "expectedRevision": rev.to_string(), "reason": "checked", "evidenceIds": [],
        "sourceRevision": "c2"}),
    ));
    let c = fx.claim(&user, proj("project-a"), &claim);
    assert_eq!(c.stale, Some(false));
    assert_eq!(c.verified_source_revision.as_ref().unwrap().0, "c2");
    assert_eq!(wire(&c.state), "Verified");
    // Supersession keeps both, linked; the old one leaves active search.
    let rev = fx.revision(&claim);
    let newer = receipt(fx.record(
        &user,
        proj("project-a"),
        json!({"entities": [], "evidence": [],
        "claims": [with(draft("Fact", "tiers rotate by pin"), json!({"subjectId": c.subject_id,
            "supersedes": claim}))]}),
    ))
    .claim_ids
    .0[0]
        .clone();
    let old = fx.claim(&user, proj("project-a"), &claim);
    assert_eq!(wire(old.lifecycle.as_ref().unwrap()), "Superseded");
    assert_eq!(old.superseded_by.as_ref(), Some(&newer));
    assert!(fx.revision(&claim) > rev);
    assert_eq!(
        fx.read_claim(&user, proj("project-a"), &newer)
            .supersession_ids
            .0,
        std::slice::from_ref(&claim)
    );
    // Retraction keeps it readable as retracted.
    let rev = fx.revision(&newer);
    receipt(fx.cmd(
        &user,
        "knowledge.retract",
        json!({"scope": proj("project-a"), "claimId": newer,
        "expectedRevision": rev.to_string(), "reason": "wrong", "evidenceIds": []}),
    ));
    let r = fx.claim(&user, proj("project-a"), &newer);
    assert_eq!(wire(r.lifecycle.as_ref().unwrap()), "Retracted");
    assert!(
        fx.search(&user, proj("project-a"), "pin", json!({}))
            .items
            .0
            .is_empty()
    );
    assert_eq!(
        fx.search(
            &user,
            proj("project-a"),
            "pin",
            json!({"states": ["Retracted"]})
        )
        .items
        .0
        .len(),
        1
    );
    // A recorded past revision reads back as it was.
    let past = match result(fx.q(
        &user,
        "knowledge.read",
        json!({"scope": proj("project-a"),
        "claimId": claim, "revision": "1"}),
    )) {
        OperationResult::KnowledgeRead { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(wire(past.claims.0[0].lifecycle.as_ref().unwrap()), "Active");
}

#[test]
fn conflicting_claims_stay_inspectable() {
    let fx = Fx::new();
    let user = fx.user();
    let a = receipt(fx.record(
        &user,
        proj("project-a"),
        json!({
        "entities": [{"clientId": "s", "label": "Port", "entityType": "c", "aliases": []}],
        "evidence": [], "claims": [draft("Fact", "8080")]}),
    ));
    let subject = fx
        .read_claim(&user, proj("project-a"), &a.claim_ids.0[0])
        .entities
        .0[0]
        .id
        .clone();
    let b = receipt(fx.record(
        &user,
        proj("project-a"),
        json!({"entities": [], "evidence": [],
        "claims": [with(draft("Fact", "9090"), json!({"subjectId": subject}))]}),
    ));
    assert!(b.diagnostics.0.iter().any(|d| d.code.0 == "conflict"));
    let bundle = fx.read_claim(&user, proj("project-a"), &a.claim_ids.0[0]);
    assert_eq!(bundle.conflicts.0.len(), 1);
    assert_eq!(bundle.claims.0.len(), 2, "both claims are kept and shown");
    // An explicit contradiction is shown too.
    let c = fx.claim_id(
        &user,
        proj("project-a"),
        "Other",
        draft("Fact", "never port 80"),
    );
    receipt(fx.cmd(
        &user,
        "knowledge.relate",
        json!({"scope": proj("project-a"),
        "subjectId": c, "predicate": "contradicts", "objectId": a.claim_ids.0[0],
        "evidenceIds": []}),
    ));
    let bundle = fx.read_claim(&user, proj("project-a"), &c);
    assert_eq!(bundle.conflicts.0.len(), 1);
    assert_eq!(bundle.relations.unwrap().0.len(), 1);
}

#[test]
fn decisions_name_who_made_them() {
    let fx = Fx::new();
    let user = fx.user();
    let a = fx.agent("agent-a", Some("project-a"));
    let own = fx.claim_id(&a, proj("project-a"), "D", draft("Decision", "use sqlite"));
    let c = fx.claim(&a, proj("project-a"), &own);
    assert_eq!(
        (wire(c.origin.as_ref().unwrap()), c.autonomous),
        ("Agent".into(), Some(true))
    );
    let approved = fx.claim_id(
        &a,
        proj("project-a"),
        "D",
        with(
            draft("Decision", "use fts5"),
            json!({"origin": "User", "originRef": "message-9"}),
        ),
    );
    let c = fx.claim(&a, proj("project-a"), &approved);
    assert_eq!(
        (wire(c.origin.as_ref().unwrap()), c.autonomous),
        ("User".into(), Some(false))
    );
    assert_eq!(c.origin_ref.as_ref().unwrap().0, "message-9");
    // An agent cannot claim the user's origin without the approval it relays.
    let f = refused(fx.record(
        &a,
        proj("project-a"),
        json!({"entities": [{"clientId": "s",
        "label": "D", "entityType": "topic", "aliases": []}], "evidence": [],
        "claims": [with(draft("Decision", "x"), json!({"origin": "User"}))]}),
    ));
    assert_eq!(f.code, DomainErrorCode::InvalidArgument);
    let mine = fx.claim_id(&user, proj("project-a"), "D", draft("Decision", "ship it"));
    let c = fx.claim(&user, proj("project-a"), &mine);
    assert_eq!(
        (wire(c.origin.as_ref().unwrap()), c.autonomous),
        ("User".into(), Some(false))
    );
}

#[test]
fn lessons_take_effect_by_origin_and_recurrence_without_approval() {
    let fx = Fx::new();
    let user = fx.user();
    let a = fx.agent("agent-a", Some("project-a"));
    // The user's own words: effective at once.
    let prompt = fx.claim_id(
        &user,
        proj("project-a"),
        "Fmt",
        with(
            draft("Lesson", "run cargo fmt"),
            json!({"lesson": lesson_draft("Prompt", Some("p1"), None)}),
        ),
    );
    let l = fx.claim(&user, proj("project-a"), &prompt).lesson.unwrap();
    assert_eq!(
        (wire(&l.rule_state), wire(l.effect.as_ref().unwrap())),
        ("Effective".into(), "UserInstruction".into())
    );
    // An agent relays the user's words; it cannot state them as its own prompt.
    let f = refused(fx.record(&a, proj("project-a"), json!({"entities": [{"clientId": "s",
        "label": "x", "entityType": "topic", "aliases": []}], "evidence": [],
        "claims": [with(draft("Lesson", "x"), json!({"lesson": lesson_draft("Prompt", None, None)}))]})));
    assert_eq!(f.code, DomainErrorCode::PermissionDenied);
    let f = refused(fx.record(&a, proj("project-a"), json!({"entities": [{"clientId": "s",
        "label": "x", "entityType": "topic", "aliases": []}], "evidence": [],
        "claims": [with(draft("Lesson", "x"), json!({"lesson": lesson_draft("RelayedReview", Some("r1"), None)}))]})));
    assert_eq!(
        f.code,
        DomainErrorCode::InvalidArgument,
        "relayed words are required"
    );
    let relayed = fx.claim_id(
        &a,
        proj("project-a"),
        "Review",
        with(
            draft("Lesson", "test the edge"),
            json!({"lesson":
            lesson_draft("RelayedReview", Some("https://review/1"), Some("please test the edge"))}),
        ),
    );
    assert_eq!(
        wire(
            &fx.claim(&a, proj("project-a"), &relayed)
                .lesson
                .unwrap()
                .rule_state
        ),
        "Effective"
    );
    // An inferred lesson is candidate evidence until three distinct events (the default).
    let inferred = fx.claim_id(
        &a,
        proj("project-a"),
        "Retry",
        with(
            draft("Lesson", "retry flaky io"),
            json!({"lesson": lesson_draft("Agent", Some("e1"), None)}),
        ),
    );
    let recur = |reference: &str| {
        fx.record(
            &a,
            proj("project-a"),
            json!({"entities": [], "evidence": [], "claims": [
            with(draft("Lesson", "retry flaky io again"), json!({"subjectId": "unused-entity",
                "lesson": with(lesson_draft("Subagent", Some(reference), None),
                    json!({"sameAs": inferred}))}))]}),
        )
    };
    assert_eq!(
        wire(
            &fx.claim(&a, proj("project-a"), &inferred)
                .lesson
                .unwrap()
                .rule_state
        ),
        "Candidate"
    );
    receipt(recur("e2"));
    // The same source event again is one event (recurrence is not proof).
    let again = recur("e2");
    assert!(matches!(decision(again), DomainDecision::NoChange { .. }));
    let l = fx.claim(&a, proj("project-a"), &inferred).lesson.unwrap();
    assert_eq!(
        (wire(&l.rule_state), l.recurrence.get()),
        ("Candidate".into(), 2)
    );
    let r = receipt(recur("e3"));
    assert!(r.diagnostics.0.iter().any(|d| d.code.0 == "rule_effective"));
    let l = fx.claim(&a, proj("project-a"), &inferred).lesson.unwrap();
    assert_eq!(
        (wire(&l.rule_state), wire(l.effect.as_ref().unwrap())),
        ("Effective".into(), "Recurrence".into())
    );
    // No rule asks for approval: an agent cannot promote or reject; the user can.
    let candidate = fx.claim_id(
        &a,
        proj("project-a"),
        "Docs",
        with(
            draft("Lesson", "update docs"),
            json!({"lesson": lesson_draft("BotReview", Some("bot-1"), None)}),
        ),
    );
    let rev = fx.revision(&candidate);
    let f = refused(fx.cmd(
        &a,
        "knowledge.promote",
        json!({"claimId": candidate,
        "expectedRevision": rev.to_string(), "destination": proj("project-a")}),
    ));
    assert_eq!(f.code, DomainErrorCode::PermissionDenied);
    receipt(fx.cmd(&a, "knowledge.promote", json!({"claimId": candidate,
        "expectedRevision": rev.to_string(), "destination": proj("project-a"), "action": "Propose"})));
    assert_eq!(
        wire(
            &fx.claim(&a, proj("project-a"), &candidate)
                .lesson
                .unwrap()
                .rule_state
        ),
        "Proposed"
    );
    let rev = fx.revision(&candidate);
    receipt(fx.cmd(
        &user,
        "knowledge.promote",
        json!({"claimId": candidate,
        "expectedRevision": rev.to_string(), "destination": proj("project-a")}),
    ));
    let l = fx
        .claim(&user, proj("project-a"), &candidate)
        .lesson
        .unwrap();
    assert_eq!(
        (wire(&l.rule_state), wire(l.effect.as_ref().unwrap())),
        ("Effective".into(), "Promoted".into())
    );
    // Challenge: anyone opens one, the rule stays effective; the user retires it.
    let rev = fx.revision(&prompt);
    let opened = receipt(fx.cmd(
        &a,
        "knowledge.challenge",
        json!({"scope": proj("project-a"),
        "claimId": prompt, "expectedRevision": rev.to_string(), "reason": "breaks CI",
        "evidenceIds": []}),
    ));
    assert!(
        opened
            .diagnostics
            .0
            .iter()
            .any(|d| d.code.0 == "challenge_opened")
    );
    let l = fx.claim(&user, proj("project-a"), &prompt).lesson.unwrap();
    assert_eq!(wire(&l.rule_state), "Effective");
    let challenge = l.challenges.0[0].id.clone();
    let rev = fx.revision(&prompt);
    let f = refused(fx.cmd(
        &a,
        "knowledge.challenge",
        json!({"scope": proj("project-a"),
        "claimId": prompt, "expectedRevision": rev.to_string(), "reason": "x", "evidenceIds": [],
        "resolveChallengeId": challenge, "outcome": "Retire"}),
    ));
    assert_eq!(f.code, DomainErrorCode::PermissionDenied);
    receipt(fx.cmd(
        &user,
        "knowledge.challenge",
        json!({"scope": proj("project-a"),
        "claimId": prompt, "expectedRevision": rev.to_string(), "reason": "obsolete",
        "evidenceIds": [], "resolveChallengeId": challenge, "outcome": "Retire"}),
    ));
    let l = fx.claim(&user, proj("project-a"), &prompt).lesson.unwrap();
    assert_eq!(wire(&l.rule_state), "Retired");
    assert_eq!(wire(&l.challenges.0[0].state), "Retired");
    // Reject needs the user's reason; a rejected lesson never becomes effective by recurrence.
    let rejected = fx.claim_id(
        &a,
        proj("project-a"),
        "Bad",
        with(
            draft("Lesson", "skip tests"),
            json!({"lesson": lesson_draft("Agent", Some("b1"), None)}),
        ),
    );
    let rev = fx.revision(&rejected);
    let f = refused(fx.cmd(
        &user,
        "knowledge.promote",
        json!({"claimId": rejected,
        "expectedRevision": rev.to_string(), "destination": proj("project-a"), "action": "Reject"}),
    ));
    assert_eq!(f.code, DomainErrorCode::InvalidArgument);
    receipt(fx.cmd(
        &user,
        "knowledge.promote",
        json!({"claimId": rejected,
        "expectedRevision": rev.to_string(), "destination": proj("project-a"), "action": "Reject",
        "reason": "unsafe"}),
    ));
    for r in ["b2", "b3", "b4"] {
        fx.record(
            &a,
            proj("project-a"),
            json!({"entities": [], "evidence": [], "claims": [
            with(draft("Lesson", "skip"), json!({"subjectId": "x", "lesson":
                with(lesson_draft("Agent", Some(r), None), json!({"sameAs": rejected}))}))]}),
        );
    }
    assert_eq!(
        wire(
            &fx.claim(&user, proj("project-a"), &rejected)
                .lesson
                .unwrap()
                .rule_state
        ),
        "Rejected"
    );
}

#[test]
fn a_lower_threshold_applies_at_the_next_read_without_a_write() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", Some("project-a"));
    let lesson = fx.claim_id(
        &a,
        proj("project-a"),
        "Io",
        with(
            draft("Lesson", "close files"),
            json!({"lesson": lesson_draft("Agent", Some("e1"), None)}),
        ),
    );
    fx.record(
        &a,
        proj("project-a"),
        json!({"entities": [], "evidence": [], "claims": [
        with(draft("Lesson", "x"), json!({"subjectId": "x", "lesson":
            with(lesson_draft("Agent", Some("e2"), None), json!({"sameAs": lesson}))}))]}),
    );
    assert_eq!(
        wire(
            &fx.claim(&a, proj("project-a"), &lesson)
                .lesson
                .unwrap()
                .rule_state
        ),
        "Candidate"
    );
    assert!(
        fx.lessons(&a, proj("project-a"), json!({"ruleStates": ["Effective"]}))
            .is_empty()
    );
    fx.set_threshold(2);
    // The stored candidate is read against the current setting, in reads and in filters.
    let l = fx.claim(&a, proj("project-a"), &lesson).lesson.unwrap();
    assert_eq!(
        (wire(&l.rule_state), l.recurrence_threshold.get()),
        ("Effective".into(), 2)
    );
    let effective = fx.lessons(&a, proj("project-a"), json!({"ruleStates": ["Effective"]}));
    assert_eq!(effective.len(), 1);
}

#[test]
fn lesson_views_addresses_and_delivery_are_inspectable() {
    let fx = Fx::new();
    let user = fx.user();
    let rule = fx.claim_id(
        &user,
        proj("project-a"),
        "Fmt",
        with(
            draft("Lesson", "run fmt"),
            json!({"lesson": lesson_draft("Prompt", None, None)}),
        ),
    );
    let other = fx.claim_id(
        &user,
        proj("project-a"),
        "Lint",
        with(
            draft("Lesson", "run clippy"),
            json!({"lesson": lesson_draft("Prompt", None, None)}),
        ),
    );
    let names = |v: Vec<KnowledgeClaim>| -> BTreeSet<String> {
        v.iter().map(|c| c.id.as_str().to_owned()).collect()
    };
    assert_eq!(
        names(fx.lessons(&user, proj("project-a"), json!({"view": "Unaddressed"}))).len(),
        2
    );
    let rev = fx.revision(&rule);
    receipt(fx.cmd(
        &user,
        "knowledge.promote",
        json!({"claimId": rule,
        "expectedRevision": rev.to_string(), "destination": proj("project-a"), "action": "Address",
        "address": {"kind": "Test", "name": "fmt_check", "reference": "ci.yml"}}),
    ));
    assert_eq!(
        names(fx.lessons(&user, proj("project-a"), json!({"view": "Unaddressed"}))),
        BTreeSet::from([other.as_str().to_owned()])
    );
    let l = fx.claim(&user, proj("project-a"), &rule).lesson.unwrap();
    assert_eq!(l.addressed_by.0[0].name.0, "fmt_check");
    // Delivery to a session is recorded by the context pipeline (KNOW-06).
    fx.write(|tx| {
        tx.sessions().insert_session(&StoredSession {
            conversation_id: id("conversation-s1"),
            id: id("s1"),
            agent_id: actor("agent-a"),
            project_id: Some(project("project-a")),
            root_id: None,
            title: "Chat".into(),
            context_epoch: 0,
            revision: 1,
            created_by: actor("agent-a"),
            created_at_ms: 1,
            closed_at_ms: None,
            closed_by: None,
            next_sequence: 1,
            checkpoint_sequence: 0,
        })
    });
    let rev = fx.revision(&rule);
    fx.write({
        let (k, rule) = (fx.knowledge.clone(), rule.clone());
        move |tx| k.record_delivery(tx, &id("s1"), &[(rule, rev)])
    });
    let unknown = run_write(&*fx.store, WriteOptions::default(), {
        let (k, rule) = (fx.knowledge.clone(), rule.clone());
        move |tx| k.record_delivery(tx, &id("no-session"), &[(rule, 1)])
    });
    assert!(matches!(unknown, Err(StorageError::NotFound { .. })));
    // A project-A rule is never recorded as supplied to another project's session.
    fx.write(|tx| {
        tx.sessions().insert_session(&StoredSession {
            conversation_id: id("conversation-s2"),
            id: id("s2"),
            agent_id: actor("agent-b"),
            project_id: Some(project("project-b")),
            root_id: None,
            title: "Chat".into(),
            context_epoch: 0,
            revision: 1,
            created_by: actor("agent-b"),
            created_at_ms: 1,
            closed_at_ms: None,
            closed_by: None,
            next_sequence: 1,
            checkpoint_sequence: 0,
        })
    });
    let leaked = run_write(&*fx.store, WriteOptions::default(), {
        let (k, rule) = (fx.knowledge.clone(), rule.clone());
        move |tx| k.record_delivery(tx, &id("s2"), &[(rule, rev)])
    });
    assert!(matches!(leaked, Err(StorageError::NotFound { .. })));
    let l = fx.claim(&user, proj("project-a"), &rule).lesson.unwrap();
    assert_eq!(l.delivered_sessions.get(), 1);
    assert_eq!(
        names(fx.lessons(&user, proj("project-a"), json!({"view": "Undelivered"}))),
        BTreeSet::from([other.as_str().to_owned()])
    );
    assert_eq!(
        fx.lessons(
            &user,
            proj("project-a"),
            json!({"activity": "implementing"})
        )
        .len(),
        2
    );
    assert!(
        fx.lessons(&user, proj("project-a"), json!({"activity": "releasing"}))
            .is_empty()
    );
}

#[test]
fn same_as_merges_a_lesson_and_unmerge_restores_it() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", Some("project-a"));
    let keep = fx.claim_id(
        &a,
        proj("project-a"),
        "A",
        with(
            draft("Lesson", "pin versions"),
            json!({"lesson": lesson_draft("Agent", Some("k1"), None)}),
        ),
    );
    let dup = fx.claim_id(
        &a,
        proj("project-a"),
        "B",
        with(
            draft("Lesson", "pin all versions"),
            json!({"lesson": lesson_draft("Agent", Some("d1"), None)}),
        ),
    );
    receipt(fx.cmd(
        &a,
        "knowledge.relate",
        json!({"scope": proj("project-a"), "subjectId": dup,
        "predicate": "same_as", "objectId": keep, "evidenceIds": []}),
    ));
    assert_eq!(
        fx.claim(&a, proj("project-a"), &keep)
            .lesson
            .unwrap()
            .recurrence
            .get(),
        2
    );
    assert_eq!(
        wire(
            fx.claim(&a, proj("project-a"), &dup)
                .lifecycle
                .as_ref()
                .unwrap()
        ),
        "Superseded"
    );
    receipt(fx.cmd(
        &a,
        "knowledge.relate",
        json!({"scope": proj("project-a"), "subjectId": dup,
        "predicate": "unmerge", "objectId": keep, "evidenceIds": []}),
    ));
    assert_eq!(
        fx.claim(&a, proj("project-a"), &keep)
            .lesson
            .unwrap()
            .recurrence
            .get(),
        1
    );
    let d = fx.claim(&a, proj("project-a"), &dup);
    assert_eq!(wire(d.lifecycle.as_ref().unwrap()), "Active");
    assert_eq!(d.lesson.unwrap().recurrence.get(), 1);
}

#[test]
fn pages_cover_every_row_once_and_cursors_expire_with_the_scope() {
    let fx = Fx::new();
    let user = fx.user();
    for i in 0..7 {
        fx.fact(
            &user,
            proj("project-a"),
            &format!("T{i}"),
            draft("Fact", &format!("paged item {i}")),
        );
    }
    let body = |cursor: Option<&str>| {
        let mut page = json!({"limit": 3});
        if let Some(c) = cursor {
            page["cursor"] = json!(c);
        }
        json!({"scope": proj("project-a"), "text": "paged", "tags": [], "kinds": [], "page": page,
            "entityLimit": 50, "claimLimit": 50})
    };
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = match result(fx.q(&user, "knowledge.search", body(cursor.as_deref()))) {
            OperationResult::KnowledgeSearch { value } => value,
            other => panic!("{other:?}"),
        };
        seen.extend(ids(&page));
        match page.next_cursor {
            Some(c) => cursor = Some(c.0),
            None => break,
        }
    }
    assert_eq!(seen.len(), 7);
    assert_eq!(seen.iter().collect::<BTreeSet<_>>().len(), 7);
    // A cursor read before a change of its scope expires instead of skipping or repeating.
    let first = match result(fx.q(&user, "knowledge.search", body(None))) {
        OperationResult::KnowledgeSearch { value } => value.next_cursor.unwrap().0,
        other => panic!("{other:?}"),
    };
    fx.fact(&user, proj("project-a"), "T9", draft("Fact", "paged late"));
    let f = query_fault(fx.q(&user, "knowledge.search", body(Some(&first))));
    assert_eq!(f.code, DomainErrorCode::CursorExpired);
    // A change of general Knowledge (also read by a project page) expires it too.
    let first = match result(fx.q(&user, "knowledge.search", body(None))) {
        OperationResult::KnowledgeSearch { value } => value.next_cursor.unwrap().0,
        other => panic!("{other:?}"),
    };
    fx.fact(&user, ws(), "G", draft("Fact", "unrelated general"));
    let f = query_fault(fx.q(&user, "knowledge.search", body(Some(&first))));
    assert_eq!(f.code, DomainErrorCode::CursorExpired);
    // The page size is honoured at the repository maximum (500), and claimLimit caps it.
    let page = match result(fx.q(
        &user,
        "knowledge.search",
        json!({"scope": proj("project-a"),
        "text": "paged", "tags": [], "kinds": [], "page": {"limit": 500}, "entityLimit": 50,
        "claimLimit": 2}),
    )) {
        OperationResult::KnowledgeSearch { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(page.items.0.len(), 2);
    assert!(page.has_more);
}

#[test]
fn a_page_of_the_repository_maximum_continues_instead_of_truncating() {
    let fx = Fx::new();
    let user = fx.user();
    for batch in 0..4 {
        let claims: Vec<Value> = (0..126)
            .map(|i| draft("Fact", &format!("bulk row {batch} {i}")))
            .collect();
        receipt(fx.record(
            &user,
            proj("project-a"),
            json!({"entities": [{"clientId": "s",
            "label": "Bulk", "entityType": "topic", "aliases": []}], "evidence": [],
            "claims": claims}),
        ));
    }
    let body = |cursor: Option<&str>| {
        let mut page = json!({"limit": 500});
        if let Some(c) = cursor {
            page["cursor"] = json!(c);
        }
        json!({"scope": proj("project-a"), "text": "bulk", "tags": [], "kinds": [], "page": page,
            "entityLimit": 500, "claimLimit": 500})
    };
    let first = match result(fx.q(&user, "knowledge.search", body(None))) {
        OperationResult::KnowledgeSearch { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(first.items.0.len(), 500);
    assert!(
        first.has_more,
        "a full page of the maximum says more follow"
    );
    let second = match result(fx.q(
        &user,
        "knowledge.search",
        body(Some(&first.next_cursor.unwrap().0)),
    )) {
        OperationResult::KnowledgeSearch { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(second.items.0.len(), 4);
    assert!(!second.has_more);
    // The context seam reads every effective rule, beyond one repository page.
    let rules: Vec<Value> = (0..126)
        .map(|i| {
            with(
                draft("Lesson", &format!("rule {i}")),
                json!({"lesson": lesson_draft("Prompt", None, None)}),
            )
        })
        .collect();
    for _ in 0..5 {
        receipt(fx.record(
            &user,
            proj("project-a"),
            json!({"entities": [{"clientId": "s",
            "label": "Rules", "entityType": "topic", "aliases": []}], "evidence": [],
            "claims": rules.clone()}),
        ));
    }
    let all = fx.read(|s| fx.knowledge.rules_for(s, Some(&project("project-a")), None));
    assert_eq!(all.len(), 630);
}

#[test]
fn deletion_is_permissioned_previewed_and_keeps_provenance() {
    let fx = Fx::new();
    let user = fx.user();
    let a = fx.agent("agent-a", Some("project-a"));
    let batch = json!({
        "entities": [{"clientId": "s", "label": "Test run 7", "entityType": "run", "aliases": [],
            "source": "test-run-7"}],
        "evidence": [{"id": "shared", "source": {"kind": "Observation", "sourceRef": "log:7"},
            "snippet": "secret log line"}],
        "claims": [
            with(draft("Fact", "flaky secret one"), json!({"source": "test-run-7", "evidenceIds": ["shared"]})),
            with(draft("Fact", "flaky secret two"), json!({"source": "test-run-7"}))]});
    let r = receipt(fx.record(&a, proj("project-a"), batch));
    let (deleted, subject) = (r.claim_ids.0[0].clone(), r.entity_ids.0[0].clone());
    // A surviving claim from another source cites the same evidence.
    let evidence = fx.read_claim(&a, proj("project-a"), &deleted).evidence.0[0]
        .id
        .clone();
    let keep = receipt(fx.record(
        &user,
        proj("project-a"),
        json!({"entities": [], "evidence": [],
        "claims": [with(draft("Fact", "kept fact"), json!({"subjectId": subject,
            "evidenceIds": [evidence]}))]}),
    ))
    .claim_ids
    .0[0]
        .clone();
    let (d, scope_revision) = fx.diagnostics(
        &user,
        json!({"scope": proj("project-a"), "sourceId": "test-run-7"}),
    );
    let preview = d
        .iter()
        .find(|x| x.component.0 == "knowledge.preview")
        .unwrap();
    let digest = preview.id.as_str().to_owned();
    let body = |digest: &str, revision: i64| {
        json!({"scope": proj("project-a"), "sourceId": "test-run-7",
        "expectedScopeRevision": revision.to_string(), "previewDigest": digest})
    };
    // Agents cannot delete.
    let f = refused(fx.cmd(
        &a,
        "knowledge.delete_by_source",
        body(&digest, scope_revision),
    ));
    assert_eq!(f.code, DomainErrorCode::PermissionDenied);
    // A different preview, or a scope that moved since, is refused.
    let f = refused(fx.cmd(
        &user,
        "knowledge.delete_by_source",
        body(&"0".repeat(64), scope_revision),
    ));
    assert_eq!(f.code, DomainErrorCode::Conflict);
    let f = refused(fx.cmd(
        &user,
        "knowledge.delete_by_source",
        body(&digest, scope_revision - 1),
    ));
    assert_eq!(f.code, DomainErrorCode::RevisionConflict);
    let s = summary(fx.cmd(
        &user,
        "knowledge.delete_by_source",
        body(&digest, scope_revision),
    ));
    assert!(s.changed);
    assert!(s.revision.get() > scope_revision);
    // Tombstones keep identity and attribution; content is gone, also from history.
    let c = fx.claim(&user, proj("project-a"), &deleted);
    assert_eq!(wire(c.lifecycle.as_ref().unwrap()), "Deleted");
    assert!(c.value.is_none());
    assert_eq!(c.author_id.as_ref().unwrap().0.as_str(), "agent-a");
    let history = fx.read(|s| s.knowledge().knowledge_history(&deleted, 50));
    assert!(history.iter().all(|h| h.snapshot.get("value").is_none()));
    assert!(
        fx.search(
            &user,
            proj("project-a"),
            "secret",
            json!({"states": ["Active", "Retracted", "Superseded", "Deleted"]})
        )
        .items
        .0
        .is_empty()
    );
    // Independent surviving evidence and the entity still used are kept.
    let kept = fx.read_claim(&user, proj("project-a"), &keep);
    assert_eq!(kept.evidence.0[0].snippet.0, "secret log line");
    assert_eq!(kept.entities.0[0].label.0, "Test run 7");
    // Selected deletion (test data): exact revisions.
    let rev = fx.revision(&keep);
    let f = refused(fx.cmd(
        &user,
        "knowledge.delete_selected",
        json!({"scope": proj("project-a"),
        "claims": [{"id": keep, "revision": (rev + 1).to_string()}]}),
    ));
    assert_eq!(f.code, DomainErrorCode::RevisionConflict);
    let s = summary(fx.cmd(
        &user,
        "knowledge.delete_selected",
        json!({"scope": proj("project-a"),
        "claims": [{"id": keep, "revision": rev.to_string()}]}),
    ));
    assert!(s.changed);
    // The evidence no surviving claim cites is purged now.
    let e = fx
        .read(move |s| s.knowledge().knowledge_evidence(&evidence))
        .unwrap();
    assert!(e.deleted_at_ms.is_some() && e.snippet.is_empty());
    // A bundle never lists purged evidence, though the claim's IDs still name it.
    assert!(
        fx.read_claim(&user, proj("project-a"), &keep)
            .evidence
            .0
            .is_empty()
    );
}

#[test]
fn tags_replace_and_moves_follow_scope_rules() {
    let fx = Fx::new();
    let user = fx.user();
    let b = fx.agent("agent-b", Some("project-b"));
    let claim = fx.claim_id(
        &user,
        proj("project-a"),
        "Topic",
        draft("Fact", "movable fact"),
    );
    let rev = fx.revision(&claim);
    let body = |tags: Value, rev: i64| {
        json!({"scope": proj("project-a"), "claimIds": [claim],
        "expectedRevisions": [{"id": claim, "revision": rev.to_string()}], "tags": tags})
    };
    receipt(fx.cmd(
        &user,
        "knowledge.tag",
        body(json!(["Ops", "OPS", "infra"]), rev),
    ));
    let tags: Vec<String> = fx
        .claim(&user, proj("project-a"), &claim)
        .tags
        .0
        .iter()
        .map(|t| t.0.clone())
        .collect();
    assert_eq!(tags, ["infra", "ops"]);
    let rev = fx.revision(&claim);
    assert!(matches!(
        unchanged(fx.cmd(&user, "knowledge.tag", body(json!(["infra", "ops"]), rev))),
        OperationResult::KnowledgeTag { .. }
    ));
    // An entity another project-A claim still uses cannot move with this one.
    let subject = fx.claim(&user, proj("project-a"), &claim).subject_id;
    let pinned = receipt(fx.record(
        &user,
        proj("project-a"),
        json!({"entities": [], "evidence": [],
        "claims": [with(draft("Fact", "stays home"), json!({"subjectId": subject}))]}),
    ))
    .claim_ids
    .0[0]
        .clone();
    let f = refused(fx.cmd(
        &user,
        "knowledge.scope",
        json!({"claimIds": [{"id": claim,
        "revision": rev.to_string()}], "destination": ws()}),
    ));
    assert_eq!(f.code, DomainErrorCode::Conflict);
    // Moving both makes them general: project B sees them, under the same identities.
    let prev = fx.revision(&pinned);
    receipt(fx.cmd(
        &user,
        "knowledge.scope",
        json!({"claimIds": [{"id": claim,
        "revision": rev.to_string()}, {"id": pinned, "revision": prev.to_string()}],
        "destination": ws()}),
    ));
    let seen = ids(&fx.search(&b, proj("project-b"), "movable", json!({})));
    assert_eq!(seen, [claim.as_str()]);
    assert_eq!(
        fx.read_claim(&b, proj("project-b"), &claim).entities.0[0].id,
        subject
    );
    // A confined agent cannot move Knowledge it cannot change.
    let rev = fx.revision(&claim);
    let f = refused(fx.cmd(
        &b,
        "knowledge.scope",
        json!({"claimIds": [{"id": claim,
        "revision": rev.to_string()}], "destination": proj("project-b")}),
    ));
    assert_eq!(f.code, DomainErrorCode::NotFound);
}

#[test]
fn user_import_previews_exactly_and_stores_only_verified_candidates_once() {
    let fx = Fx::new();
    let user = fx.user();
    let bytes = serde_json::to_vec(&json!({"format": IMPORT_SCHEMA_ID, "candidates": [
        {"entry": {"kind": "decision", "summary": "Use SQLite", "subject": "Storage"},
            "sourceFile": "MEMORY.md", "verification": "verified"},
        {"entry": {"kind": "fact", "summary": "Editor is vim"}, "sourceFile": "MEMORY.md",
            "verification": "personal"},
        {"entry": {"kind": "lesson", "summary": "Run fmt", "activities": ["implementing"]},
            "sourceFile": "MEMORY.md", "verification": "unverifiable"},
        {"entry": {"kind": "gotcha", "summary": "Old path"}, "sourceFile": "MEMORY.md",
            "verification": "stale"}]}))
    .unwrap();
    let relative = "import.json";
    std::fs::write(fx.sources.root.location.join(relative), &bytes).unwrap();
    fx.sources
        .handles
        .lock()
        .unwrap()
        .insert("ticket-1".into(), relative.into());
    let frame = json!({"protocol": "1.0-draft.1", "requestKey": "save-1", "command": {
        "kind": "Save", "projectId": "project-a", "title": "Memory export",
        "mediaType": "application/json", "source": {"kind": "Upload", "ticketId": "ticket-1"},
        "expectedDigest": sha256_hex(&bytes), "expectedBytes": bytes.len().to_string()}});
    let reply = {
        let authority = fx.authority();
        let gateway = Gateway {
            authority: &authority,
            store: &*fx.store,
            ids: &*fx.h.ids,
        };
        fx.cases
            .command(
                &gateway,
                &user,
                "artifacts.save",
                frame.to_string().as_bytes(),
            )
            .expect("addressable")
    };
    let saved: ArtifactRecord = match applied(reply) {
        OperationResult::ArtifactsSave { value } => value,
        other => panic!("{other:?}"),
    };
    let source = serde_json::to_value(&saved.artifact).unwrap();
    let schema =
        json!({"schemaId": IMPORT_SCHEMA_ID, "revision": "1", "sha256": import_schema_digest()});
    let (d, _) = fx.diagnostics(
        &user,
        json!({"scope": proj("project-a"), "importSource": source,
        "importSchema": schema}),
    );
    let outcomes: Vec<&str> = d
        .iter()
        .filter(|x| x.component.0 == "knowledge.import")
        .map(|x| x.code.0.as_str())
        .collect();
    assert_eq!(
        outcomes,
        ["would_store", "personal", "would_store", "stale"]
    );
    let digest = d
        .iter()
        .find(|x| x.component.0 == "knowledge.preview")
        .unwrap()
        .id
        .as_str()
        .to_owned();
    let body = |digest: &str| {
        json!({"scope": proj("project-a"), "source": source, "schema": schema,
        "previewDigest": digest})
    };
    // An agent cannot import the user's data; a different preview is refused.
    let f = refused(fx.cmd(
        &fx.agent("agent-a", Some("project-a")),
        "knowledge.import_user_data",
        body(&digest),
    ));
    assert_eq!(f.code, DomainErrorCode::PermissionDenied);
    let f = refused(fx.cmd(&user, "knowledge.import_user_data", body(&"1".repeat(64))));
    assert_eq!(f.code, DomainErrorCode::Conflict);
    let bad_schema = json!({"scope": proj("project-a"), "source": source, "schema":
        {"schemaId": IMPORT_SCHEMA_ID, "revision": "1", "sha256": "0".repeat(64)}, "previewDigest": digest});
    assert_eq!(
        refused(fx.cmd(&user, "knowledge.import_user_data", bad_schema)).code,
        DomainErrorCode::SchemaMismatch
    );
    let r = receipt(fx.cmd(&user, "knowledge.import_user_data", body(&digest)));
    assert_eq!(r.claim_ids.0.len(), 2);
    let lesson = r
        .claim_ids
        .0
        .iter()
        .map(|c| fx.claim(&user, proj("project-a"), c))
        .find(|c| c.lesson.is_some())
        .unwrap();
    assert_eq!(wire(&lesson.lesson.unwrap().rule_state), "Candidate");
    // The identity map makes a second import of the same version a preview of nothing new.
    let (d, _) = fx.diagnostics(
        &user,
        json!({"scope": proj("project-a"), "importSource": source,
        "importSchema": schema}),
    );
    let outcomes: Vec<&str> = d
        .iter()
        .filter(|x| x.component.0 == "knowledge.import")
        .map(|x| x.code.0.as_str())
        .collect();
    assert_eq!(
        outcomes,
        ["already_imported", "personal", "already_imported", "stale"]
    );
    // The old preview no longer matches the store, so it cannot import twice.
    let f = refused(fx.cmd(&user, "knowledge.import_user_data", body(&digest)));
    assert_eq!(f.code, DomainErrorCode::Conflict);
}

/// Saves `bytes` as the user's artifact and returns its reference.
fn saved_import(fx: &Fx, user: &InvocationContext, bytes: &[u8], key: &str) -> Value {
    let relative = format!("{key}.json");
    std::fs::write(fx.sources.root.location.join(&relative), bytes).unwrap();
    fx.sources
        .handles
        .lock()
        .unwrap()
        .insert(format!("ticket-{key}"), relative);
    let frame = json!({"protocol": "1.0-draft.1", "requestKey": format!("save-{key}"),
        "command": {"kind": "Save", "projectId": "project-a", "title": "Memory export",
        "mediaType": "application/json",
        "source": {"kind": "Upload", "ticketId": format!("ticket-{key}")},
        "expectedDigest": sha256_hex(bytes), "expectedBytes": bytes.len().to_string()}});
    let reply = {
        let authority = fx.authority();
        let gateway = Gateway {
            authority: &authority,
            store: &*fx.store,
            ids: &*fx.h.ids,
        };
        fx.cases
            .command(
                &gateway,
                user,
                "artifacts.save",
                frame.to_string().as_bytes(),
            )
            .expect("addressable")
    };
    match applied(reply) {
        OperationResult::ArtifactsSave { value } => serde_json::to_value(&value.artifact).unwrap(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_import_of_every_candidate_its_format_allows_stores_and_names_them_all() {
    let fx = Fx::new();
    let user = fx.user();
    // A stale candidate first, then its verified twin: the twin is stored, not a duplicate.
    let mut candidates = vec![
        json!({"entry": {"kind": "fact", "summary": "Port 8080"}, "sourceFile": "a.md",
            "verification": "stale"}),
        json!({"entry": {"kind": "fact", "summary": "port 8080"}, "sourceFile": "b.md",
            "verification": "verified"}),
    ];
    candidates.extend((2..200).map(|i| {
        json!({"entry": {"kind": "fact", "summary": format!("Fact {i}")}, "sourceFile": "c.md",
            "verification": "verified"})
    }));
    let bytes =
        serde_json::to_vec(&json!({"format": IMPORT_SCHEMA_ID, "candidates": candidates})).unwrap();
    let source = saved_import(&fx, &user, &bytes, "bulk");
    let schema =
        json!({"schemaId": IMPORT_SCHEMA_ID, "revision": "1", "sha256": import_schema_digest()});
    let (d, _) = fx.diagnostics(
        &user,
        json!({"scope": proj("project-a"), "importSource": source, "importSchema": schema}),
    );
    let outcomes: Vec<&str> = d
        .iter()
        .filter(|x| x.component.0 == "knowledge.import")
        .map(|x| x.code.0.as_str())
        .collect();
    assert_eq!(outcomes.len(), 200);
    assert_eq!(&outcomes[..2], ["stale", "would_store"]);
    let digest = d
        .iter()
        .find(|x| x.component.0 == "knowledge.preview")
        .unwrap()
        .id
        .as_str()
        .to_owned();
    let r = receipt(fx.cmd(
        &user,
        "knowledge.import_user_data",
        json!({"scope": proj("project-a"), "source": source, "schema": schema,
        "previewDigest": digest}),
    ));
    assert_eq!(r.claim_count.unwrap().get(), 199);
    let (pages, _, ids) = all_receipt_ids(&fx, &user, r.write_id.as_str());
    assert_eq!((pages, ids.len()), (2, 199));
}

// ---------------------------------------------------------------------------------------------
// Codex round 1

/// Every text value of the store's Knowledge rows and snapshots (to prove content is gone).
fn all_text(fx: &Fx) -> String {
    fx.store
        .with_writer_connection(|c| {
            let mut out = String::new();
            for sql in [
                "SELECT snapshot_json FROM knowledge_history",
                "SELECT reason || coalesce(resolution,'') || evidence_json FROM knowledge_challenges",
                "SELECT name || coalesce(reference,'') FROM knowledge_addresses",
                "SELECT event_key || coalesce(source_ref,'') FROM knowledge_lesson_events",
                "SELECT body_json || coalesce(applicability_json,'') FROM knowledge_claims",
            ] {
                let mut st = c.prepare(sql).unwrap();
                let rows = st.query_map([], |r| r.get::<_, String>(0)).unwrap();
                for r in rows {
                    out.push_str(&r.unwrap());
                    out.push('\n');
                }
            }
            Ok(out)
        })
        .unwrap()
}

#[test]
fn deleting_a_lesson_scrubs_its_challenges_addresses_and_references_everywhere() {
    let fx = Fx::new();
    let user = fx.user();
    let lesson = fx.claim_id(
        &user,
        proj("project-a"),
        "Secret",
        with(
            draft("Lesson", "secret rule text"),
            json!({"lesson": with(lesson_draft("Prompt", Some("secret-prompt-ref"), None),
                json!({"applicability": {"activities": ["implementing"],
                    "paths": ["secret/path.rs"], "tools": ["secret-tool"],
                    "repositoryIds": []}}))}),
        ),
    );
    let rev = fx.revision(&lesson);
    receipt(fx.cmd(
        &user,
        "knowledge.challenge",
        json!({"scope": proj("project-a"),
        "claimId": lesson, "expectedRevision": rev.to_string(),
        "reason": "secret challenge reason", "evidenceIds": []}),
    ));
    let rev = fx.revision(&lesson);
    receipt(fx.cmd(
        &user,
        "knowledge.promote",
        json!({"claimId": lesson,
        "expectedRevision": rev.to_string(), "destination": proj("project-a"),
        "action": "Address", "address": {"kind": "Test", "name": "secret_address_name",
            "reference": "secret/address/ref"}}),
    ));
    assert!(all_text(&fx).contains("secret challenge reason"));
    let rev = fx.revision(&lesson);
    summary(fx.cmd(
        &user,
        "knowledge.delete_selected",
        json!({"scope": proj("project-a"),
        "claims": [{"id": lesson, "revision": rev.to_string()}]}),
    ));
    // The deleted claim reads with identity and attribution only.
    let c = fx.claim(&user, proj("project-a"), &lesson);
    let l = c.lesson.unwrap();
    assert!(l.challenges.0.is_empty() && l.addressed_by.0.is_empty());
    assert!(l.applicability.paths.0.is_empty() && l.source_ref.is_none());
    assert_eq!(c.author_id.unwrap().0.as_str(), "user-a");
    // No row or snapshot (including the deletion snapshot) keeps any of the content.
    let text = all_text(&fx);
    for secret in [
        "secret rule text",
        "secret challenge reason",
        "secret_address_name",
        "secret/address/ref",
        "secret-prompt-ref",
        "secret/path.rs",
        "secret-tool",
    ] {
        assert!(!text.contains(secret), "{secret} survived the deletion");
    }
    // The challenge keeps its identity, state and author; the event count is kept.
    let challenges = fx.read(|s| s.knowledge().knowledge_challenges(&lesson, 10));
    assert_eq!(challenges.len(), 1);
    assert_eq!(challenges[0].author.0.as_str(), "user-a");
    assert_eq!(
        fx.read(|s| s.knowledge().knowledge_lesson_event_count(&lesson)),
        1
    );
}

#[test]
fn nested_merges_unwind_one_level_at_a_time_without_losing_events() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", Some("project-a"));
    let lesson = |name: &str, event: &str| {
        fx.claim_id(
            &a,
            proj("project-a"),
            name,
            with(
                draft("Lesson", name),
                json!({"lesson": lesson_draft("Agent", Some(event), None)}),
            ),
        )
    };
    let (la, lb, lc) = (lesson("A", "ea"), lesson("B", "eb"), lesson("C", "ec"));
    let relate = |s: &Id, p: &str, o: &Id| {
        fx.cmd(
            &a,
            "knowledge.relate",
            json!({"scope": proj("project-a"), "subjectId": s,
            "predicate": p, "objectId": o, "evidenceIds": []}),
        )
    };
    let recurrence = |id: &Id| {
        fx.claim(&a, proj("project-a"), id)
            .lesson
            .unwrap()
            .recurrence
            .get()
    };
    receipt(relate(&la, "same_as", &lb));
    receipt(relate(&lb, "same_as", &lc));
    assert_eq!(recurrence(&lc), 3);
    // The inner merge cannot be unwound while its target is itself merged.
    assert_eq!(
        refused(relate(&la, "unmerge", &lb)).code,
        DomainErrorCode::Conflict
    );
    receipt(relate(&lb, "unmerge", &lc));
    assert_eq!(
        (recurrence(&lb), recurrence(&lc)),
        (2, 1),
        "B gets its own and A's events back"
    );
    receipt(relate(&la, "unmerge", &lb));
    assert_eq!(
        (recurrence(&la), recurrence(&lb), recurrence(&lc)),
        (1, 1, 1)
    );
}

#[test]
fn a_past_revision_reads_with_its_own_entities_and_evidence() {
    let fx = Fx::new();
    let user = fx.user();
    let r = receipt(fx.record(&user, proj("project-a"), json!({
        "entities": [{"clientId": "old", "label": "Old subject", "entityType": "t", "aliases": []},
            {"clientId": "new", "label": "New subject", "entityType": "t", "aliases": []}],
        "evidence": [{"id": "e1", "source": {"kind": "Observation", "sourceRef": "log:1"},
            "snippet": "first evidence"},
            {"id": "e2", "source": {"kind": "Observation", "sourceRef": "log:2"},
            "snippet": "second evidence"}],
        "claims": [with(draft("Fact", "first"), json!({"subjectId": "old", "evidenceIds": ["e1"]}))]})));
    let claim = r.claim_ids.0[0].clone();
    let bundle = fx.read_claim(&user, proj("project-a"), &claim);
    let old = bundle.entities.0[0].id.clone();
    let e1 = bundle.evidence.0[0].id.clone();
    let (new, e2) = {
        let others: Vec<Id> = r
            .entity_ids
            .0
            .iter()
            .filter(|e| **e != old)
            .cloned()
            .collect();
        let w = fx
            .read({
                let id = r.write_id.clone();
                move |s| s.knowledge().knowledge_write(&id)
            })
            .unwrap();
        let evidence: Vec<Id> = w
            .changes
            .iter()
            .filter(|c| c.target_kind == "Evidence" && c.id != e1)
            .map(|c| c.id.clone())
            .collect();
        (others[0].clone(), evidence[0].clone())
    };
    // A later entity revision (a new alias) is not part of the first revision's view.
    receipt(fx.record(
        &user,
        proj("project-a"),
        json!({"entities": [{"clientId": "o",
        "label": "Old subject", "entityType": "t", "aliases": ["later alias"]}],
        "evidence": [], "claims": []}),
    ));
    receipt(fx.cmd(
        &user,
        "knowledge.revise",
        json!({"scope": proj("project-a"), "claimId": claim,
        "expectedRevision": "1", "replacement": with(draft("Fact", "second"),
            json!({"subjectId": new, "evidenceIds": [e2]}))}),
    ));
    let past = match result(fx.q(
        &user,
        "knowledge.read",
        json!({"scope": proj("project-a"),
        "claimId": claim, "revision": "1"}),
    )) {
        OperationResult::KnowledgeRead { value } => value,
        other => panic!("{other:?}"),
    };
    let entity_ids: Vec<&Id> = past.entities.0.iter().map(|e| &e.id).collect();
    assert_eq!(
        entity_ids,
        [&old],
        "the revision's subject, not the current one"
    );
    assert!(
        past.entities.0[0].aliases.0.is_empty(),
        "the entity as it was then"
    );
    let evidence_ids: Vec<&Id> = past.evidence.0.iter().map(|e| &e.id).collect();
    assert_eq!(evidence_ids, [&e1]);
    assert!(past.relations.is_none() && past.conflicts.0.is_empty());
    let now = fx.read_claim(&user, proj("project-a"), &claim);
    assert_eq!(now.entities.0[0].id, new);
    assert_eq!(now.evidence.0[0].id, e2);
}

#[test]
fn an_observation_that_changes_staleness_expires_a_stale_filtered_cursor() {
    let fx = Fx::new();
    let user = fx.user();
    for i in 0..3 {
        receipt(fx.record(&user, proj("project-a"), json!({
            "entities": [{"clientId": "s", "label": "Watched", "entityType": "t", "aliases": []}],
            "evidence": [{"id": "e", "source": {"kind": "Code", "sourceRef": format!("repo:f{i}")},
                "snippet": "x", "sourceRevision": "c1"}],
            "claims": [with(draft("Fact", &format!("watched fact {i}")), json!({"evidenceIds": ["e"]}))]})));
    }
    let body = |cursor: Option<&str>| {
        let mut page = json!({"limit": 1});
        if let Some(c) = cursor {
            page["cursor"] = json!(c);
        }
        json!({"scope": proj("project-a"), "text": "watched", "tags": [], "kinds": [],
            "page": page, "entityLimit": 50, "claimLimit": 50, "stale": false})
    };
    let first = match result(fx.q(&user, "knowledge.search", body(None))) {
        OperationResult::KnowledgeSearch { value } => value.next_cursor.unwrap().0,
        other => panic!("{other:?}"),
    };
    // The same revision again changes nothing: the cursor still continues.
    fx.write({
        let k = fx.knowledge.clone();
        move |tx| k.observe_source(tx, "repo:f0", "c1")
    });
    assert!(matches!(
        fx.q(&user, "knowledge.search", body(Some(&first))),
        DomainQueryReply::Result { .. }
    ));
    let changed = fx.write({
        let k = fx.knowledge.clone();
        move |tx| k.observe_source(tx, "repo:f0", "c2")
    });
    assert_eq!(changed.len(), 1);
    let f = query_fault(fx.q(&user, "knowledge.search", body(Some(&first))));
    assert_eq!(f.code, DomainErrorCode::CursorExpired);
}

#[test]
fn another_projects_identical_relation_is_its_own_record() {
    let fx = Fx::new();
    let user = fx.user();
    let x = fx.claim_id(&user, ws(), "X", draft("Fact", "general x"));
    let y = fx.claim_id(&user, ws(), "Y", draft("Fact", "general y"));
    let relate = |p: &str| {
        fx.cmd(
            &user,
            "knowledge.relate",
            json!({"scope": proj(p), "subjectId": x,
            "predicate": "depends_on", "objectId": y, "evidenceIds": []}),
        )
    };
    receipt(relate("project-a"));
    // Project B's relation is recorded, not suppressed by project A's.
    receipt(relate("project-b"));
    assert!(matches!(
        decision(relate("project-b")),
        DomainDecision::NoChange { .. }
    ));
    let seen = |p: &str| fx.read_claim(&user, proj(p), &x).relations.unwrap().0.len();
    assert_eq!((seen("project-a"), seen("project-b")), (1, 1));
}

/// Runs a delete-by-source job's remaining batches, as its durable consumer's deliveries do;
/// returns how many ran.
fn run_deletion(fx: &Fx, write: &Id) -> usize {
    let mut batches = 0;
    loop {
        let (k, id) = (fx.knowledge.clone(), write.clone());
        let more = fx.write(move |tx| k.continue_source_deletion(tx, &id));
        batches += 1;
        if !more {
            return batches;
        }
    }
}

/// Every claim and entity ID a write's receipt names, across its write_status pages.
fn all_receipt_ids(fx: &Fx, ctx: &InvocationContext, write: &str) -> (usize, usize, Vec<String>) {
    let status = |cursor: Option<String>| {
        let mut body = json!({"writeId": write});
        if let Some(c) = cursor {
            body["cursor"] = json!(c);
        }
        match result(fx.q(ctx, "knowledge.write_status", body)) {
            OperationResult::KnowledgeWriteStatus { value } => value,
            other => panic!("{other:?}"),
        }
    };
    let first = status(None);
    let totals = (
        first.claim_count.as_ref().unwrap().get() as usize,
        first.entity_count.as_ref().unwrap().get() as usize,
    );
    let (mut claims, mut entities, mut pages) = (Vec::new(), Vec::new(), 0);
    let mut page = Some(first);
    while let Some(r) = page {
        pages += 1;
        assert!(r.claim_ids.0.len() <= 128 && r.entity_ids.0.len() <= 128);
        claims.extend(r.claim_ids.0.iter().map(|i| i.as_str().to_owned()));
        entities.extend(r.entity_ids.0.iter().map(|i| i.as_str().to_owned()));
        page = r.next_cursor.map(|c| status(Some(c.0)));
    }
    assert_eq!((claims.len(), entities.len()), totals, "every ID once");
    claims.sort();
    claims.dedup();
    assert_eq!(claims.len(), totals.0, "no ID twice");
    (pages, entities.len(), claims)
}

#[test]
fn a_write_with_more_notes_than_a_receipt_holds_keeps_them_all() {
    let fx = Fx::new();
    let user = fx.user();
    let topic = json!([{"clientId": "s", "label": "Topic", "entityType": "t", "aliases": []}]);
    receipt(fx.record(
        &user,
        proj("project-a"),
        json!({"entities": topic, "evidence": [], "claims": [draft("Fact", "base")]}),
    ));
    // The entity is reused (one note) and every claim conflicts with the base (128 notes).
    let claims: Vec<Value> = (0..128)
        .map(|i| draft("Fact", &format!("other {i}")))
        .collect();
    let r = receipt(fx.record(
        &user,
        proj("project-a"),
        json!({"entities": topic, "evidence": [], "claims": claims}),
    ));
    assert_eq!(r.diagnostics.0.len(), 128);
    let mut notes: Vec<String> = r
        .diagnostics
        .0
        .iter()
        .map(|d| d.id.as_str().to_owned())
        .collect();
    let mut cursor = r.next_cursor.map(|c| c.0);
    while let Some(c) = cursor {
        match result(fx.q(
            &user,
            "knowledge.write_status",
            json!({"writeId": r.write_id, "cursor": c}),
        )) {
            OperationResult::KnowledgeWriteStatus { value } => {
                notes.extend(value.diagnostics.0.iter().map(|d| d.id.as_str().to_owned()));
                cursor = value.next_cursor.map(|c| c.0);
            }
            other => panic!("{other:?}"),
        }
    }
    let count = notes.len();
    notes.sort();
    notes.dedup();
    assert_eq!((count, notes.len()), (129, 129));
}

#[test]
fn a_write_past_a_receipts_window_is_named_completely() {
    let fx = Fx::new();
    let user = fx.user();
    let target = fx.claim_id(&user, proj("project-a"), "T", draft("Fact", "target"));
    let claims: Vec<Value> = (0..128)
        .map(|i| draft("Fact", &format!("bulk {i}")))
        .collect();
    // 128 new claims plus the existing claim the evidence attaches to: 129 changed.
    let r = receipt(fx.record(
        &user,
        proj("project-a"),
        json!({
        "entities": [{"clientId": "s", "label": "Bulk", "entityType": "t", "aliases": []}],
        "evidence": [{"id": "e", "claimId": target, "source": {"kind": "Observation",
            "sourceRef": "log:x"}, "snippet": "attached"}],
        "claims": claims}),
    ));
    assert_eq!(r.claim_ids.0.len(), 128);
    assert_eq!(r.claim_count.unwrap().get(), 129);
    assert!(r.next_cursor.is_some());
    let (pages, _, ids) = all_receipt_ids(&fx, &user, r.write_id.as_str());
    assert_eq!(pages, 2);
    assert!(ids.contains(&target.as_str().to_owned()));
    // A cursor of another write, or an altered one, is not a continuation.
    let other = receipt(fx.record(
        &user,
        proj("project-a"),
        json!({"entities": [], "evidence": [], "claims": [with(draft("Fact", "one more"),
            json!({"subjectId": r.entity_ids.0[0]}))]}),
    ));
    let f = query_fault(fx.q(
        &user,
        "knowledge.write_status",
        json!({"writeId": other.write_id, "cursor": r.next_cursor.unwrap().0}),
    ));
    assert_eq!(f.code, DomainErrorCode::CursorExpired);
}

#[test]
fn deleting_a_source_past_a_receipts_window_deletes_and_names_all_of_it() {
    let fx = Fx::new();
    let user = fx.user();
    let mut recorded = Vec::new();
    for batch in 0..3 {
        let entities: Vec<Value> = (0..100)
            .map(|i| {
                json!({"clientId": format!("e{i}"), "label": format!("Bulk {batch}-{i}"),
                "entityType": "t", "aliases": [], "source": "bulk-run"})
            })
            .collect();
        let claims: Vec<Value> = (0..100)
            .map(|i| {
                with(
                    draft("Fact", &format!("bulk fact {batch}-{i}")),
                    json!({"subjectId": format!("e{i}"), "source": "bulk-run"}),
                )
            })
            .collect();
        let r = receipt(fx.record(
            &user,
            proj("project-a"),
            json!({"entities": entities, "evidence": [], "claims": claims}),
        ));
        recorded.extend(r.claim_ids.0.iter().map(|i| i.as_str().to_owned()));
    }
    let (d, scope_revision) = fx.diagnostics(
        &user,
        json!({"scope": proj("project-a"), "sourceId": "bulk-run"}),
    );
    let digest = d
        .iter()
        .find(|x| x.component.0 == "knowledge.preview")
        .unwrap()
        .id
        .as_str()
        .to_owned();
    let s = summary(fx.cmd(
        &user,
        "knowledge.delete_by_source",
        json!({"scope": proj("project-a"), "sourceId": "bulk-run",
        "expectedScopeRevision": scope_revision.to_string(), "previewDigest": digest}),
    ));
    assert!(s.changed);
    // 300 claims and 200 entities in the confirm; the last 100 entities in one more batch.
    assert_eq!(run_deletion(&fx, &s.id), 1);
    let (pages, entities, mut ids) = all_receipt_ids(&fx, &user, s.id.as_str());
    assert_eq!((pages, entities), (3, 300));
    recorded.sort();
    ids.sort();
    assert_eq!(ids, recorded, "the receipt names every deleted claim");
    for id in [&recorded[0], &recorded[299]] {
        let c = fx.claim(&user, proj("project-a"), &Id::new(id.clone()).unwrap());
        assert_eq!(wire(c.lifecycle.as_ref().unwrap()), "Deleted");
    }
}

#[test]
fn a_new_delivery_expires_an_undelivered_cursor() {
    let fx = Fx::new();
    let user = fx.user();
    let rules: Vec<Id> = (0..3)
        .map(|i| {
            fx.claim_id(
                &user,
                proj("project-a"),
                &format!("R{i}"),
                with(
                    draft("Lesson", &format!("rule {i}")),
                    json!({"lesson": lesson_draft("Prompt", None, None)}),
                ),
            )
        })
        .collect();
    fx.write(|tx| {
        tx.sessions().insert_session(&StoredSession {
            conversation_id: id("conversation-s1"),
            id: id("s1"),
            agent_id: actor("agent-a"),
            project_id: Some(project("project-a")),
            root_id: None,
            title: "Chat".into(),
            context_epoch: 0,
            revision: 1,
            created_by: actor("agent-a"),
            created_at_ms: 1,
            closed_at_ms: None,
            closed_by: None,
            next_sequence: 1,
            checkpoint_sequence: 0,
        })
    });
    let body = |cursor: Option<&str>| {
        let mut page = json!({"limit": 1});
        if let Some(c) = cursor {
            page["cursor"] = json!(c);
        }
        json!({"scope": proj("project-a"), "tags": ["lesson"], "view": "Undelivered", "page": page})
    };
    let cursor = || match result(fx.q(&user, "knowledge.tagged_lessons", body(None))) {
        OperationResult::KnowledgeTaggedLessons { value } => value.next_cursor.unwrap().0,
        other => panic!("{other:?}"),
    };
    let deliver = |rule: &Id| {
        let (rule, rev) = (rule.clone(), fx.revision(rule));
        fx.write({
            let k = fx.knowledge.clone();
            move |tx| k.record_delivery(tx, &id("s1"), &[(rule, rev)])
        });
    };
    let first = cursor();
    deliver(&rules[2]);
    // The view lost a member: continuing the old page would skip or repeat a lesson.
    let f = query_fault(fx.q(&user, "knowledge.tagged_lessons", body(Some(&first))));
    assert_eq!(f.code, DomainErrorCode::CursorExpired);
    // Recording the same delivery again changes nothing, so a fresh cursor still continues.
    let fresh = cursor();
    deliver(&rules[2]);
    assert!(matches!(
        fx.q(&user, "knowledge.tagged_lessons", body(Some(&fresh))),
        DomainQueryReply::Result { .. }
    ));
}

#[test]
fn a_deleted_claim_leaves_no_live_relation_in_another_bundle() {
    let fx = Fx::new();
    let user = fx.user();
    let x = fx.claim_id(&user, ws(), "X", draft("Fact", "general x"));
    let y = fx.claim_id(&user, ws(), "Y", draft("Fact", "general y"));
    receipt(fx.cmd(
        &user,
        "knowledge.relate",
        json!({"scope": ws(), "subjectId": x,
        "predicate": "depends_on", "objectId": y, "evidenceIds": []}),
    ));
    let related = || fx.read_claim(&user, ws(), &y).relations.unwrap().0.len();
    assert_eq!(related(), 1);
    let rev = fx.revision(&x);
    summary(fx.cmd(
        &user,
        "knowledge.delete_selected",
        json!({"scope": ws(), "claims": [{"id": x, "revision": rev.to_string()}]}),
    ));
    assert_eq!(related(), 0);
}

#[test]
fn an_unmerge_takes_back_only_the_citations_its_merge_added() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", Some("project-a"));
    let r = receipt(fx.record(
        &a,
        proj("project-a"),
        json!({
        "entities": [{"clientId": "s", "label": "Rules", "entityType": "t", "aliases": []}],
        "evidence": [
            {"id": "shared", "source": {"kind": "Observation", "sourceRef": "log:1"},
                "snippet": "cited by both"},
            {"id": "own", "source": {"kind": "Observation", "sourceRef": "log:2"},
                "snippet": "cited by A only"}],
        "claims": [
            with(draft("Lesson", "rule a"), json!({"evidenceIds": ["shared", "own"],
                "lesson": lesson_draft("Agent", Some("ea"), None)})),
            with(draft("Lesson", "rule b"), json!({"evidenceIds": ["shared"],
                "lesson": lesson_draft("Agent", Some("eb"), None)}))]}),
    ));
    let (la, lb) = (r.claim_ids.0[0].clone(), r.claim_ids.0[1].clone());
    let cited = |id: &Id| -> BTreeSet<String> {
        fx.read_claim(&a, proj("project-a"), id)
            .evidence
            .0
            .iter()
            .map(|e| e.snippet.0.clone())
            .collect()
    };
    let relate = |p: &str| {
        receipt(fx.cmd(
            &a,
            "knowledge.relate",
            json!({"scope": proj("project-a"), "subjectId": la,
            "predicate": p, "objectId": lb, "evidenceIds": []}),
        ))
    };
    relate("same_as");
    assert_eq!(cited(&lb).len(), 2);
    relate("unmerge");
    // B cited the shared evidence itself: the unmerge keeps it.
    assert_eq!(cited(&lb), BTreeSet::from(["cited by both".to_owned()]));
}

#[test]
fn another_projects_relations_never_use_a_bundles_relation_budget() {
    let fx = Fx::new();
    let user = fx.user();
    let x = fx.claim_id(&user, ws(), "X", draft("Fact", "general x"));
    let y = fx.claim_id(&user, ws(), "Y", draft("Fact", "general y"));
    let z = fx.claim_id(&user, ws(), "Z", draft("Fact", "general z"));
    let relate = |scope: Value, predicate: String, object: &Id| {
        receipt(fx.cmd(
            &user,
            "knowledge.relate",
            json!({"scope": scope, "subjectId": x,
            "predicate": predicate, "objectId": object, "evidenceIds": []}),
        ))
    };
    // Project B's relations come first and fill more than a bundle's budget.
    for i in 0..129 {
        relate(proj("project-b"), format!("depends_on_{i}"), &y);
    }
    relate(proj("project-a"), "contradicts".into(), &z);
    let b = fx.read_claim(&user, proj("project-a"), &x);
    assert_eq!(b.relations.unwrap().0.len(), 1);
    assert!(
        !b.truncated,
        "project B's rows are not project A's to count"
    );
    assert!(
        b.conflicts.0.iter().any(|c| c.claim_ids.0.contains(&z)),
        "project A's contradiction is shown"
    );
}

#[test]
fn a_source_whose_entity_and_evidence_have_many_users_deletes_them_too() {
    let fx = Fx::new();
    let user = fx.user();
    // More claims than a bounded user listing holds, all of one source, sharing one entity and
    // one evidence record.
    let first = receipt(fx.record(
        &user,
        proj("project-a"),
        json!({
        "entities": [{"clientId": "s", "label": "Busy secret entity", "entityType": "t",
            "aliases": [], "source": "big-run"}],
        "evidence": [{"id": "e", "source": {"kind": "Observation", "sourceRef": "log:big"},
            "snippet": "busy secret snippet"}],
        "claims": (0..128).map(|i| with(draft("Fact", &format!("big {i}")),
            json!({"source": "big-run", "evidenceIds": ["e"]}))).collect::<Vec<_>>()}),
    ));
    let entity = first.entity_ids.0[0].clone();
    let evidence = fx
        .read_claim(&user, proj("project-a"), &first.claim_ids.0[0])
        .evidence
        .0[0]
        .id
        .clone();
    for batch in 1..4 {
        let claims: Vec<Value> = (0..128)
            .map(|i| {
                with(
                    draft("Fact", &format!("big {batch}-{i}")),
                    json!({"subjectId": entity, "source": "big-run", "evidenceIds": [evidence]}),
                )
            })
            .collect();
        receipt(fx.record(
            &user,
            proj("project-a"),
            json!({"entities": [], "evidence": [], "claims": claims}),
        ));
    }
    let (d, scope_revision) = fx.diagnostics(
        &user,
        json!({"scope": proj("project-a"), "sourceId": "big-run"}),
    );
    let digest = d
        .iter()
        .find(|x| x.component.0 == "knowledge.preview")
        .unwrap()
        .id
        .as_str()
        .to_owned();
    let s = summary(fx.cmd(
        &user,
        "knowledge.delete_by_source",
        json!({"scope": proj("project-a"), "sourceId": "big-run",
        "expectedScopeRevision": scope_revision.to_string(), "previewDigest": digest}),
    ));
    run_deletion(&fx, &s.id);
    let text = all_text(&fx);
    for secret in ["Busy secret entity", "busy secret snippet"] {
        assert!(!text.contains(secret), "{secret} survived the deletion");
    }
    let e = fx
        .read(move |s| s.knowledge().knowledge_entity(&entity))
        .unwrap();
    assert!(e.deleted_at_ms.is_some());
    let ev = fx
        .read(move |s| s.knowledge().knowledge_evidence(&evidence))
        .unwrap();
    assert!(ev.deleted_at_ms.is_some() && ev.snippet.is_empty());
}

#[test]
fn a_shared_entity_or_evidence_another_source_uses_is_kept() {
    let fx = Fx::new();
    let user = fx.user();
    let r = receipt(fx.record(
        &user,
        proj("project-a"),
        json!({
        "entities": [{"clientId": "s", "label": "Shared entity", "entityType": "t",
            "aliases": [], "source": "run-1"}],
        "evidence": [{"id": "e", "source": {"kind": "Observation", "sourceRef": "log:s"},
            "snippet": "shared snippet"}],
        "claims": [
            with(draft("Fact", "from run 1"), json!({"source": "run-1", "evidenceIds": ["e"]})),
            // No source at all: still outside run-1.
            with(draft("Fact", "sourceless"), json!({"evidenceIds": ["e"]}))]}),
    ));
    let entity = r.entity_ids.0[0].clone();
    let (d, scope_revision) = fx.diagnostics(
        &user,
        json!({"scope": proj("project-a"), "sourceId": "run-1"}),
    );
    let digest = d
        .iter()
        .find(|x| x.component.0 == "knowledge.preview")
        .unwrap()
        .id
        .as_str()
        .to_owned();
    summary(fx.cmd(
        &user,
        "knowledge.delete_by_source",
        json!({"scope": proj("project-a"), "sourceId": "run-1",
        "expectedScopeRevision": scope_revision.to_string(), "previewDigest": digest}),
    ));
    let kept = fx.read_claim(&user, proj("project-a"), &r.claim_ids.0[1]);
    assert_eq!(kept.evidence.0[0].snippet.0, "shared snippet");
    assert_eq!(kept.entities.0[0].label.0, "Shared entity");
    let e = fx
        .read(move |s| s.knowledge().knowledge_entity(&entity))
        .unwrap();
    assert!(e.deleted_at_ms.is_none());
}

#[test]
fn a_bundle_shows_its_whole_chain_and_every_conflicting_claim_up_to_the_wire_bound() {
    let fx = Fx::new();
    let user = fx.user();
    let first = fx.claim_id(
        &user,
        proj("project-a"),
        "Chain",
        draft("Fact", "version 0"),
    );
    let subject = fx.claim(&user, proj("project-a"), &first).subject_id;
    let mut last = first.clone();
    for i in 1..=40 {
        last = receipt(fx.record(
            &user,
            proj("project-a"),
            json!({"entities": [], "evidence": [],
            "claims": [with(draft("Fact", &format!("version {i}")),
                json!({"subjectId": subject, "supersedes": last}))]}),
        ))
        .claim_ids
        .0[0]
            .clone();
    }
    let b = fx.read_claim(&user, proj("project-a"), &last);
    assert_eq!(b.supersession_ids.0.len(), 40);
    assert!(!b.truncated);
    // Twenty active claims state the same subject differently: the bundle carries them all.
    let other = fx.claim_id(&user, proj("project-a"), "Busy", draft("Fact", "state 0"));
    let busy = fx.claim(&user, proj("project-a"), &other).subject_id;
    for i in 1..20 {
        receipt(fx.record(
            &user,
            proj("project-a"),
            json!({"entities": [], "evidence": [],
            "claims": [with(draft("Fact", &format!("state {i}")), json!({"subjectId": busy}))]}),
        ));
    }
    let b = fx.read_claim(&user, proj("project-a"), &other);
    assert_eq!(b.conflicts.0[0].claim_ids.0.len(), 20);
    assert_eq!(b.claims.0.len(), 20);
}

#[test]
fn a_source_past_the_old_cap_is_deleted_whole_as_a_fenced_job() {
    let fx = Fx::new();
    let user = fx.user();
    // 10,100 claims, past the 10,000 the deletion used to refuse; one entity and one evidence
    // record per recorded batch.
    let mut recorded = Vec::new();
    for b in 0..79 {
        let n = if b == 78 { 116 } else { 128 };
        let claims: Vec<Value> = (0..n)
            .map(|i| {
                with(
                    draft("Fact", &format!("huge {b}-{i}")),
                    json!({"source": "huge", "evidenceIds": ["v"]}),
                )
            })
            .collect();
        let r = receipt(fx.record(
            &user,
            proj("project-a"),
            json!({
            "entities": [{"clientId": "s", "label": format!("Huge {b}"), "entityType": "t",
                "aliases": [], "source": "huge"}],
            "evidence": [{"id": "v", "source": {"kind": "Observation",
                "sourceRef": format!("log:{b}")}, "snippet": format!("huge snippet {b}")}],
            "claims": claims}),
        ));
        recorded.extend(r.claim_ids.0.iter().map(|i| i.as_str().to_owned()));
    }
    assert_eq!(recorded.len(), 10_100);
    let preview = |fx: &Fx| {
        let (d, revision) = fx.diagnostics(
            &user,
            json!({"scope": proj("project-a"), "sourceId": "huge"}),
        );
        let digest = d
            .iter()
            .find(|x| x.component.0 == "knowledge.preview")
            .unwrap()
            .id
            .as_str()
            .to_owned();
        json!({"scope": proj("project-a"), "sourceId": "huge",
            "expectedScopeRevision": revision.to_string(), "previewDigest": digest})
    };
    let s = summary(fx.cmd(&user, "knowledge.delete_by_source", preview(&fx)));
    assert!(s.changed);
    let status =
        |fx: &Fx| match result(fx.q(&user, "knowledge.write_status", json!({"writeId": s.id}))) {
            OperationResult::KnowledgeWriteStatus { value } => value,
            other => panic!("{other:?}"),
        };
    // The first batch committed with the confirm; the rest is a running job.
    let r = status(&fx);
    assert_eq!(wire(&r.state), "Dispatching");
    assert_eq!(r.claim_count.unwrap().get(), 500);
    // While it runs, its source is read-only: no new record, change or second deletion.
    let f = refused(fx.record(
        &user,
        proj("project-a"),
        json!({"entities": [{"clientId": "s", "label": "Late", "entityType": "t", "aliases": []}],
            "evidence": [], "claims": [with(draft("Fact", "late"), json!({"source": "huge"}))]}),
    ));
    assert_eq!(f.code, DomainErrorCode::Conflict);
    assert_eq!(f.target_kind.unwrap().0, "knowledge_source_deletion");
    let remaining = Id::new(recorded.last().unwrap().clone()).unwrap();
    let rev = fx.revision(&remaining);
    let f = refused(fx.cmd(
        &user,
        "knowledge.retract",
        json!({"scope": proj("project-a"), "claimId": remaining,
        "expectedRevision": rev.to_string(), "reason": "x", "evidenceIds": []}),
    ));
    assert_eq!(f.code, DomainErrorCode::Conflict);
    assert_eq!(
        refused(fx.cmd(&user, "knowledge.delete_by_source", preview(&fx))).code,
        DomainErrorCode::Conflict
    );
    // Twenty more batches finish it: every claim, entity and evidence record of the source.
    assert_eq!(run_deletion(&fx, &s.id), 20);
    let r = status(&fx);
    assert_eq!(wire(&r.state), "ObservedComplete");
    let (_, entities, mut ids) = all_receipt_ids(&fx, &user, s.id.as_str());
    recorded.sort();
    ids.sort();
    assert_eq!(ids, recorded, "the receipt names every deleted claim");
    assert_eq!(entities, 79);
    let text = all_text(&fx);
    assert!(!text.contains("huge snippet") && !text.contains("Huge 7"));
    // A finished (or replayed) job does nothing more, and the source is writable again.
    assert_eq!(run_deletion(&fx, &s.id), 1);
    receipt(fx.record(
        &user,
        proj("project-a"),
        json!({"entities": [{"clientId": "s", "label": "Again", "entityType": "t", "aliases": [],
            "source": "huge"}], "evidence": [],
            "claims": [with(draft("Fact", "again"), json!({"source": "huge"}))]}),
    ));
}

// ---------------------------------------------------------------------------------------------
// Codex round 3

/// Two lessons of `scope`: A cites `repo:merged` at `c1` (its basis), B cites nothing.
fn two_lessons(fx: &Fx, ctx: &InvocationContext, scope: Value) -> (Id, Id) {
    let r = receipt(fx.record(
        ctx,
        scope,
        json!({
        "entities": [{"clientId": "s", "label": "Rules", "entityType": "t", "aliases": []}],
        "evidence": [{"id": "e", "source": {"kind": "Code", "sourceRef": "repo:merged"},
            "snippet": "merged evidence", "sourceRevision": "c1"}],
        "claims": [
            with(draft("Lesson", "rule a"), json!({"evidenceIds": ["e"],
                "lesson": lesson_draft("Agent", Some("ea"), None)})),
            with(draft("Lesson", "rule b"),
                json!({"lesson": lesson_draft("Agent", Some("eb"), None)}))]}),
    ));
    (r.claim_ids.0[0].clone(), r.claim_ids.0[1].clone())
}

#[test]
fn a_merged_citation_brings_its_source_baseline_and_unmerge_takes_it_back() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", Some("project-a"));
    let (la, lb) = two_lessons(&fx, &a, proj("project-a"));
    let relate = |p: &str| {
        receipt(fx.cmd(
            &a,
            "knowledge.relate",
            json!({"scope": proj("project-a"), "subjectId": la,
            "predicate": p, "objectId": lb, "evidenceIds": []}),
        ))
    };
    let stale = |id: &Id| fx.claim(&a, proj("project-a"), id).stale;
    relate("same_as");
    // A newer revision of the merged citation's source makes the target stale too.
    fx.write({
        let k = fx.knowledge.clone();
        move |tx| k.observe_source(tx, "repo:merged", "c2")
    });
    assert_eq!(stale(&lb), Some(true));
    relate("unmerge");
    // The baseline came with the merge, so it leaves with it.
    assert_eq!(stale(&lb), Some(false));
    assert_eq!(stale(&la), Some(true));
}

#[test]
fn moving_merged_lessons_moves_their_merge_so_unmerge_still_reverses_it() {
    let fx = Fx::new();
    let user = fx.user();
    let (la, lb) = two_lessons(&fx, &user, proj("project-a"));
    receipt(fx.cmd(
        &user,
        "knowledge.relate",
        json!({"scope": proj("project-a"), "subjectId": la,
        "predicate": "same_as", "objectId": lb, "evidenceIds": []}),
    ));
    let (ra, rb) = (fx.revision(&la), fx.revision(&lb));
    receipt(fx.cmd(
        &user,
        "knowledge.scope",
        json!({"claimIds": [{"id": la, "revision": ra.to_string()},
            {"id": lb, "revision": rb.to_string()}], "destination": ws()}),
    ));
    receipt(fx.cmd(
        &user,
        "knowledge.relate",
        json!({"scope": ws(), "subjectId": la,
        "predicate": "unmerge", "objectId": lb, "evidenceIds": []}),
    ));
    // The merge's own citation leaves B; no merge relation stays live anywhere.
    let b = fx.read_claim(&user, ws(), &lb);
    assert!(b.evidence.0.is_empty(), "{:?}", b.evidence.0);
    assert!(b.relations.unwrap().0.is_empty());
    assert!(
        fx.read_claim(&user, proj("project-a"), &lb)
            .relations
            .unwrap()
            .0
            .is_empty()
    );
}

#[test]
fn evidence_a_surviving_relation_or_challenge_cites_outlives_a_deleted_source() {
    let fx = Fx::new();
    let user = fx.user();
    let delete = |source: &str| {
        let (d, revision) = fx.diagnostics(
            &user,
            json!({"scope": proj("project-a"), "sourceId": source}),
        );
        let digest = d
            .iter()
            .find(|x| x.component.0 == "knowledge.preview")
            .unwrap()
            .id
            .as_str()
            .to_owned();
        summary(fx.cmd(
            &user,
            "knowledge.delete_by_source",
            json!({"scope": proj("project-a"), "sourceId": source,
            "expectedScopeRevision": revision.to_string(), "previewDigest": digest}),
        ));
    };
    let cited = |source: &str, snippet: &str| -> Id {
        let r = receipt(fx.record(
            &user,
            proj("project-a"),
            json!({"entities": [{"clientId": "s", "label": source, "entityType": "t",
                "aliases": []}],
            "evidence": [{"id": "e", "source": {"kind": "Observation", "sourceRef": "log:z"},
                "snippet": snippet}],
            "claims": [with(draft("Fact", &format!("{source} fact")),
                json!({"source": source, "evidenceIds": ["e"]}))]}),
        ));
        fx.read_claim(&user, proj("project-a"), &r.claim_ids.0[0])
            .evidence
            .0[0]
            .id
            .clone()
    };
    let y = fx.claim_id(&user, proj("project-a"), "Y", draft("Fact", "y"));
    let z = fx.claim_id(&user, proj("project-a"), "Z", draft("Fact", "z"));
    let snippet = |e: &Id| {
        let e = e.clone();
        fx.read(move |s| s.knowledge().knowledge_evidence(&e))
            .unwrap()
            .snippet
    };
    // Cited by a deleted claim and by a relation between two surviving claims.
    let by_relation = cited("run-1", "relation evidence");
    receipt(fx.cmd(
        &user,
        "knowledge.relate",
        json!({"scope": proj("project-a"), "subjectId": y,
        "predicate": "depends_on", "objectId": z, "evidenceIds": [by_relation]}),
    ));
    delete("run-1");
    assert_eq!(snippet(&by_relation), "relation evidence");
    // Cited by a deleted claim and by a challenge of a surviving claim.
    let by_challenge = cited("run-2", "challenge evidence");
    let rev = fx.revision(&y);
    receipt(fx.cmd(
        &user,
        "knowledge.challenge",
        json!({"scope": proj("project-a"), "claimId": y, "expectedRevision": rev.to_string(),
        "reason": "doubt", "evidenceIds": [by_challenge]}),
    ));
    delete("run-2");
    assert_eq!(snippet(&by_challenge), "challenge evidence");
    // Cited only by the deleted claim: purged.
    let alone = cited("run-3", "alone evidence");
    delete("run-3");
    assert!(snippet(&alone).is_empty());
}

/// Previews and confirms a delete by source of project A; returns its write.
fn delete_source(fx: &Fx, ctx: &InvocationContext, source: &str) -> Id {
    let (d, revision) =
        fx.diagnostics(ctx, json!({"scope": proj("project-a"), "sourceId": source}));
    let digest = d
        .iter()
        .find(|x| x.component.0 == "knowledge.preview")
        .unwrap()
        .id
        .as_str()
        .to_owned();
    summary(fx.cmd(
        ctx,
        "knowledge.delete_by_source",
        json!({"scope": proj("project-a"), "sourceId": source,
        "expectedScopeRevision": revision.to_string(), "previewDigest": digest}),
    ))
    .id
}

#[test]
fn a_deletion_skips_entities_something_still_uses_without_revisiting_them() {
    let fx = Fx::new();
    let user = fx.user();
    // 600 entities of the source, each named by a claim of no source (so each is kept).
    for b in 0..5 {
        let entities: Vec<Value> = (0..120)
            .map(|i| {
                json!({"clientId": format!("e{i}"), "label": format!("Kept {b}-{i}"),
                "entityType": "t", "aliases": [], "source": "keep-run"})
            })
            .collect();
        let claims: Vec<Value> = (0..120)
            .map(|i| {
                with(
                    draft("Fact", &format!("kept {b}-{i}")),
                    json!({"subjectId": format!("e{i}")}),
                )
            })
            .collect();
        receipt(fx.record(
            &user,
            proj("project-a"),
            json!({"entities": entities, "evidence": [], "claims": claims}),
        ));
    }
    receipt(fx.record(
        &user,
        proj("project-a"),
        json!({"entities": [{"clientId": "s", "label": "Other", "entityType": "t",
            "aliases": []}], "evidence": [],
            "claims": [with(draft("Fact", "the source's"), json!({"source": "keep-run"}))]}),
    ));
    let write = delete_source(&fx, &user, "keep-run");
    // The confirm examined 499 entities; one more batch examines the last 101 and finishes.
    let mut batches = 0;
    loop {
        batches += 1;
        assert!(
            batches <= 3,
            "kept entities are skipped, never examined again"
        );
        let (k, id) = (fx.knowledge.clone(), write.clone());
        if !fx.write(move |tx| k.continue_source_deletion(tx, &id)) {
            break;
        }
    }
    assert!(
        fx.search(&user, proj("project-a"), "kept", json!({}))
            .items
            .0
            .len()
            >= 50
    );
}

#[test]
fn a_move_that_would_strand_a_relation_behind_is_refused() {
    let fx = Fx::new();
    let user = fx.user();
    let x = fx.claim_id(&user, proj("project-a"), "X", draft("Fact", "x"));
    let y = fx.claim_id(&user, proj("project-a"), "Y", draft("Fact", "y"));
    receipt(fx.cmd(
        &user,
        "knowledge.relate",
        json!({"scope": proj("project-a"), "subjectId": x,
        "predicate": "depends_on", "objectId": y, "evidenceIds": []}),
    ));
    // Project A could no longer see X in project B: the relation cannot stay, nor follow.
    let rev = fx.revision(&x);
    let f = refused(fx.cmd(
        &user,
        "knowledge.scope",
        json!({"claimIds": [{"id": x, "revision": rev.to_string()}],
            "destination": proj("project-b")}),
    ));
    assert_eq!(f.code, DomainErrorCode::Conflict);
    assert_eq!(f.target_kind.unwrap().0, "knowledge_relation");
    // To general Knowledge, which project A still sees, it may move.
    receipt(fx.cmd(
        &user,
        "knowledge.scope",
        json!({"claimIds": [{"id": x, "revision": rev.to_string()}], "destination": ws()}),
    ));
}

#[test]
fn an_entity_a_surviving_relation_names_outlives_a_deleted_source() {
    let fx = Fx::new();
    let user = fx.user();
    let r = receipt(fx.record(
        &user,
        proj("project-a"),
        json!({"entities": [{"clientId": "s", "label": "Related entity", "entityType": "t",
            "aliases": [], "source": "run-5"}], "evidence": [],
        "claims": [with(draft("Fact", "run five"), json!({"source": "run-5"}))]}),
    ));
    let entity = r.entity_ids.0[0].clone();
    let w = fx.claim_id(&user, proj("project-a"), "W", draft("Fact", "w"));
    receipt(fx.cmd(
        &user,
        "knowledge.relate",
        json!({"scope": proj("project-a"), "subjectId": w,
        "predicate": "about", "objectId": entity, "evidenceIds": []}),
    ));
    delete_source(&fx, &user, "run-5");
    let e = fx
        .read(move |s| s.knowledge().knowledge_entity(&entity))
        .unwrap();
    assert!(e.deleted_at_ms.is_none());
    assert_eq!(e.label, "Related entity");
}

// Codex round 4

/// Three lessons of project A: A and C cite `repo:merged` at `c1` (their basis), B cites
/// nothing. Returns (A, B, C).
fn three_lessons(fx: &Fx, ctx: &InvocationContext) -> (Id, Id, Id) {
    let r = receipt(fx.record(
        ctx,
        proj("project-a"),
        json!({
        "entities": [{"clientId": "s", "label": "Rules", "entityType": "t", "aliases": []}],
        "evidence": [{"id": "e", "source": {"kind": "Code", "sourceRef": "repo:merged"},
            "snippet": "merged evidence", "sourceRevision": "c1"}],
        "claims": [
            with(draft("Lesson", "rule a"), json!({"evidenceIds": ["e"],
                "lesson": lesson_draft("Agent", Some("ea"), None)})),
            with(draft("Lesson", "rule b"),
                json!({"lesson": lesson_draft("Agent", Some("eb"), None)})),
            with(draft("Lesson", "rule c"), json!({"evidenceIds": ["e"],
                "lesson": lesson_draft("Agent", Some("ec"), None)}))]}),
    ));
    (
        r.claim_ids.0[0].clone(),
        r.claim_ids.0[1].clone(),
        r.claim_ids.0[2].clone(),
    )
}

fn relate_in_a(fx: &Fx, ctx: &InvocationContext, subject: &Id, predicate: &str, object: &Id) {
    relate_with(fx, ctx, subject, predicate, object, &[]);
}

fn relate_with(
    fx: &Fx,
    ctx: &InvocationContext,
    subject: &Id,
    predicate: &str,
    object: &Id,
    evidence: &[&Id],
) {
    receipt(fx.cmd(
        ctx,
        "knowledge.relate",
        json!({"scope": proj("project-a"), "subjectId": subject,
        "predicate": predicate, "objectId": object, "evidenceIds": evidence}),
    ));
}

fn cited_snippets(fx: &Fx, ctx: &InvocationContext, claim: &Id) -> Vec<String> {
    fx.read_claim(ctx, proj("project-a"), claim)
        .evidence
        .0
        .iter()
        .map(|e| e.snippet.0.clone())
        .collect()
}

fn stored_snippet(fx: &Fx, evidence: &Id) -> String {
    let e = evidence.clone();
    fx.read(move |s| s.knowledge().knowledge_evidence(&e))
        .unwrap()
        .snippet
}

fn observe_merged_source(fx: &Fx) {
    fx.write({
        let k = fx.knowledge.clone();
        move |tx| k.observe_source(tx, "repo:merged", "c2")
    });
}

fn delete_claim(fx: &Fx, ctx: &InvocationContext, claim: &Id) {
    let rev = fx.revision(claim);
    summary(fx.cmd(
        ctx,
        "knowledge.delete_selected",
        json!({"scope": proj("project-a"),
        "claims": [{"id": claim, "revision": rev.to_string()}]}),
    ));
}

#[test]
fn an_unmerge_keeps_what_another_active_merge_into_the_target_still_needs() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", Some("project-a"));
    let (la, lb, lc) = three_lessons(&fx, &a);
    relate_in_a(&fx, &a, &la, "same_as", &lb);
    // C cites what B holds only through A's merge: C's merge needs it as well.
    relate_in_a(&fx, &a, &lc, "same_as", &lb);
    relate_in_a(&fx, &a, &la, "unmerge", &lb);
    assert_eq!(cited_snippets(&fx, &a, &lb), vec!["merged evidence"]);
    // ... and its source baseline: a newer revision still marks B stale.
    observe_merged_source(&fx);
    assert_eq!(fx.claim(&a, proj("project-a"), &lb).stale, Some(true));
    // Once no merge needs them, the last unmerge takes both back.
    relate_in_a(&fx, &a, &lc, "unmerge", &lb);
    assert!(cited_snippets(&fx, &a, &lb).is_empty());
    assert_eq!(fx.claim(&a, proj("project-a"), &lb).stale, Some(false));
}

#[test]
fn a_deleted_merges_citations_stay_with_the_target_when_another_merge_unmerges() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", Some("project-a"));
    let user = fx.user();
    let (la, lb, lc) = three_lessons(&fx, &a);
    relate_in_a(&fx, &a, &la, "same_as", &lb);
    relate_in_a(&fx, &a, &lc, "same_as", &lb);
    // Deleting A never undoes its merge (its events stay with B): what it brought is B's own
    // now, exactly as when no other merge shared it.
    delete_claim(&fx, &user, &la);
    relate_in_a(&fx, &a, &lc, "unmerge", &lb);
    assert_eq!(cited_snippets(&fx, &a, &lb), vec!["merged evidence"]);
    observe_merged_source(&fx);
    assert_eq!(fx.claim(&a, proj("project-a"), &lb).stale, Some(true));
}

#[test]
fn deleting_a_claim_purges_evidence_only_its_challenges_or_relations_cite() {
    let fx = Fx::new();
    let user = fx.user();
    let r = receipt(fx.record(
        &user,
        proj("project-a"),
        json!({
        "entities": [{"clientId": "s", "label": "Holder", "entityType": "t", "aliases": []}],
        "evidence": [
            {"id": "c", "source": {"kind": "Observation", "sourceRef": "log:c"},
                "snippet": "challenge only evidence"},
            {"id": "r", "source": {"kind": "Observation", "sourceRef": "log:r"},
                "snippet": "relation only evidence"}],
        "claims": [with(draft("Fact", "holder"), json!({"evidenceIds": ["c", "r"]}))]}),
    ));
    let holder = r.claim_ids.0[0].clone();
    let evidence: HashMap<String, Id> = fx
        .read_claim(&user, proj("project-a"), &holder)
        .evidence
        .0
        .iter()
        .map(|e| (e.snippet.0.clone(), e.id.clone()))
        .collect();
    let vc = evidence["challenge only evidence"].clone();
    let vr = evidence["relation only evidence"].clone();
    let x = fx.claim_id(&user, proj("project-a"), "X", draft("Fact", "x"));
    let y = fx.claim_id(&user, proj("project-a"), "Y", draft("Fact", "y"));
    let rev = fx.revision(&x);
    receipt(fx.cmd(
        &user,
        "knowledge.challenge",
        json!({"scope": proj("project-a"), "claimId": x, "expectedRevision": rev.to_string(),
        "reason": "doubt", "evidenceIds": [vc]}),
    ));
    relate_with(&fx, &user, &x, "depends_on", &y, &[&vr]);
    // X's challenge and relation still cite both: they outlive the claim that recorded them.
    delete_claim(&fx, &user, &holder);
    assert_eq!(stored_snippet(&fx, &vc), "challenge only evidence");
    assert_eq!(stored_snippet(&fx, &vr), "relation only evidence");
    // Deleting X empties its challenge's citations and removes its relation: nothing cites
    // either any more.
    delete_claim(&fx, &user, &x);
    assert!(stored_snippet(&fx, &vc).is_empty());
    assert!(stored_snippet(&fx, &vr).is_empty());
}

/// The preview message of deleting `source` from project A.
fn preview_message(fx: &Fx, ctx: &InvocationContext, source: &str) -> String {
    let (d, _) = fx.diagnostics(ctx, json!({"scope": proj("project-a"), "sourceId": source}));
    d.iter()
        .find(|x| x.component.0 == "knowledge.preview")
        .unwrap()
        .message
        .0
        .clone()
}

/// Entities `S` and `T` of source `run-9` and a sourceless claim citing one evidence record
/// (its deletion leaves the record cited only by relations). Returns (S, T, evidence, claim).
fn source_entities_and_evidence(fx: &Fx, ctx: &InvocationContext) -> (Id, Id, Id, Id) {
    let r = receipt(fx.record(
        ctx,
        proj("project-a"),
        json!({
        "entities": [
            {"clientId": "s", "label": "Run nine S", "entityType": "t", "aliases": [],
                "source": "run-9"},
            {"clientId": "t", "label": "Run nine T", "entityType": "t", "aliases": [],
                "source": "run-9"},
            {"clientId": "k", "label": "Keeper", "entityType": "t", "aliases": []}],
        "evidence": [{"id": "v", "source": {"kind": "Observation", "sourceRef": "log:v"},
            "snippet": "entity relation evidence"}],
        "claims": [with(draft("Fact", "keeper"),
            json!({"subjectId": "k", "evidenceIds": ["v"]}))]}),
    ));
    let holder = r.claim_ids.0[0].clone();
    let v = fx.read_claim(ctx, proj("project-a"), &holder).evidence.0[0]
        .id
        .clone();
    (
        r.entity_ids.0[0].clone(),
        r.entity_ids.0[1].clone(),
        v,
        holder,
    )
}

#[test]
fn deleting_a_source_purges_evidence_only_its_entities_relations_cite_as_previewed() {
    let fx = Fx::new();
    let user = fx.user();
    let (s, t, v, holder) = source_entities_and_evidence(&fx, &user);
    relate_with(&fx, &user, &s, "about", &t, &[&v]);
    delete_claim(&fx, &user, &holder);
    assert_eq!(stored_snippet(&fx, &v), "entity relation evidence");
    // The preview counts the evidence only the source's removed relation cites, and the
    // confirmed deletion purges exactly that.
    let message = preview_message(&fx, &user, "run-9");
    assert!(
        message.contains("0 claims, 2 entities and 1 evidence records"),
        "{message}"
    );
    delete_source(&fx, &user, "run-9");
    assert!(stored_snippet(&fx, &v).is_empty());
}

#[test]
fn evidence_a_kept_source_entitys_relation_cites_outlives_the_deletion() {
    let fx = Fx::new();
    let user = fx.user();
    let (s, x, v, holder) = source_entities_and_evidence(&fx, &user);
    let y = fx.claim_id(&user, proj("project-a"), "Y", draft("Fact", "y"));
    // S goes with its source; X stays (surviving Y relates to it), and so does X->Y.
    relate_with(&fx, &user, &s, "about", &x, &[&v]);
    relate_with(&fx, &user, &x, "about", &y, &[&v]);
    delete_claim(&fx, &user, &holder);
    let message = preview_message(&fx, &user, "run-9");
    assert!(
        message.contains("0 claims, 1 entities and 0 evidence records"),
        "{message}"
    );
    delete_source(&fx, &user, "run-9");
    assert_eq!(stored_snippet(&fx, &v), "entity relation evidence");
    let kept = fx
        .read(move |s| s.knowledge().knowledge_entity(&x))
        .unwrap();
    assert!(kept.deleted_at_ms.is_none());
}

// Codex round 5

/// A lesson of project A from `source` with the source event `event`.
fn lesson_in_a(fx: &Fx, ctx: &InvocationContext, name: &str, source: &str, event: &str) -> Id {
    fx.claim_id(
        ctx,
        proj("project-a"),
        name,
        with(
            draft("Lesson", name),
            json!({"lesson": lesson_draft(source, Some(event), None)}),
        ),
    )
}

/// Records another source event of `lesson` (a `same_as` recurrence).
fn recur_in_a(fx: &Fx, ctx: &InvocationContext, lesson: &Id, source: &str, event: &str) {
    receipt(fx.record(
        ctx,
        proj("project-a"),
        json!({"entities": [], "evidence": [], "claims": [
        with(draft("Lesson", "again"), json!({"subjectId": "unused-entity", "lesson":
            with(lesson_draft(source, Some(event), None), json!({"sameAs": lesson}))}))]}),
    ));
}

/// (rule state, effect, distinct events) of a lesson as read.
fn rule(fx: &Fx, ctx: &InvocationContext, lesson: &Id) -> (String, Option<String>, u64) {
    let l = fx.claim(ctx, proj("project-a"), lesson).lesson.unwrap();
    (
        wire(&l.rule_state),
        l.effect.as_ref().map(wire),
        l.recurrence.get() as u64,
    )
}

fn effective(effect: &str, events: u64) -> (String, Option<String>, u64) {
    ("Effective".to_owned(), Some(effect.to_owned()), events)
}

#[test]
fn an_unmerge_leaves_the_target_an_event_another_merge_also_brought() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", Some("project-a"));
    // A and C recurred in the same source event; B has two events of its own.
    let la = lesson_in_a(&fx, &a, "A", "Agent", "shared");
    let lc = lesson_in_a(&fx, &a, "C", "Agent", "shared");
    let lb = lesson_in_a(&fx, &a, "B", "Agent", "eb1");
    recur_in_a(&fx, &a, &lb, "Agent", "eb2");
    relate_in_a(&fx, &a, &la, "same_as", &lb);
    assert_eq!(rule(&fx, &a, &lb), effective("Recurrence", 3));
    relate_in_a(&fx, &a, &lc, "same_as", &lb);
    relate_in_a(&fx, &a, &la, "unmerge", &lb);
    // C is still merged and brought the shared event too: B keeps it, and its rule.
    assert_eq!(rule(&fx, &a, &lb), effective("Recurrence", 3));
    assert_eq!(rule(&fx, &a, &la).2, 1);
    relate_in_a(&fx, &a, &lc, "unmerge", &lb);
    assert_eq!(rule(&fx, &a, &lb), ("Candidate".into(), None, 2));
    assert_eq!(rule(&fx, &a, &lc).2, 1);
}

#[test]
fn a_deleted_merges_shared_event_stays_with_the_target_when_another_merge_unmerges() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", Some("project-a"));
    let user = fx.user();
    let la = lesson_in_a(&fx, &a, "A", "Agent", "shared");
    let lc = lesson_in_a(&fx, &a, "C", "Agent", "shared");
    let lb = lesson_in_a(&fx, &a, "B", "Agent", "eb1");
    recur_in_a(&fx, &a, &lb, "Agent", "eb2");
    relate_in_a(&fx, &a, &la, "same_as", &lb);
    relate_in_a(&fx, &a, &lc, "same_as", &lb);
    // Deleting C never undoes its merge: the event it shared with A is B's for good.
    delete_claim(&fx, &user, &lc);
    relate_in_a(&fx, &a, &la, "unmerge", &lb);
    assert_eq!(rule(&fx, &a, &lb), effective("Recurrence", 3));
    assert_eq!(rule(&fx, &a, &la).2, 1, "A gets its own event back");
}

#[test]
fn a_merged_user_rule_keeps_its_target_in_effect_until_the_last_carrying_merge_is_undone() {
    let fx = Fx::new();
    let user = fx.user();
    let a = fx.agent("agent-a", Some("project-a"));
    let la = lesson_in_a(&fx, &user, "A", "Prompt", "p1");
    let lc = lesson_in_a(&fx, &user, "C", "Review", "r1");
    let lb = lesson_in_a(&fx, &a, "B", "Agent", "eb1");
    relate_in_a(&fx, &user, &la, "same_as", &lb);
    // The user's own words take effect at once, merged or not (KNOW-05).
    assert_eq!(rule(&fx, &a, &lb), effective("UserInstruction", 2));
    // The rules served to a project's agents.
    let in_effect = |fx: &Fx| -> Vec<Id> {
        let k = fx.knowledge.clone();
        fx.read(move |s| k.rules_for(s, Some(&project("project-a")), None))
            .into_iter()
            .map(|c| c.id)
            .collect()
    };
    assert!(in_effect(&fx).contains(&lb));
    relate_in_a(&fx, &user, &lc, "same_as", &lb);
    relate_in_a(&fx, &user, &la, "unmerge", &lb);
    // C carries the user's rule as well.
    assert_eq!(rule(&fx, &a, &lb), effective("UserInstruction", 2));
    assert_eq!(rule(&fx, &a, &la), effective("UserInstruction", 1));
    relate_in_a(&fx, &user, &lc, "unmerge", &lb);
    // The last one undone restores B's own rule.
    assert_eq!(rule(&fx, &a, &lb), ("Candidate".into(), None, 1));
    assert!(!in_effect(&fx).contains(&lb));
}

#[test]
fn a_deleted_user_rules_merge_keeps_its_target_in_effect() {
    let fx = Fx::new();
    let user = fx.user();
    let a = fx.agent("agent-a", Some("project-a"));
    let la = lesson_in_a(&fx, &user, "A", "Prompt", "p1");
    let lc = lesson_in_a(&fx, &a, "C", "Agent", "ec");
    let lb = lesson_in_a(&fx, &a, "B", "Agent", "eb1");
    relate_in_a(&fx, &user, &la, "same_as", &lb);
    relate_in_a(&fx, &user, &lc, "same_as", &lb);
    // Deleting A makes its merge permanent, and the user's rule it carried B's own.
    delete_claim(&fx, &user, &la);
    relate_in_a(&fx, &user, &lc, "unmerge", &lb);
    assert_eq!(rule(&fx, &a, &lb), effective("UserInstruction", 2));
}

#[test]
fn the_users_own_act_keeps_a_rule_in_effect_by_recurrence_or_merge() {
    let fx = Fx::new();
    let user = fx.user();
    let a = fx.agent("agent-a", Some("project-a"));
    // In effect by recurrence through a merge, then promoted by the user.
    let la = lesson_in_a(&fx, &a, "A", "Agent", "ea");
    let lb = lesson_in_a(&fx, &a, "B", "Agent", "eb1");
    recur_in_a(&fx, &a, &lb, "Agent", "eb2");
    relate_in_a(&fx, &a, &la, "same_as", &lb);
    assert_eq!(rule(&fx, &a, &lb), effective("Recurrence", 3));
    let rev = fx.revision(&lb);
    receipt(fx.cmd(
        &user,
        "knowledge.promote",
        json!({"claimId": lb, "expectedRevision": rev.to_string(),
        "destination": proj("project-a")}),
    ));
    relate_in_a(&fx, &a, &la, "unmerge", &lb);
    assert_eq!(rule(&fx, &a, &lb), effective("Promoted", 2));
    // In effect by a merged user rule, then stated by the user themself.
    let lx = lesson_in_a(&fx, &user, "X", "Prompt", "p1");
    let ly = lesson_in_a(&fx, &a, "Y", "Agent", "ey");
    relate_in_a(&fx, &user, &lx, "same_as", &ly);
    recur_in_a(&fx, &user, &ly, "Prompt", "p2");
    relate_in_a(&fx, &user, &lx, "unmerge", &ly);
    assert_eq!(rule(&fx, &a, &ly), effective("UserInstruction", 2));
}

#[test]
fn search_words_may_split_between_a_claim_and_its_subject() {
    let fx = Fx::new();
    let user = fx.user();
    let claim = fx.claim_id(
        &user,
        proj("project-a"),
        "Router",
        draft("Fact", "tiers rotate"),
    );
    let found = |scope: Value, text: &str| ids(&fx.search(&user, scope, text, json!({})));
    assert_eq!(found(proj("project-a"), "router tiers"), [claim.as_str()]);
    assert_eq!(found(proj("project-a"), "tiers router"), [claim.as_str()]);
    // Every word is still required, in one or the other.
    assert!(found(proj("project-a"), "router pins").is_empty());
    // ... and scope still applies to the claim and to its subject.
    assert!(found(proj("project-b"), "router tiers").is_empty());
    let general = fx.claim_id(&user, ws(), "Gateway", draft("Fact", "routes retry"));
    assert_eq!(
        found(proj("project-b"), "gateway retry"),
        [general.as_str()]
    );
}

#[test]
fn a_move_that_would_strand_a_claim_naming_the_moved_claim_is_refused() {
    let fx = Fx::new();
    let user = fx.user();
    let g = fx.claim_id(&user, ws(), "G", draft("Fact", "g"));
    let c = fx.claim_id(
        &user,
        ws(),
        "C",
        with(draft("Fact", "c"), json!({"objectId": g})),
    );
    let move_to = |claims: &[&Id], dest: Value| {
        let refs: Vec<Value> = claims
            .iter()
            .map(|id| json!({"id": id, "revision": fx.revision(id).to_string()}))
            .collect();
        fx.cmd(
            &user,
            "knowledge.scope",
            json!({"claimIds": refs, "destination": dest}),
        )
    };
    // General C could no longer see G in project A.
    let f = refused(move_to(&[&g], proj("project-a")));
    assert_eq!(f.code, DomainErrorCode::Conflict);
    assert_eq!(f.target_id.as_ref(), Some(&c));
    // A supersession link names both claims: the superseded one stays general.
    let subject = fx.claim(&user, ws(), &g).subject_id;
    let h = receipt(fx.record(
        &user,
        ws(),
        json!({"entities": [], "evidence": [], "claims": [with(draft("Fact", "h"),
            json!({"subjectId": subject, "supersedes": g}))]}),
    ))
    .claim_ids
    .0[0]
        .clone();
    let f = refused(move_to(&[&h], proj("project-a")));
    assert_eq!(f.code, DomainErrorCode::Conflict);
    assert_eq!(f.target_id.as_ref(), Some(&g));
    // Moving them together keeps every reference visible.
    receipt(move_to(&[&g, &c, &h], proj("project-a")));
}

#[test]
fn attaching_newer_evidence_never_restores_a_stale_verification() {
    let fx = Fx::new();
    let user = fx.user();
    let a = fx.agent("agent-a", Some("project-a"));
    let r = receipt(fx.record(
        &user,
        proj("project-a"),
        json!({
        "entities": [{"clientId": "s", "label": "Router", "entityType": "c", "aliases": []}],
        "evidence": [{"id": "ev", "source": {"kind": "Code", "sourceRef": "repo:a/router.rs"},
            "snippet": "x", "sourceRevision": "c1"}],
        "claims": [
            with(draft("Fact", "tiers rotate"), json!({"evidenceIds": ["ev"]})),
            with(draft("Lesson", "pin tiers"), json!({"evidenceIds": ["ev"],
                "lesson": lesson_draft("Agent", Some("l1"), None)}))]}),
    ));
    let (claim, lesson) = (r.claim_ids.0[0].clone(), r.claim_ids.0[1].clone());
    for id in [&claim, &lesson] {
        receipt(fx.cmd(
            &user,
            "knowledge.verify",
            json!({"scope": proj("project-a"), "claimId": id,
            "expectedRevision": fx.revision(id).to_string(), "reason": "checked",
            "evidenceIds": [], "sourceRevision": "c1"}),
        ));
    }
    fx.write({
        let k = fx.knowledge.clone();
        move |tx| k.observe_source(tx, "repo:a/router.rs", "c2")
    });
    // Evidence from the new revision, attached without a verification ...
    receipt(fx.record(
        &a,
        proj("project-a"),
        json!({"entities": [], "claims": [], "evidence": [{"id": "n",
            "source": {"kind": "Code", "sourceRef": "repo:a/router.rs"}, "snippet": "newer",
            "sourceRevision": "c2", "claimId": claim}]}),
    ));
    // ... and a lesson recurrence citing it.
    receipt(fx.record(
        &a,
        proj("project-a"),
        json!({"entities": [], "evidence": [{"id": "m",
            "source": {"kind": "Code", "sourceRef": "repo:a/router.rs"}, "snippet": "newer",
            "sourceRevision": "c2"}], "claims": [with(draft("Lesson", "again"),
            json!({"subjectId": "unused-entity", "evidenceIds": ["m"], "lesson":
            with(lesson_draft("Agent", Some("l2"), None), json!({"sameAs": lesson}))}))]}),
    ));
    for id in [&claim, &lesson] {
        let c = fx.claim(&user, proj("project-a"), id);
        // Still stale against the revision its verification used, never read as Verified.
        assert_eq!(c.stale, Some(true), "{id:?}");
        assert_ne!(wire(&c.state), "Verified", "{id:?}");
        assert_eq!(c.verified_source_revision.as_ref().unwrap().0, "c1");
    }
    // Verifying again moves the baseline.
    receipt(fx.cmd(
        &user,
        "knowledge.verify",
        json!({"scope": proj("project-a"), "claimId": claim,
        "expectedRevision": fx.revision(&claim).to_string(), "reason": "rechecked",
        "evidenceIds": []}),
    ));
    let c = fx.claim(&user, proj("project-a"), &claim);
    assert_eq!((c.stale, wire(&c.state)), (Some(false), "Verified".into()));
}

#[test]
fn a_deleted_merges_shared_event_settles_while_its_target_is_itself_merged() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", Some("project-a"));
    let user = fx.user();
    let la = lesson_in_a(&fx, &a, "A", "Agent", "shared");
    let lc = lesson_in_a(&fx, &a, "C", "Agent", "shared");
    let lb = lesson_in_a(&fx, &a, "B", "Agent", "eb1");
    let ld = lesson_in_a(&fx, &a, "D", "Agent", "ed");
    recur_in_a(&fx, &a, &lb, "Agent", "eb2");
    relate_in_a(&fx, &a, &la, "same_as", &lb);
    relate_in_a(&fx, &a, &lc, "same_as", &lb);
    // B's copy of the shared event has moved on into D when C is deleted.
    relate_in_a(&fx, &a, &lb, "same_as", &ld);
    delete_claim(&fx, &user, &lc);
    relate_in_a(&fx, &a, &lb, "unmerge", &ld);
    relate_in_a(&fx, &a, &la, "unmerge", &lb);
    assert_eq!(rule(&fx, &a, &lb), effective("Recurrence", 3));
    assert_eq!((rule(&fx, &a, &la).2, rule(&fx, &a, &ld).2), (1, 1));
}

// Lesson lifecycle: what an automatic transition did, undoing it restores

fn lifecycle(fx: &Fx, ctx: &InvocationContext, claim: &Id) -> String {
    wire(
        fx.claim(ctx, proj("project-a"), claim)
            .lifecycle
            .as_ref()
            .unwrap(),
    )
}

/// The event keys a lesson holds, as stored.
fn event_keys(fx: &Fx, lesson: &Id) -> Vec<String> {
    let l = lesson.clone();
    let mut keys: Vec<String> = fx
        .read(move |s| s.knowledge().knowledge_lesson_events(&l, 500))
        .into_iter()
        .map(|e| e.event_key)
        .collect();
    keys.sort();
    keys
}

fn propose(fx: &Fx, ctx: &InvocationContext, lesson: &Id) {
    let rev = fx.revision(lesson);
    receipt(fx.cmd(
        ctx,
        "knowledge.promote",
        json!({"claimId": lesson, "expectedRevision": rev.to_string(),
        "destination": proj("project-a"), "action": "Propose"}),
    ));
}

fn proposed(events: u64) -> (String, Option<String>, u64) {
    ("Proposed".to_owned(), None, events)
}

#[test]
fn a_proposed_rule_that_loses_its_recurrence_returns_to_proposed() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", Some("project-a"));
    let la = lesson_in_a(&fx, &a, "A", "Agent", "ea");
    let lb = lesson_in_a(&fx, &a, "B", "Agent", "eb1");
    propose(&fx, &a, &lb);
    recur_in_a(&fx, &a, &lb, "Agent", "eb2");
    assert_eq!(rule(&fx, &a, &lb), proposed(2));
    relate_in_a(&fx, &a, &la, "same_as", &lb);
    assert_eq!(rule(&fx, &a, &lb), effective("Recurrence", 3));
    // A revision keeps what the rule took effect from.
    let subject = {
        let b = lb.clone();
        fx.read(move |s| s.knowledge().knowledge_claim(&b))
            .unwrap()
            .subject_id
    };
    receipt(fx.cmd(
        &a,
        "knowledge.revise",
        json!({"scope": proj("project-a"), "claimId": lb,
        "expectedRevision": fx.revision(&lb).to_string(),
        "replacement": with(draft("Lesson", "B, reworded"), json!({"subjectId": subject}))}),
    ));
    relate_in_a(&fx, &a, &la, "unmerge", &lb);
    assert_eq!(rule(&fx, &a, &lb), proposed(2));
    // It takes effect again, and loses it again, as often as its recurrence comes and goes.
    relate_in_a(&fx, &a, &la, "same_as", &lb);
    assert_eq!(rule(&fx, &a, &lb), effective("Recurrence", 3));
    relate_in_a(&fx, &a, &la, "unmerge", &lb);
    assert_eq!(rule(&fx, &a, &lb), proposed(2));
}

#[test]
fn a_proposed_rule_carried_by_a_merged_user_rule_returns_to_proposed() {
    let fx = Fx::new();
    let user = fx.user();
    let a = fx.agent("agent-a", Some("project-a"));
    let la = lesson_in_a(&fx, &user, "A", "Prompt", "p1");
    let lb = lesson_in_a(&fx, &a, "B", "Agent", "eb1");
    propose(&fx, &a, &lb);
    relate_in_a(&fx, &user, &la, "same_as", &lb);
    assert_eq!(rule(&fx, &a, &lb), effective("UserInstruction", 2));
    relate_in_a(&fx, &user, &la, "unmerge", &lb);
    assert_eq!(rule(&fx, &a, &lb), proposed(1));
}

#[test]
fn deleting_a_merge_target_restores_the_lessons_merged_into_it() {
    let fx = Fx::new();
    let user = fx.user();
    let a = fx.agent("agent-a", Some("project-a"));
    // A cites `repo:merged` at c1 and is in effect by its own recurrence; C is the user's rule.
    let (la, lb, _) = three_lessons(&fx, &a);
    recur_in_a(&fx, &a, &la, "Agent", "ea2");
    recur_in_a(&fx, &a, &la, "Agent", "ea3");
    assert_eq!(rule(&fx, &a, &la), effective("Recurrence", 3));
    let lc = lesson_in_a(&fx, &user, "C", "Prompt", "p1");
    let a_events = event_keys(&fx, &la);
    relate_in_a(&fx, &a, &la, "same_as", &lb);
    relate_in_a(&fx, &user, &lc, "same_as", &lb);
    assert_eq!(rule(&fx, &a, &lb), effective("UserInstruction", 5));
    delete_claim(&fx, &user, &lb);
    assert_eq!(lifecycle(&fx, &a, &lb), "Deleted");
    // Each merged lesson is independent again, with its own events (references intact) and
    // its effect: the user's rule stays in effect, recurrence is recounted.
    for l in [&la, &lc] {
        let c = fx.claim(&a, proj("project-a"), l);
        assert_eq!(wire(c.lifecycle.as_ref().unwrap()), "Active");
        assert_eq!(c.superseded_by, None);
    }
    assert_eq!(event_keys(&fx, &la), a_events);
    assert_eq!(rule(&fx, &a, &la), effective("Recurrence", 3));
    assert_eq!(rule(&fx, &a, &lc), effective("UserInstruction", 1));
    // A keeps its citation and its baseline: a newer revision of the source marks it stale.
    assert_eq!(cited_snippets(&fx, &a, &la), vec!["merged evidence"]);
    observe_merged_source(&fx);
    assert_eq!(fx.claim(&a, proj("project-a"), &la).stale, Some(true));
    // No merge is left to undo: A and C are lessons of their own.
    relate_in_a(&fx, &a, &la, "same_as", &lc);
    assert_eq!(rule(&fx, &a, &lc), effective("UserInstruction", 4));
}

#[test]
fn deleting_a_merge_target_that_is_merged_on_restores_through_the_chain() {
    let fx = Fx::new();
    let user = fx.user();
    let a = fx.agent("agent-a", Some("project-a"));
    // E is merged into A, A into B, and B on into D, which takes effect by recurrence.
    let le = lesson_in_a(&fx, &a, "E", "Agent", "ee");
    let la = lesson_in_a(&fx, &a, "A", "Agent", "ea");
    let lb = lesson_in_a(&fx, &a, "B", "Agent", "eb");
    let ld = lesson_in_a(&fx, &a, "D", "Agent", "ed");
    propose(&fx, &a, &ld);
    relate_in_a(&fx, &a, &le, "same_as", &la);
    relate_in_a(&fx, &a, &la, "same_as", &lb);
    relate_in_a(&fx, &a, &lb, "same_as", &ld);
    assert_eq!(rule(&fx, &a, &ld), effective("Recurrence", 4));
    delete_claim(&fx, &user, &lb);
    // A is independent again with its own events and E's (E stays merged into A) ...
    assert_eq!(lifecycle(&fx, &a, &la), "Active");
    assert_eq!(rule(&fx, &a, &la).2, 2);
    assert_eq!(lifecycle(&fx, &a, &le), "Superseded");
    relate_in_a(&fx, &a, &le, "unmerge", &la);
    assert_eq!((rule(&fx, &a, &la).2, rule(&fx, &a, &le).2), (1, 1));
    // ... and D, which held them through B, keeps only its own and B's: its recurrence is
    // recounted, back to the state it took effect from.
    assert_eq!(rule(&fx, &a, &ld), proposed(2));
}

#[test]
fn a_restored_user_rule_no_longer_keeps_the_lessons_above_in_effect() {
    let fx = Fx::new();
    let user = fx.user();
    let a = fx.agent("agent-a", Some("project-a"));
    let la = lesson_in_a(&fx, &user, "A", "Prompt", "p1");
    let lb = lesson_in_a(&fx, &a, "B", "Agent", "eb");
    let ld = lesson_in_a(&fx, &a, "D", "Agent", "ed");
    relate_in_a(&fx, &user, &la, "same_as", &lb);
    relate_in_a(&fx, &a, &lb, "same_as", &ld);
    assert_eq!(rule(&fx, &a, &ld), effective("UserInstruction", 3));
    // B carried only A's rule, and A is restored: D's own rule returns.
    delete_claim(&fx, &user, &lb);
    assert_eq!(rule(&fx, &a, &la), effective("UserInstruction", 1));
    assert_eq!(rule(&fx, &a, &ld), ("Candidate".into(), None, 2));
}

#[test]
fn a_restoration_refills_the_active_lesson_from_a_deeper_merges_copy() {
    let fx = Fx::new();
    let user = fx.user();
    let a = fx.agent("agent-a", Some("project-a"));
    // A and Y recurred in the same event. A's copy travels A→B→D→F; Y's stays with Y (D had
    // the event when Y merged into D).
    let la = lesson_in_a(&fx, &a, "A", "Agent", "shared");
    let ly = lesson_in_a(&fx, &a, "Y", "Agent", "shared");
    let lb = lesson_in_a(&fx, &a, "B", "Agent", "eb");
    let ld = lesson_in_a(&fx, &a, "D", "Agent", "ed");
    let lf = lesson_in_a(&fx, &a, "F", "Agent", "ef");
    relate_in_a(&fx, &a, &la, "same_as", &lb);
    relate_in_a(&fx, &a, &lb, "same_as", &ld);
    relate_in_a(&fx, &a, &ly, "same_as", &ld);
    relate_in_a(&fx, &a, &ld, "same_as", &lf);
    assert_eq!(rule(&fx, &a, &lf).2, 4);
    delete_claim(&fx, &user, &lb);
    // A takes its copy back; F gets Y's copy in its place (through D), so the event still
    // counts once for F, and unwinding D then Y returns it to Y.
    assert_eq!((rule(&fx, &a, &la).2, rule(&fx, &a, &lf).2), (1, 4));
    relate_in_a(&fx, &a, &ld, "unmerge", &lf);
    relate_in_a(&fx, &a, &ly, "unmerge", &ld);
    assert_eq!(
        (
            rule(&fx, &a, &ly).2,
            rule(&fx, &a, &ld).2,
            rule(&fx, &a, &lf).2
        ),
        (1, 2, 1)
    );
}

#[test]
fn a_source_deletion_previews_and_restores_the_lessons_merged_into_its_lessons() {
    let fx = Fx::new();
    let user = fx.user();
    let a = fx.agent("agent-a", Some("project-a"));
    let lb = receipt(fx.record(
        &a,
        proj("project-a"),
        json!({"entities": [{"clientId": "s", "label": "Rules", "entityType": "t",
            "aliases": []}], "evidence": [], "claims": [
            with(draft("Lesson", "from run 7"), json!({"source": "run-7",
                "lesson": lesson_draft("Agent", Some("eb"), None)}))]}),
    ))
    .claim_ids
    .0[0]
        .clone();
    let la = lesson_in_a(&fx, &a, "A", "Agent", "ea");
    relate_in_a(&fx, &a, &la, "same_as", &lb);
    assert!(
        preview_message(&fx, &user, "run-7").contains("restores 1 lessons"),
        "{}",
        preview_message(&fx, &user, "run-7")
    );
    let (d, _) = fx.diagnostics(
        &user,
        json!({"scope": proj("project-a"), "sourceId": "run-7"}),
    );
    let listed: Vec<String> = d
        .iter()
        .filter(|x| x.component.0 == "knowledge.restore")
        .map(|x| x.message.0.clone())
        .collect();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].contains(la.as_str()) && listed[0].contains(lb.as_str()));
    delete_source(&fx, &user, "run-7");
    assert_eq!(lifecycle(&fx, &a, &lb), "Deleted");
    assert_eq!(lifecycle(&fx, &a, &la), "Active");
    assert_eq!(rule(&fx, &a, &la).2, 1);
    assert_eq!(event_keys(&fx, &la), vec!["Agent|ea".to_owned()]);
}

#[test]
fn a_restored_lesson_recounts_its_recurrence_and_keeps_the_users_rule() {
    let fx = Fx::new();
    let user = fx.user();
    let a = fx.agent("agent-a", Some("project-a"));
    let la = lesson_in_a(&fx, &a, "A", "Agent", "ea1");
    propose(&fx, &a, &la);
    recur_in_a(&fx, &a, &la, "Agent", "ea2");
    recur_in_a(&fx, &a, &la, "Agent", "ea3");
    assert_eq!(rule(&fx, &a, &la), effective("Recurrence", 3));
    let lc = lesson_in_a(&fx, &user, "C", "Prompt", "p1");
    let lb = lesson_in_a(&fx, &a, "B", "Agent", "eb");
    relate_in_a(&fx, &a, &la, "same_as", &lb);
    relate_in_a(&fx, &user, &lc, "same_as", &lb);
    fx.set_threshold(5);
    delete_claim(&fx, &user, &lb);
    // Three events no longer reach the threshold: A is back to the state it took effect from.
    assert_eq!(rule(&fx, &a, &la), proposed(3));
    assert_eq!(rule(&fx, &a, &lc), effective("UserInstruction", 1));
}

#[test]
fn deleting_a_merge_chain_together_restores_the_lesson_merged_into_it() {
    let fx = Fx::new();
    let user = fx.user();
    let a = fx.agent("agent-a", Some("project-a"));
    let la = lesson_in_a(&fx, &a, "A", "Agent", "ea");
    let lb = lesson_in_a(&fx, &a, "B", "Agent", "eb");
    let ld = lesson_in_a(&fx, &a, "D", "Agent", "ed");
    relate_in_a(&fx, &a, &la, "same_as", &lb);
    relate_in_a(&fx, &a, &lb, "same_as", &ld);
    // D goes first: A's event, held by D through B, returns to A before D's are purged.
    summary(fx.cmd(
        &user,
        "knowledge.delete_selected",
        json!({"scope": proj("project-a"), "claims": [
            {"id": ld, "revision": fx.revision(&ld).to_string()},
            {"id": lb, "revision": fx.revision(&lb).to_string()}]}),
    ));
    assert_eq!(lifecycle(&fx, &a, &la), "Active");
    assert_eq!(event_keys(&fx, &la), vec!["Agent|ea".to_owned()]);
    assert_eq!(
        (lifecycle(&fx, &a, &lb), lifecycle(&fx, &a, &ld)),
        ("Deleted".into(), "Deleted".into())
    );
}

// Codex round 6

/// Two general claims X and Y, and evidence project B recorded. Returns (X, Y, B's evidence).
fn general_pair_and_b_evidence(
    fx: &Fx,
    user: &InvocationContext,
    b: &InvocationContext,
) -> (Id, Id, Id) {
    let x = fx.claim_id(user, ws(), "X", draft("Fact", "x"));
    let y = fx.claim_id(user, ws(), "Y", draft("Fact", "y"));
    let r = receipt(fx.record(
        b,
        proj("project-b"),
        json!({
        "entities": [{"clientId": "s", "label": "B notes", "entityType": "t", "aliases": []}],
        "evidence": [{"id": "e", "source": {"kind": "Observation", "sourceRef": "log:b"},
            "snippet": "project B private evidence"}],
        "claims": [with(draft("Fact", "b holder"), json!({"evidenceIds": ["e"]}))]}),
    ));
    let e = fx
        .read_claim(b, proj("project-b"), &r.claim_ids.0[0])
        .evidence
        .0[0]
        .id
        .clone();
    (x, y, e)
}

fn relations_of(fx: &Fx, claim: &Id) -> Vec<StoredRelation> {
    let c = claim.clone();
    fx.read(move |s| {
        s.knowledge()
            .knowledge_relations_touching(std::slice::from_ref(&c))
    })
}

fn move_general(fx: &Fx, ctx: &InvocationContext, claims: &[&Id]) -> DomainFault {
    let refs: Vec<Value> = claims
        .iter()
        .map(|c| json!({"id": c, "revision": fx.revision(c).to_string()}))
        .collect();
    refused(fx.cmd(
        ctx,
        "knowledge.scope",
        json!({"claimIds": refs, "destination": proj("project-a")}),
    ))
}

#[test]
fn a_move_never_takes_or_strands_another_projects_private_relation() {
    let fx = Fx::new();
    let user = fx.user();
    let b = fx.agent("agent-b", Some("project-b"));
    let (x, y, e) = general_pair_and_b_evidence(&fx, &user, &b);
    // Project B relates the two general claims privately, citing its own evidence.
    receipt(fx.cmd(
        &b,
        "knowledge.relate",
        json!({"scope": proj("project-b"), "subjectId": x,
        "predicate": "depends_on", "objectId": y, "evidenceIds": [e]}),
    ));
    // Moving both into project A would take B's relation (and its evidence) into A; moving one
    // would strand it where B no longer sees the moved claim. Both are refused, without
    // naming B's record.
    for claims in [vec![&x, &y], vec![&x]] {
        let f = move_general(&fx, &user, &claims);
        assert_eq!(f.code, DomainErrorCode::Conflict);
        assert_eq!(f.target_kind.unwrap().0, "knowledge_relation");
        assert_eq!(f.target_id, None);
    }
    // B keeps its relation in its own scope, and X and Y stay general.
    let kept = relations_of(&fx, &x);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].scope, KnowledgeScope::Project(project("project-b")));
    for c in [&x, &y] {
        let c = c.clone();
        let stored = fx.read(move |s| s.knowledge().knowledge_claim(&c)).unwrap();
        assert_eq!(stored.scope, KnowledgeScope::Workspace);
    }
    assert_eq!(stored_snippet(&fx, &e), "project B private evidence");
}

#[test]
fn a_move_refused_by_another_projects_claim_never_names_it() {
    let fx = Fx::new();
    let user = fx.user();
    let b = fx.agent("agent-b", Some("project-b"));
    let (x, _, _) = general_pair_and_b_evidence(&fx, &user, &b);
    fx.claim_id(
        &b,
        proj("project-b"),
        "B",
        with(draft("Fact", "b depends on x"), json!({"objectId": x})),
    );
    // B's claim names X: X cannot move where B no longer sees it, and A learns nothing of B's
    // claim but that something blocks the move.
    let f = move_general(&fx, &user, &[&x]);
    assert_eq!(f.code, DomainErrorCode::Conflict);
    assert_eq!(f.target_kind.unwrap().0, "knowledge_claim");
    assert_eq!(f.target_id, None);
}

/// Lessons A, B and C of project A (A and C cite `repo:merged` at c1), A merged into B.
/// Returns (A, B, C, the merged evidence).
fn merged_citation(fx: &Fx, a: &InvocationContext) -> (Id, Id, Id, Id) {
    let (la, lb, lc) = three_lessons(fx, a);
    let e = fx.read_claim(a, proj("project-a"), &la).evidence.0[0]
        .id
        .clone();
    relate_in_a(fx, a, &la, "same_as", &lb);
    (la, lb, lc, e)
}

/// Whether B still cites the merged evidence and a newer revision of its source marks it
/// stale (its baseline stayed).
fn keeps_merged_citation(fx: &Fx, a: &InvocationContext, lb: &Id) {
    assert_eq!(cited_snippets(fx, a, lb), vec!["merged evidence"]);
    observe_merged_source(fx);
    assert_eq!(fx.claim(a, proj("project-a"), lb).stale, Some(true));
}

#[test]
fn a_verification_keeps_the_citation_and_baseline_it_used_after_an_unmerge() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", Some("project-a"));
    let user = fx.user();
    let (la, lb, _, e) = merged_citation(&fx, &a);
    receipt(fx.cmd(
        &user,
        "knowledge.verify",
        json!({"scope": proj("project-a"), "claimId": lb,
        "expectedRevision": fx.revision(&lb).to_string(), "reason": "checked",
        "evidenceIds": [e], "sourceRevision": "c1"}),
    ));
    relate_in_a(&fx, &a, &la, "unmerge", &lb);
    keeps_merged_citation(&fx, &a, &lb);
}

#[test]
fn a_revision_keeps_the_citations_it_restates_after_an_unmerge() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", Some("project-a"));
    let (la, lb, _, e) = merged_citation(&fx, &a);
    let subject = {
        let b = lb.clone();
        fx.read(move |s| s.knowledge().knowledge_claim(&b))
            .unwrap()
            .subject_id
    };
    receipt(fx.cmd(
        &a,
        "knowledge.revise",
        json!({"scope": proj("project-a"), "claimId": lb,
        "expectedRevision": fx.revision(&lb).to_string(),
        "replacement": with(draft("Lesson", "rule b, reworded"),
            json!({"subjectId": subject, "evidenceIds": [e]}))}),
    ));
    relate_in_a(&fx, &a, &la, "unmerge", &lb);
    keeps_merged_citation(&fx, &a, &lb);
}

#[test]
fn a_recurrence_keeps_the_citation_it_brings_after_an_unmerge() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", Some("project-a"));
    let (la, lb, _, e) = merged_citation(&fx, &a);
    receipt(fx.record(
        &a,
        proj("project-a"),
        json!({"entities": [], "evidence": [], "claims": [
        with(draft("Lesson", "again"), json!({"subjectId": "unused-entity",
            "evidenceIds": [e], "lesson": with(lesson_draft("Agent", Some("eb2"), None),
            json!({"sameAs": lb}))}))]}),
    ));
    relate_in_a(&fx, &a, &la, "unmerge", &lb);
    keeps_merged_citation(&fx, &a, &lb);
}

#[test]
fn attached_evidence_keeps_its_sources_baseline_after_an_unmerge() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", Some("project-a"));
    let (la, lb, _, _) = merged_citation(&fx, &a);
    // New evidence of the merged source at the same revision: B already has that baseline
    // (through the merge), which is now B's own.
    receipt(fx.record(
        &a,
        proj("project-a"),
        json!({"entities": [], "claims": [], "evidence": [{"id": "n",
            "source": {"kind": "Code", "sourceRef": "repo:merged"},
            "snippet": "attached evidence", "sourceRevision": "c1", "claimId": lb}]}),
    ));
    relate_in_a(&fx, &a, &la, "unmerge", &lb);
    observe_merged_source(&fx);
    assert_eq!(fx.claim(&a, proj("project-a"), &lb).stale, Some(true));
}

#[test]
fn evidence_an_unmerge_cites_stays_after_the_last_merge_is_undone() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", Some("project-a"));
    let (la, lb, lc, e) = merged_citation(&fx, &a);
    relate_in_a(&fx, &a, &lc, "same_as", &lb);
    // The first unmerge cites the merged evidence: B keeps it, as its own.
    relate_with(&fx, &a, &la, "unmerge", &lb, &[&e]);
    relate_in_a(&fx, &a, &lc, "unmerge", &lb);
    keeps_merged_citation(&fx, &a, &lb);
}

#[test]
fn an_unmerge_keeps_the_baseline_of_the_evidence_it_cites() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", Some("project-a"));
    let (la, lb, _, e) = merged_citation(&fx, &a);
    // The unmerge cites the merged evidence, so B keeps it, and its baseline with it.
    relate_with(&fx, &a, &la, "unmerge", &lb, &[&e]);
    keeps_merged_citation(&fx, &a, &lb);
}

#[test]
fn evidence_supplied_with_a_merge_brings_its_baseline_and_unmerge_takes_it_back() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", Some("project-a"));
    // B cites nothing; X is a lesson without evidence; the evidence of `repo:merged` at c1
    // comes only with the merge.
    let (la, lb, _) = three_lessons(&fx, &a);
    let e = fx.read_claim(&a, proj("project-a"), &la).evidence.0[0]
        .id
        .clone();
    let lx = lesson_in_a(&fx, &a, "X", "Agent", "ex");
    relate_with(&fx, &a, &lx, "same_as", &lb, &[&e]);
    assert_eq!(cited_snippets(&fx, &a, &lb), vec!["merged evidence"]);
    observe_merged_source(&fx);
    assert_eq!(fx.claim(&a, proj("project-a"), &lb).stale, Some(true));
    // The merge owns both, so its unmerge takes both back.
    relate_in_a(&fx, &a, &lx, "unmerge", &lb);
    assert!(cited_snippets(&fx, &a, &lb).is_empty());
    assert_eq!(fx.claim(&a, proj("project-a"), &lb).stale, Some(false));
}

/// Every text value of every Knowledge table, its lexical projections included.
fn every_knowledge_text(fx: &Fx) -> String {
    fx.store
        .with_writer_connection(|c| {
            let mut tables = Vec::new();
            {
                let mut st = c
                    .prepare(
                        "SELECT name FROM sqlite_schema WHERE type='table' AND name LIKE \
                         'knowledge\\_%' ESCAPE '\\'",
                    )
                    .unwrap();
                let rows = st.query_map([], |r| r.get::<_, String>(0)).unwrap();
                for r in rows {
                    tables.push(r.unwrap());
                }
            }
            let mut out = String::new();
            for t in tables {
                let mut st = c.prepare(&format!("SELECT * FROM \"{t}\"")).unwrap();
                let n = st.column_count();
                let mut rows = st.query([]).unwrap();
                while let Some(row) = rows.next().unwrap() {
                    for i in 0..n {
                        if let Ok(rusqlite::types::ValueRef::Text(b)) = row.get_ref(i) {
                            out.push_str(&format!("{t}: "));
                            out.push_str(&String::from_utf8_lossy(b));
                            out.push('\n');
                        }
                    }
                }
            }
            Ok(out)
        })
        .unwrap()
}

/// A claim of source `run-<tag>` whose every authored field carries `secret-<tag>`: its
/// subject entity (of the same source), evidence, tags, a revision, a challenge and a
/// relation to a claim of no source. Returns the claim.
fn secret_claim(fx: &Fx, user: &InvocationContext, tag: &str) -> Id {
    let s = |what: &str| format!("{what}-secret-{tag}");
    let source = format!("run-{tag}");
    let r = receipt(fx.record(
        user,
        proj("project-a"),
        json!({
        "entities": [{"clientId": "s", "label": s("label"), "entityType": s("type"),
            "aliases": [s("alias")], "canonicalName": s("canon"), "externalId": s("ext"),
            "source": source}],
        "evidence": [{"id": "e", "source": {"kind": "Observation", "sourceRef": s("ref"),
            "locator": s("locator")}, "snippet": s("snippet")}],
        "claims": [{"kind": "Fact", "subjectId": "s", "predicate": s("predicate"),
            "value": s("value"), "confidence": 0.5, "evidenceIds": ["e"],
            "tags": [s("tag")], "source": source}]}),
    ));
    let claim = r.claim_ids.0[0].clone();
    let subject = r.entity_ids.0[0].clone();
    let e = fx.read_claim(user, proj("project-a"), &claim).evidence.0[0]
        .id
        .clone();
    receipt(fx.cmd(
        user,
        "knowledge.revise",
        json!({"scope": proj("project-a"), "claimId": claim,
        "expectedRevision": fx.revision(&claim).to_string(),
        "replacement": {"kind": "Fact", "subjectId": subject, "predicate": s("predicate"),
            "value": s("revised"), "confidence": 0.5, "evidenceIds": [e],
            "tags": [s("tag")], "source": source}}),
    ));
    receipt(fx.cmd(
        user,
        "knowledge.challenge",
        json!({"scope": proj("project-a"), "claimId": claim,
        "expectedRevision": fx.revision(&claim).to_string(), "reason": s("challenge"),
        "evidenceIds": []}),
    ));
    let other = fx.claim_id(user, proj("project-a"), "Other", draft("Fact", "other"));
    relate_in_a(fx, user, &claim, &s("relation"), &other);
    claim
}

#[test]
fn deleting_a_claim_scrubs_its_predicate_and_every_authored_field_everywhere() {
    let fx = Fx::new();
    let user = fx.user();
    let selected = secret_claim(&fx, &user, "x");
    let by_source = secret_claim(&fx, &user, "y");
    let text = every_knowledge_text(&fx);
    assert!(text.contains("predicate-secret-x") && text.contains("predicate-secret-y"));
    delete_claim(&fx, &user, &selected);
    delete_source(&fx, &user, "run-y");
    for c in [&selected, &by_source] {
        let read = fx.claim(&user, proj("project-a"), c);
        assert_eq!(wire(read.lifecycle.as_ref().unwrap()), "Deleted");
        assert_eq!(read.predicate.0, "(deleted)");
    }
    let text = every_knowledge_text(&fx);
    // A selected deletion removes the claim only (its subject entity stays, as recorded); a
    // delete by source removes the entity it no longer needs as well.
    for what in [
        "predicate",
        "value",
        "revised",
        "tag",
        "ref",
        "locator",
        "snippet",
        "challenge",
        "relation",
    ] {
        for tag in ["x", "y"] {
            let secret = format!("{what}-secret-{tag}");
            assert!(
                !text.contains(&secret),
                "{secret} survived the deletion: {}",
                text.lines()
                    .filter(|l| l.contains(&secret))
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
        }
    }
    for what in ["label", "type", "alias", "canon", "ext"] {
        let secret = format!("{what}-secret-y");
        assert!(!text.contains(&secret), "{secret} survived the deletion");
        assert!(
            text.contains(&format!("{what}-secret-x")),
            "the kept entity keeps {what}"
        );
    }
}

#[test]
fn search_words_match_composed_and_decomposed_spellings_alike() {
    let fx = Fx::new();
    let user = fx.user();
    let nfc = "résumé";
    let nfd = "re\u{301}sume\u{301}";
    let composed = fx.claim_id(
        &user,
        proj("project-a"),
        "Hiring",
        draft("Fact", &format!("the {nfc} template")),
    );
    let decomposed = fx.claim_id(
        &user,
        proj("project-a"),
        "Hiring",
        draft("Fact", &format!("another {nfd} layout")),
    );
    // Stored either way, found by an exact query either way.
    for query in [
        nfc,
        nfd,
        &format!("{nfd} template"),
        &format!("{nfc} layout"),
    ] {
        let found = ids(&fx.search(&user, proj("project-a"), query, json!({})));
        let expect: Vec<String> = match query {
            q if q.ends_with("template") => vec![composed.as_str().to_owned()],
            q if q.ends_with("layout") => vec![decomposed.as_str().to_owned()],
            _ => {
                let mut both = vec![composed.as_str().to_owned(), decomposed.as_str().to_owned()];
                both.sort();
                both
            }
        };
        let mut found = found;
        found.sort();
        assert_eq!(found, expect, "query {query:?}");
    }
}

// Codex round 8

/// The recurrence a lesson's revision `revision` reads with.
fn recurrence_at(fx: &Fx, ctx: &InvocationContext, lesson: &Id, revision: i64) -> u64 {
    let past = match result(fx.q(
        ctx,
        "knowledge.read",
        json!({"scope": proj("project-a"), "claimId": lesson,
        "revision": revision.to_string()}),
    )) {
        OperationResult::KnowledgeRead { value } => value,
        other => panic!("{other:?}"),
    };
    past.claims
        .0
        .iter()
        .find(|c| c.id == *lesson)
        .unwrap()
        .lesson
        .as_ref()
        .unwrap()
        .recurrence
        .get() as u64
}

#[test]
fn a_recorded_lessons_first_revision_reads_with_its_first_event() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", Some("project-a"));
    let lesson = lesson_in_a(&fx, &a, "A", "Agent", "e1");
    recur_in_a(&fx, &a, &lesson, "Agent", "e2");
    assert_eq!(recurrence_at(&fx, &a, &lesson, 1), 1);
    assert_eq!(rule(&fx, &a, &lesson).2, 2);
}

#[test]
fn an_imported_lessons_first_revision_reads_with_its_first_event() {
    let fx = Fx::new();
    let user = fx.user();
    let bytes = serde_json::to_vec(&json!({"format": IMPORT_SCHEMA_ID, "candidates": [
        {"entry": {"kind": "lesson", "summary": "Run fmt before committing"},
            "sourceFile": "a.md", "verification": "verified"}]}))
    .unwrap();
    let source = saved_import(&fx, &user, &bytes, "lesson");
    let schema =
        json!({"schemaId": IMPORT_SCHEMA_ID, "revision": "1", "sha256": import_schema_digest()});
    let (d, _) = fx.diagnostics(
        &user,
        json!({"scope": proj("project-a"), "importSource": source, "importSchema": schema}),
    );
    let digest = d
        .iter()
        .find(|x| x.component.0 == "knowledge.preview")
        .unwrap()
        .id
        .as_str()
        .to_owned();
    let r = receipt(fx.cmd(
        &user,
        "knowledge.import_user_data",
        json!({"scope": proj("project-a"), "source": source, "schema": schema,
        "previewDigest": digest}),
    ));
    let lesson = r.claim_ids.0[0].clone();
    recur_in_a(&fx, &user, &lesson, "Agent", "later");
    assert_eq!(recurrence_at(&fx, &user, &lesson, 1), 1);
    assert_eq!(rule(&fx, &user, &lesson).2, 2);
}

/// Claims X and Y of project A about entities E1 and E2 (one each), E1 related to E2, and a
/// third claim Z about entity E3. Returns (X, Y, Z, E1, E2, E3).
fn related_entities(fx: &Fx, user: &InvocationContext) -> (Id, Id, Id, Id, Id, Id) {
    let r = receipt(fx.record(
        user,
        proj("project-a"),
        json!({
        "entities": [
            {"clientId": "e1", "label": "Parser", "entityType": "module", "aliases": []},
            {"clientId": "e2", "label": "Lexer", "entityType": "module", "aliases": []},
            {"clientId": "e3", "label": "Printer", "entityType": "module", "aliases": []}],
        "evidence": [],
        "claims": [
            with(draft("Fact", "x"), json!({"subjectId": "e1"})),
            with(draft("Fact", "y"), json!({"subjectId": "e2"})),
            with(draft("Fact", "z"), json!({"subjectId": "e3"}))]}),
    ));
    let c = &r.claim_ids.0;
    let e = &r.entity_ids.0;
    relate_in_a(fx, user, &e[0], "depends_on", &e[1]);
    (
        c[0].clone(),
        c[1].clone(),
        c[2].clone(),
        e[0].clone(),
        e[1].clone(),
        e[2].clone(),
    )
}

fn move_to(fx: &Fx, ctx: &InvocationContext, claims: &[&Id], to: Value) -> DomainCommandReply {
    let refs: Vec<Value> = claims
        .iter()
        .map(|c| json!({"id": c, "revision": fx.revision(c).to_string()}))
        .collect();
    fx.cmd(
        ctx,
        "knowledge.scope",
        json!({"claimIds": refs, "destination": to}),
    )
}

#[test]
fn a_relation_between_entities_that_all_move_moves_with_them() {
    let fx = Fx::new();
    let user = fx.user();
    let (x, y, _, e1, e2, _) = related_entities(&fx, &user);
    // X and Y are the only users of E1 and E2: both entities and their relation move.
    receipt(move_to(&fx, &user, &[&x, &y], ws()));
    for e in [&e1, &e2] {
        let id = e.clone();
        let stored = fx
            .read(move |s| s.knowledge().knowledge_entity(&id))
            .unwrap();
        assert_eq!(stored.scope, KnowledgeScope::Workspace);
    }
    let rel = relations_of_entity(&fx, &e1);
    assert_eq!(rel.len(), 1);
    assert_eq!(rel[0].scope, KnowledgeScope::Workspace);
}

#[test]
fn a_relation_to_an_entity_that_stays_still_refuses_the_move() {
    let fx = Fx::new();
    let user = fx.user();
    let (x, _, _, _, _, _) = related_entities(&fx, &user);
    // E1 would follow X, but it is related to E2, which stays with Y.
    let f = refused(move_to(&fx, &user, &[&x], ws()));
    assert_eq!(f.code, DomainErrorCode::Conflict);
    assert_eq!(f.target_kind.unwrap().0, "knowledge_entity");
    // A relation from a scope the mover cannot read still refuses, even between records that
    // all move (the round-6 rule); a readable one between moving entities does not.
    let fx = Fx::new();
    let user = fx.user();
    let (x, y, _, _, _, _) = related_entities(&fx, &user);
    let b = fx.agent("agent-b", Some("project-b"));
    let w1 = fx.claim_id(&user, ws(), "W1", draft("Fact", "w1"));
    let w2 = fx.claim_id(&user, ws(), "W2", draft("Fact", "w2"));
    receipt(fx.cmd(
        &b,
        "knowledge.relate",
        json!({"scope": proj("project-b"), "subjectId": w1,
        "predicate": "depends_on", "objectId": w2, "evidenceIds": []}),
    ));
    let f = refused(move_to(&fx, &user, &[&w1, &w2], proj("project-a")));
    assert_eq!(f.code, DomainErrorCode::Conflict);
    assert_eq!(f.target_id, None);
    receipt(move_to(&fx, &user, &[&x, &y], ws()));
}

fn relations_of_entity(fx: &Fx, entity: &Id) -> Vec<StoredRelation> {
    let e = entity.clone();
    fx.read(move |s| {
        s.knowledge()
            .knowledge_relations_touching(std::slice::from_ref(&e))
    })
}
