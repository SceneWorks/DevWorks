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
