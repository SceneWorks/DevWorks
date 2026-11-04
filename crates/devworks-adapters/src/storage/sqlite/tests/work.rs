//! Features, standalone issues and task cards end to end on the real SQLite store: every
//! `features.*`, `issues.*` and `tasks.*` operation through the gateway with host authority and
//! receipts; one identity across the task and issue views; revision-checked edits with no write
//! on refusal; Unicode search and scope-bound cursors; project-wide blocking links with cycle,
//! self, duplicate and cross-project refusal; priority and bucket ordering; workflow moves and
//! manual versus verified completion; membership link/unlink/transfer with the unlink pause; the
//! feature plan slot, approval requests and the human decision seam; readiness assessments and
//! unit-level invalidation feeding run control; re-planning by reference; atomic plans and a
//! project's initial plan; repository-in-use; archived projects; and migration 16. No inference.
use super::{Harness, through};
use crate::storage::sqlite::{MIGRATION_0016_SHA256, MIGRATIONS, SqliteStore};
use devworks_core::authority::HostAuthority;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::gateway::Gateway;
use devworks_core::ports::storage::*;
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::{ProjectRecord, WorkRecord};
use devworks_core::protocol::results::{
    DomainCommandReply, DomainDecision, DomainQueryReply, OperationResult,
};
use devworks_core::protocol::{
    ActorId, CommandReply, Counter, DomainErrorCode, DomainFault, Id, ProjectId, Revision, RootId,
};
use devworks_core::services::UseCases;
use devworks_core::services::history::{LinkKind, NewHistoryLink, record_link};
use devworks_core::services::projects::{ProjectService, RepositoryService};
use devworks_core::services::runs::{ControlService, Target};
use devworks_core::services::work::plans::{StagedPlans, decide_plan_approval};
use devworks_core::services::work::readiness::accept_finding_risk;
use devworks_core::services::work::workflow::default_status_id;
use devworks_core::services::work::{WorkService, plan_digest};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn actor(s: &str) -> ActorId {
    ActorId(id(s))
}

/// Staged plan bytes by digest (the artifact file reader's double).
#[derive(Default)]
struct Staged(Mutex<HashMap<String, Vec<u8>>>);
impl StagedPlans for Staged {
    fn read_plan(&self, digest: &str, _: &str, _: u64, max: usize) -> Result<Vec<u8>, String> {
        let b = self
            .0
            .lock()
            .unwrap()
            .get(digest)
            .cloned()
            .ok_or("missing")?;
        Ok(b.into_iter().take(max).collect())
    }
}

struct Fx {
    h: Harness,
    store: Arc<SqliteStore>,
    runs: Arc<ControlService>,
    cases: UseCases,
    staged: Arc<Staged>,
    n: AtomicU64,
}

impl Fx {
    fn new() -> Self {
        let h = Harness::new();
        let store = Arc::new(h.store());
        let runs = Arc::new(ControlService::new(h.ids.clone()));
        let staged = Arc::new(Staged::default());
        let work = Arc::new(
            WorkService::new(h.ids.clone(), runs.clone()).with_staged_plans(staged.clone()),
        );
        let cases = UseCases::new()
            .with_runs(runs.clone())
            .with_projects(Arc::new(
                ProjectService::new(h.ids.clone(), runs.clone()).with_work(work.clone()),
            ))
            .with_repositories(Arc::new(RepositoryService::new(h.ids.clone())))
            .with_work(work);
        let fx = Self {
            h,
            store,
            runs,
            cases,
            staged,
            n: AtomicU64::new(0),
        };
        fx.write(|tx| {
            for (a, k) in [
                ("user-a", PrincipalKind::Human),
                ("agent-w", PrincipalKind::Agent),
            ] {
                tx.actors().register(&actor(a), k, a)?;
            }
            Ok(())
        });
        // The agent is a workspace coordinator: planning and card management are its tools.
        let binding = NewPrincipalBinding {
            actor: actor("agent-w"),
            kind: PrincipalKind::Agent,
            role: Some(devworks_core::settings::catalog::AgentRole::WorkspaceCoordinator),
            project_id: None,
            tool_families: None,
            bound_by: actor("user-a"),
        };
        let by = fx.user();
        fx.write(move |tx| {
            devworks_core::authority::service::bind_principal(tx, &by, binding, None)
                .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
        });
        for r in ["repo-1", "repo-2", "repo-3"] {
            let r = StoredRepository {
                id: id(r),
                location: format!("/Users/me/src/{r}"),
                name: r.into(),
                remote_url: None,
                primary: true,
                lifecycle: "Active".into(),
                languages: Vec::new(),
                revision: 1,
                registered_at_ms: super::T0,
            };
            fx.write(move |tx| tx.projects().insert_repository(&r));
        }
        fx
    }
    fn ctx(&self, who: &str, kind: PrincipalKind, p: Option<&ProjectId>) -> InvocationContext {
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
            .with_lineage(p.cloned(), None, None)
    }
    fn user(&self) -> InvocationContext {
        self.ctx("user-a", PrincipalKind::Human, None)
    }
    fn agent(&self) -> InvocationContext {
        self.ctx("agent-w", PrincipalKind::Agent, None)
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
    fn command(&self, ctx: &InvocationContext, op: &str, body: Value) -> DomainCommandReply {
        self.command_at(ctx, op, body, None)
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
    fn project(&self, name: &str, repos: &[&str]) -> ProjectId {
        match applied(self.command(
            &self.user(),
            "projects.create",
            json!({"name": name, "description": "", "repositoryIds": repos}),
        )) {
            OperationResult::ProjectsCreate { value } => value.id,
            other => panic!("{other:?}"),
        }
    }
    fn status(p: &ProjectId, key: &str) -> String {
        default_status_id(p, key).as_str().to_owned()
    }
    fn feature(&self, p: &ProjectId, title: &str, repos: &[&str]) -> WorkRecord {
        match applied(self.command(
            &self.user(),
            "features.create",
            json!({"projectId": p.0.as_str(), "creation": {"kind": "Single", "title": title,
                "description": "", "repositoryIds": repos,
                "statusId": Self::status(p, "backlog"), "priority": 0}}),
        )) {
            OperationResult::FeaturesCreate { value } => value.items.0[0].clone(),
            other => panic!("{other:?}"),
        }
    }
    fn task(
        &self,
        ctx: &InvocationContext,
        p: &ProjectId,
        feature: Option<&WorkRecord>,
        title: &str,
        priority: i32,
    ) -> WorkRecord {
        let mut body = json!({"projectId": p.0.as_str(), "title": title, "description": "",
            "repositoryIds": [], "statusId": Self::status(p, "backlog"), "priority": priority});
        if let Some(f) = feature {
            body["parentId"] = json!(f.id.as_str());
        }
        work_of(applied(self.command(ctx, "tasks.create", body)))
    }
    fn get(&self, op: &str, w: &Id) -> Result<WorkRecord, DomainFault> {
        self.get_at(op, w, None)
    }
    fn get_at(&self, op: &str, w: &Id, revision: Option<u32>) -> Result<WorkRecord, DomainFault> {
        match self.query_at(
            &self.user(),
            op,
            json!({"workId": w.as_str(), "detailLevel": "Summary"}),
            revision,
        ) {
            DomainQueryReply::Result { result, .. } => Ok(match result {
                OperationResult::FeaturesGet { value }
                | OperationResult::IssuesGet { value }
                | OperationResult::TasksGet { value } => value,
                other => panic!("{other:?}"),
            }),
            DomainQueryReply::Rejected { fault, .. } => Err(fault),
        }
    }
    fn list(
        &self,
        op: &str,
        p: &ProjectId,
        limit: u32,
        cursor: Option<&str>,
        text: Option<&str>,
    ) -> Result<(Vec<WorkRecord>, Option<String>), DomainFault> {
        let mut page = json!({"limit": limit});
        if let Some(c) = cursor {
            page["cursor"] = json!(c);
        }
        let mut body = json!({"projectId": p.0.as_str(), "page": page, "statusIds": [],
            "repositoryIds": [], "detailLevel": "Summary"});
        if let Some(t) = text {
            body["text"] = json!(t);
        }
        match self.query(&self.user(), op, body) {
            DomainQueryReply::Result { result, .. } => {
                let page = match result {
                    OperationResult::FeaturesList { value }
                    | OperationResult::IssuesList { value }
                    | OperationResult::TasksList { value } => value,
                    other => panic!("{other:?}"),
                };
                Ok((page.items.0, page.next_cursor.map(|c| c.0)))
            }
            DomainQueryReply::Rejected { fault, .. } => Err(fault),
        }
    }
    fn query(&self, ctx: &InvocationContext, op: &str, body: Value) -> DomainQueryReply {
        self.query_at(ctx, op, body, None)
    }
    fn edit(&self, ctx: &InvocationContext, w: &WorkRecord, changes: Value) -> DomainCommandReply {
        self.command(
            ctx,
            "tasks.edit",
            json!({"workId": w.id.as_str(), "expectedRevision": w.revision.as_str(), "changes": changes}),
        )
    }
    fn links(&self, w: &WorkRecord, prereqs: &[&WorkRecord]) -> DomainCommandReply {
        let edges: Vec<Value> = prereqs
            .iter()
            .map(|p| json!({"predecessor": p.id.as_str(), "successor": w.id.as_str()}))
            .collect();
        self.command(
            &self.user(),
            "tasks.dependency_links",
            json!({"taskId": w.id.as_str(), "expectedRevision": w.revision.as_str(),
                "replacement": {"edges": edges}}),
        )
    }
    fn fresh(&self, w: &WorkRecord) -> WorkRecord {
        self.get(
            if matches!(w.kind, devworks_core::protocol::records::WorkKind::Feature) {
                "features.get"
            } else {
                "tasks.get"
            },
            &w.id,
        )
        .unwrap()
    }
    fn queue(&self, p: &ProjectId, q: &str) -> i64 {
        let (p, q) = (p.clone(), q.to_owned());
        self.read(move |s| s.work().queue_revision(&p, &q))
    }
    /// A plan content artifact owned by `p`.
    fn artifact(&self, p: &ProjectId, n: u32) -> ArtifactRef {
        let bytes = format!("plan text {n}").into_bytes();
        let digest = devworks_core::fingerprint::sha256_hex(&bytes);
        self.staged
            .0
            .lock()
            .unwrap()
            .insert(digest.clone(), bytes.clone());
        self.artifact_bytes(p, &bytes, n)
    }
    fn artifact_bytes(&self, p: &ProjectId, bytes: &[u8], n: u32) -> ArtifactRef {
        self.artifact_in(ArtifactScope::Project(p.clone()), None, bytes, n)
    }
    fn artifact_in(
        &self,
        scope: ArtifactScope,
        owner: Option<ActorId>,
        bytes: &[u8],
        n: u32,
    ) -> ArtifactRef {
        let p = scope.project().cloned();
        let digest = devworks_core::fingerprint::sha256_hex(bytes);
        let len = bytes.len() as i64;
        let (aid, vid) = (id(&format!("art-{n}")), id(&format!("ver-{n}")));
        let (a2, v2, d2) = (aid.clone(), vid.clone(), digest.clone());
        self.write(move |tx| {
            tx.artifacts()
                .put_blob(&d2, len, "gen-1", &format!("blobs/{d2}.gen-1"))?;
            let mut a = StoredArtifact {
                id: a2.clone(),
                origin_project_id: p,
                origin_work_id: None,
                title: "plan".into(),
                revision: 1,
                scope,
                owner,
                retention: ArtifactRetention::Explicit,
                scope_revision: 1,
                reference_revision: 1,
                current_version_id: None,
                state: ArtifactState::Live,
                created_at_ms: super::T0,
                provenance: json!({}),
            };
            tx.artifacts().insert_artifact(&a)?;
            tx.artifacts().insert_version(&StoredVersion {
                id: v2.clone(),
                artifact_id: a2.clone(),
                version_no: 1,
                blob_digest: d2.clone(),
                byte_count: len,
                blob_generation: "gen-1".into(),
                media_type: "text/plain".into(),
                created_at_ms: super::T0,
                deleted_at_ms: None,
                source: json!({}),
            })?;
            a.current_version_id = Some(v2.clone());
            a.revision = 2;
            tx.artifacts().update_artifact(&a)
        });
        serde_json::from_value(
            json!({"artifactId": aid.as_str(), "versionId": vid.as_str(),
            "sha256": digest, "byteLength": len.to_string(), "mediaType": "text/plain"}),
        )
        .unwrap()
    }
    /// A recorded outcome link on `root` (acceptance evidence).
    fn evidence(&self, root: &Id) -> Id {
        let ctx = self.user();
        let root = RootId(root.clone());
        self.write(move |tx| {
            Ok(record_link(
                tx,
                &ctx,
                NewHistoryLink {
                    root_id: root,
                    kind: LinkKind::Outcome,
                    git: None,
                    artifact: None,
                    target: Some(("check".into(), "run-1".into())),
                    summary: "tests passed".into(),
                    supersedes: None,
                },
            )?
            .expect("recorded")
            .id)
        })
    }
    fn plan(
        &self,
        f: &WorkRecord,
        content: &ArtifactRef,
        requirements: &[(&str, &str)],
    ) -> WorkRecord {
        work_of(applied(self.command(
            &self.user(),
            "features.edit_plan",
            json!({"featureId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
                "plan": draft(content, requirements, &[])}),
        )))
    }
    /// Requests approval of the pending plan and approves it through the decision seam.
    fn approve(&self, f: &WorkRecord) -> WorkRecord {
        let pending = f.pending_plan.clone().expect("pending plan");
        let proposal = match applied(self.command(
            &self.agent(),
            "features.request_plan_approval",
            json!({"featureId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
                "planRevision": pending.revision.as_str(),
                "planDigest": plan_digest(&pending.draft)}),
        )) {
            OperationResult::FeaturesRequestPlanApproval { value } => value,
            other => panic!("{other:?}"),
        };
        let ctx = self.user();
        let pid = proposal.id.clone();
        self.write(move |tx| {
            decide_plan_approval(tx, &ctx, &pid, 1, true)
                .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
        })
    }
    fn eligible(&self, root: &Id, target: Target) -> Vec<String> {
        let (runs, root) = (self.runs.clone(), RootId(root.clone()));
        self.read(move |s| {
            let w = s.sessions().work_root(&root)?.expect("root");
            Ok(runs
                .evaluate(s.runs(), &w, &target, ActivationPurpose::Implementation)?
                .conditions
                .iter()
                .map(|c| c.reason.clone())
                .collect())
        })
    }
}

fn draft(content: &ArtifactRef, requirements: &[(&str, &str)], repos: &[&str]) -> Value {
    json!({
        "content": content,
        "acceptance": [{"id": "A1", "text": "works", "evidenceIds": [], "state": "Unmet"}],
        "repositoryIds": repos,
        "goals": ["ship it"],
        "requirements": requirements.iter().map(|(i, t)| json!({"id": i, "statement": t,
            "status": "Confirmed", "coveredByTaskIds": []})).collect::<Vec<_>>(),
        "acceptanceTests": [],
    })
}

fn recorded(reply: DomainCommandReply) -> DomainDecision {
    match reply {
        CommandReply::Recorded { receipt, .. } => receipt.decision,
        other => panic!("expected a recorded decision, got {other:?}"),
    }
}
fn applied(reply: DomainCommandReply) -> OperationResult {
    match recorded(reply) {
        DomainDecision::Applied { result } => result,
        other => panic!("expected Applied, got {other:?}"),
    }
}
fn unchanged(reply: DomainCommandReply) -> OperationResult {
    match recorded(reply) {
        DomainDecision::NoChange { result } => result,
        other => panic!("expected NoChange, got {other:?}"),
    }
}
fn refused(reply: DomainCommandReply) -> DomainFault {
    match recorded(reply) {
        DomainDecision::Refused { fault } => fault,
        other => panic!("expected Refused, got {other:?}"),
    }
}
fn kind_of(f: &DomainFault) -> &str {
    f.target_kind.as_ref().map_or("", |k| k.0.as_str())
}
fn work_of(r: OperationResult) -> WorkRecord {
    match r {
        OperationResult::TasksCreate { value }
        | OperationResult::TasksEdit { value }
        | OperationResult::TasksMove { value }
        | OperationResult::TasksAssign { value }
        | OperationResult::TasksDependencyLinks { value }
        | OperationResult::TasksAcceptanceEvidence { value }
        | OperationResult::TasksSetFeature { value }
        | OperationResult::IssuesCreate { value }
        | OperationResult::IssuesEdit { value }
        | OperationResult::FeaturesEdit { value }
        | OperationResult::FeaturesEditPlan { value }
        | OperationResult::FeaturesDependencies { value } => value,
        other => panic!("{other:?}"),
    }
}
fn ids(v: &[WorkRecord]) -> Vec<String> {
    v.iter().map(|w| w.title.0.clone()).collect()
}

// ---------------------------------------------------------------------------------------------
// Identity, views and revisions

#[test]
fn a_standalone_task_is_an_issue_with_one_identity_across_views() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &["repo-1", "repo-2"]);
    let f = fx.feature(&p, "Checkout", &["repo-1"]);
    let t = fx.task(&fx.user(), &p, Some(&f), "Pay button", 0);
    // An agent creates through the same operation; a standalone card is the issue view.
    let issue = fx.task(&fx.agent(), &p, None, "Crash on start", 0);
    assert!(matches!(
        issue.kind,
        devworks_core::protocol::records::WorkKind::Issue
    ));
    assert!(matches!(
        t.kind,
        devworks_core::protocol::records::WorkKind::Task
    ));
    assert_eq!(fx.get("issues.get", &issue.id).unwrap().id, issue.id);
    assert_eq!(fx.get("tasks.get", &issue.id).unwrap().id, issue.id);
    // Lists: issues are the standalone cards; tasks every card; never duplicated.
    let (issues, _) = fx.list("issues.list", &p, 50, None, None).unwrap();
    assert_eq!(ids(&issues), ["Crash on start"]);
    let (tasks, _) = fx.list("tasks.list", &p, 50, None, None).unwrap();
    assert_eq!(tasks.len(), 2);
    let (features, _) = fx.list("features.list", &p, 50, None, None).unwrap();
    assert_eq!(ids(&features), ["Checkout"]);
    // Kind-specific entry points refuse the other kind; a project agent sees only its project.
    assert_eq!(
        fx.get("features.get", &t.id).unwrap_err().code,
        DomainErrorCode::UnsupportedTarget
    );
    assert_eq!(
        fx.get("tasks.get", &f.id).unwrap_err().code,
        DomainErrorCode::UnsupportedTarget
    );
    let other = fx.project("Beta", &[]);
    let confined = fx.ctx("agent-w", PrincipalKind::Agent, Some(&other));
    let hidden = fx.query(
        &confined,
        "tasks.get",
        json!({"workId": t.id.as_str(), "detailLevel": "Summary"}),
    );
    assert!(
        matches!(hidden, DomainQueryReply::Rejected { fault, .. } if fault.code == DomainErrorCode::NotFound)
    );
    // Issue creation with an optional feature uses the same membership.
    let linked = work_of(applied(fx.command(
        &fx.user(),
        "issues.create",
        json!({"projectId": p.0.as_str(), "title": "Linked", "description": "",
            "repositoryIds": [], "statusId": Fx::status(&p, "ready"), "priority": 1,
            "featureId": f.id.as_str()}),
    )));
    assert_eq!(linked.parent_id.as_ref(), Some(&f.id));
    // The issue view tags the same identity Issue; the task view shows its membership.
    assert!(matches!(
        linked.kind,
        devworks_core::protocol::records::WorkKind::Issue
    ));
    assert!(matches!(
        fx.get("tasks.get", &linked.id).unwrap().kind,
        devworks_core::protocol::records::WorkKind::Task
    ));
    // Multi-repository context, attribution and history.
    let multi = work_of(applied(fx.edit(
        &fx.user(),
        &issue,
        json!({"repositoryIds": ["repo-2", "repo-1"]}),
    )));
    assert_eq!(multi.repository_ids.0, vec![id("repo-1"), id("repo-2")]);
    let w = fx
        .read({
            let i = issue.id.clone();
            move |s| s.work().work(&i)
        })
        .unwrap();
    assert_eq!(w.created_by, Some(actor("agent-w")));
    assert_eq!(w.updated_by, Some(actor("user-a")));
}

#[test]
fn stale_edits_are_refused_and_a_refusal_writes_nothing() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &["repo-1"]);
    let t = fx.task(&fx.user(), &p, None, "Original", 0);
    let first = work_of(applied(fx.edit(
        &fx.user(),
        &t,
        json!({"title": "Agent edit"}),
    )));
    // A form opened at the earlier revision is refused: no silent last write wins.
    let fault = refused(fx.edit(&fx.user(), &t, json!({"title": "Stale form"})));
    assert_eq!(fault.code, DomainErrorCode::RevisionConflict);
    assert_eq!(fx.fresh(&t).title.0, "Agent edit");
    // A refusal after a valid part of the patch leaves nothing written.
    let fault = refused(fx.edit(
        &fx.user(),
        &first,
        json!({"title": "Half", "repositoryIds": ["repo-9"]}),
    ));
    assert_eq!(fault.code, DomainErrorCode::NotFound);
    let now = fx.fresh(&t);
    assert_eq!(
        (now.title.0.as_str(), now.revision.get()),
        ("Agent edit", 2)
    );
    // An unchanged patch is NoChange at the same revision.
    let same = work_of(unchanged(fx.edit(
        &fx.user(),
        &now,
        json!({"title": "Agent edit"}),
    )));
    assert_eq!(same.revision.get(), 2);
}

#[test]
fn search_folds_unicode_symmetrically_and_cursors_bind_the_order() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    for (title, prio) in [
        ("Äpfel Straße", 5),
        ("ÉCOLE", 4),
        ("plain", 3),
        ("other", 2),
    ] {
        fx.task(&fx.user(), &p, None, title, prio);
    }
    let (hits, _) = fx.list("tasks.list", &p, 50, None, Some("äPFEL")).unwrap();
    assert_eq!(ids(&hits), ["Äpfel Straße"]);
    let (hits, _) = fx
        .list("tasks.list", &p, 50, None, Some("STRASSE"))
        .unwrap();
    assert_eq!(ids(&hits), ["Äpfel Straße"]);
    let (hits, _) = fx.list("tasks.list", &p, 50, None, Some("école")).unwrap();
    assert_eq!(ids(&hits), ["ÉCOLE"]);
    // Paging follows the queue order and continues exactly.
    let (first, next) = fx.list("tasks.list", &p, 2, None, None).unwrap();
    assert_eq!(ids(&first), ["Äpfel Straße", "ÉCOLE"]);
    let next = next.expect("more");
    let (second, end) = fx.list("tasks.list", &p, 2, Some(&next), None).unwrap();
    assert_eq!(ids(&second), ["plain", "other"]);
    assert!(end.is_none());
    // A reprioritization between pages expires the cursor rather than skipping or repeating.
    let plain = second[0].clone();
    applied(fx.edit(&fx.user(), &plain, json!({"priority": 9})));
    let fault = fx.list("tasks.list", &p, 2, Some(&next), None).unwrap_err();
    assert_eq!(fault.code, DomainErrorCode::CursorExpired);
    // A cursor of another query (a different filter) is refused too.
    let fault = fx
        .list("tasks.list", &p, 2, Some(&next), Some("plain"))
        .unwrap_err();
    assert_eq!(fault.code, DomainErrorCode::CursorExpired);
}

// ---------------------------------------------------------------------------------------------
// Blocking links

#[test]
fn links_span_features_and_issues_and_refuse_cycles_without_partial_writes() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    let f1 = fx.feature(&p, "One", &[]);
    let f2 = fx.feature(&p, "Two", &[]);
    let a = fx.task(&fx.user(), &p, Some(&f1), "A", 0);
    let b = fx.task(&fx.user(), &p, Some(&f2), "B", 0);
    let c = fx.task(&fx.user(), &p, None, "C issue", 0);
    // A blocks B (cross-feature), B blocks C (standalone).
    let b = work_of(applied(fx.links(&b, &[&a])));
    let c = work_of(applied(fx.links(&c, &[&b])));
    assert_eq!(b.dependency_ids.0, vec![a.id.clone()]);
    assert_eq!(fx.fresh(&a).dependent_ids.0, vec![b.id.clone()]);
    // Links never change membership.
    assert_eq!(fx.fresh(&b).parent_id.as_ref(), Some(&f2.id));
    // C → A would close A → B → C → A: refused, nothing written.
    let a_now = fx.fresh(&a);
    let fault = refused(fx.links(&a_now, &[&c]));
    assert_eq!(kind_of(&fault), "dependency_cycle");
    assert!(fx.fresh(&a).dependency_ids.0.is_empty());
    assert_eq!(fx.fresh(&a).revision, a_now.revision);
    // Self and duplicate links never reach the store (strict decoding).
    let selfish = fx.links(&a_now, &[&a_now]);
    assert!(!matches!(selfish, CommandReply::Recorded { .. }));
    let dup = fx.links(&c, &[&b, &b]);
    assert!(!matches!(dup, CommandReply::Recorded { .. }));
    // Another project's work cannot be linked (and is not disclosed).
    let q = fx.project("Beta", &[]);
    let foreign = fx.task(&fx.user(), &q, None, "Foreign", 0);
    let fault = refused(fx.links(&a_now, &[&foreign]));
    assert_eq!(fault.code, DomainErrorCode::NotFound);
    // A card links to cards and a feature to features.
    let fault = refused(fx.links(&a_now, &[&f2]));
    assert_eq!(kind_of(&fault), "link_kind");
    // Explicit empty replacement removes only C's incoming links.
    let c = work_of(applied(fx.links(&c, &[])));
    assert!(c.dependency_ids.0.is_empty());
    assert_eq!(fx.fresh(&b).dependency_ids.0, vec![a.id.clone()]);
    // Feature dependencies use the same graph rules.
    let f2r = fx.fresh(&f2);
    let f2r = work_of(applied(fx.command(
        &fx.agent(),
        "features.dependencies",
        json!({"featureId": f2r.id.as_str(), "expectedRevision": f2r.revision.as_str(),
            "replacement": {"edges": [{"predecessor": f1.id.as_str(), "successor": f2r.id.as_str()}]}}),
    )));
    assert_eq!(f2r.dependency_ids.0, vec![f1.id.clone()]);
    let f1r = fx.fresh(&f1);
    let fault = refused(fx.command(
        &fx.agent(),
        "features.dependencies",
        json!({"featureId": f1r.id.as_str(), "expectedRevision": f1r.revision.as_str(),
            "replacement": {"edges": [{"predecessor": f2.id.as_str(), "successor": f1r.id.as_str()}]}}),
    ));
    assert_eq!(kind_of(&fault), "dependency_cycle");
}

#[test]
fn concurrent_link_edits_cannot_form_a_cycle_through_the_write_owner() {
    let fx = Arc::new(Fx::new());
    let p = fx.project("Alpha", &[]);
    let a = fx.task(&fx.user(), &p, None, "A", 0);
    let b = fx.task(&fx.user(), &p, None, "B", 0);
    // Two individually valid edits (A blocks B, B blocks A) race on separate threads: exactly
    // one commits, the other observes it inside its unit of work and is refused.
    let (fa, fb) = (fx.clone(), fx.clone());
    let (a1, b1, a2, b2) = (a.clone(), b.clone(), a.clone(), b.clone());
    let t1 = std::thread::spawn(move || recorded(fa.links(&b1, &[&a1])));
    let t2 = std::thread::spawn(move || recorded(fb.links(&a2, &[&b2])));
    let outcomes = [t1.join().unwrap(), t2.join().unwrap()];
    let applied_n = outcomes
        .iter()
        .filter(|d| matches!(d, DomainDecision::Applied { .. }))
        .count();
    assert_eq!(applied_n, 1, "{outcomes:?}");
    let a = fx.fresh(&a);
    let b = fx.fresh(&b);
    assert_eq!(a.dependency_ids.0.len() + b.dependency_ids.0.len(), 1);
}

#[test]
fn unsatisfied_prerequisites_hold_dispatch_until_verified_completion() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    let pre = fx.task(&fx.user(), &p, None, "Prerequisite", 0);
    let dep = fx.task(&fx.user(), &p, None, "Dependent", 9);
    applied(fx.links(&dep, &[&pre]));
    let held = fx.eligible(&dep.id, Target::Root);
    assert!(held.iter().any(|r| r.contains("prerequisite")), "{held:?}");
    // Criteria, then a manual Done: still unverified, still held.
    let pre = work_of(applied(fx.edit(
        &fx.user(),
        &pre,
        json!({"acceptance": [{"id": "C1", "text": "passes", "evidenceIds": [], "state": "Unmet"}]}),
    )));
    let wf = fx.queue(&p, WORKFLOW_QUEUE).to_string();
    let done = work_of(applied(fx.command(
        &fx.user(),
        "tasks.move",
        json!({"taskId": pre.id.as_str(), "expectedRevision": pre.revision.as_str(),
            "statusId": Fx::status(&p, "done"), "expectedWorkflowRevision": wf}),
    )));
    let stored = {
        let i = done.id.clone();
        fx.read(move |s| s.work().work(&i)).unwrap()
    };
    assert_eq!(stored.completion.as_deref(), Some(MANUAL));
    assert!(
        fx.eligible(&dep.id, Target::Root)
            .iter()
            .any(|r| r.contains("prerequisite"))
    );
    // Verified evidence completes it: the dependent is no longer held by it.
    let ev = fx.evidence(&done.id);
    let verified = work_of(applied(fx.command(
        &fx.agent(),
        "tasks.acceptance_evidence",
        json!({"taskId": done.id.as_str(), "expectedRevision": done.revision.as_str(),
            "criteria": [{"id": "C1", "text": "passes", "evidenceIds": [ev.as_str()], "state": "Met"}]}),
    )));
    let stored = {
        let i = verified.id.clone();
        fx.read(move |s| s.work().work(&i)).unwrap()
    };
    assert_eq!(stored.completion.as_deref(), Some(VERIFIED));
    assert!(
        !fx.eligible(&dep.id, Target::Root)
            .iter()
            .any(|r| r.contains("prerequisite"))
    );
    // Unknown evidence is refused; completed work's prerequisites are not edited.
    let fault = refused(fx.command(
        &fx.agent(),
        "tasks.acceptance_evidence",
        json!({"taskId": verified.id.as_str(), "expectedRevision": verified.revision.as_str(),
            "criteria": [{"id": "C1", "text": "passes", "evidenceIds": ["nope"], "state": "Met"}]}),
    ));
    assert_eq!(fault.code, DomainErrorCode::NotFound);
    let other = fx.task(&fx.user(), &p, None, "Other", 0);
    let fault = refused(fx.links(&fx.fresh(&verified), &[&other]));
    assert_eq!(kind_of(&fault), "work_completed");
    // A stale workflow revision refuses a move.
    let fault = refused(fx.command(
        &fx.user(),
        "tasks.move",
        json!({"taskId": other.id.as_str(), "expectedRevision": other.revision.as_str(),
            "statusId": Fx::status(&p, "ready"), "expectedWorkflowRevision": "7"}),
    ));
    assert_eq!(fault.code, DomainErrorCode::RevisionConflict);
}

// ---------------------------------------------------------------------------------------------
// Priority and order

#[test]
fn prioritize_moves_selected_cards_to_the_front_of_their_bucket() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    let f = fx.feature(&p, "F", &[]);
    let a = fx.task(&fx.user(), &p, Some(&f), "a", 1);
    let b = fx.task(&fx.user(), &p, Some(&f), "b", 1);
    let c = fx.task(&fx.user(), &p, Some(&f), "c", 1);
    let d = fx.task(&fx.user(), &p, Some(&f), "d", 5);
    let q = feature_queue(&f.id);
    let rev = fx.queue(&p, &q);
    let m = match applied(fx.command(
        &fx.agent(),
        "tasks.prioritize",
        json!({"projectId": p.0.as_str(), "featureId": f.id.as_str(),
            "expectedQueueRevision": rev.to_string(), "priority": 1,
            "orderedTaskIds": [c.id.as_str(), b.id.as_str()]}),
    )) {
        OperationResult::TasksPrioritize { value } => value,
        other => panic!("{other:?}"),
    };
    assert!(m.changed);
    let (all, _) = fx.list("tasks.list", &p, 50, None, None).unwrap();
    assert_eq!(ids(&all), ["d", "c", "b", "a"]);
    // The other bucket keeps its priority; a stale queue revision is refused.
    assert_eq!(fx.fresh(&d).priority, 5);
    let fault = refused(fx.command(
        &fx.user(),
        "tasks.prioritize",
        json!({"projectId": p.0.as_str(), "featureId": f.id.as_str(),
            "expectedQueueRevision": rev.to_string(), "priority": 1,
            "orderedTaskIds": [a.id.as_str()]}),
    ));
    assert_eq!(fault.code, DomainErrorCode::RevisionConflict);
    // Standalone ordering uses the mixed project queue; a feature child is not a member.
    let i1 = fx.task(&fx.user(), &p, None, "i1", 0);
    let rev = fx.queue(&p, PROJECT_QUEUE);
    let fault = refused(fx.command(
        &fx.user(),
        "tasks.prioritize",
        json!({"projectId": p.0.as_str(), "expectedQueueRevision": rev.to_string(),
            "priority": 3, "orderedTaskIds": [a.id.as_str()]}),
    ));
    assert_eq!(kind_of(&fault), "queue_member");
    applied(fx.command(
        &fx.user(),
        "issues.prioritize",
        json!({"projectId": p.0.as_str(), "expectedQueueRevision": rev.to_string(),
            "orderedWorkIds": [i1.id.as_str()]}),
    ));
    let rev = fx.queue(&p, PROJECT_QUEUE);
    applied(fx.command(
        &fx.user(),
        "features.prioritize",
        json!({"projectId": p.0.as_str(), "expectedQueueRevision": rev.to_string(),
            "orderedWorkIds": [f.id.as_str()]}),
    ));
}

// ---------------------------------------------------------------------------------------------
// Membership

#[test]
fn unlink_pauses_until_continue_relink_keeps_it_and_transfer_creates_none() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    let f1 = fx.feature(&p, "One", &[]);
    let f2 = fx.feature(&p, "Two", &[]);
    let t = fx.task(&fx.user(), &p, Some(&f1), "Card", 0);
    let set = |w: &WorkRecord, f: Option<&WorkRecord>| {
        let mut body = json!({"taskId": w.id.as_str(), "expectedRevision": w.revision.as_str(),
            "clearFeature": f.is_none()});
        if let Some(f) = f {
            body["featureId"] = json!(f.id.as_str());
        }
        fx.command(&fx.agent(), "tasks.set_feature", body)
    };
    let holds = |w: &Id| {
        let w = RootId(w.clone());
        fx.read(move |s| s.runs().blockers(&BlockerFilter::RootLevel(w), None, 100))
    };
    // A direct transfer keeps its identity and creates no pause.
    let moved = work_of(applied(set(&t, Some(&f2))));
    assert_eq!(
        (moved.id.clone(), moved.parent_id.clone()),
        (t.id.clone(), Some(f2.id.clone()))
    );
    assert!(holds(&t.id).is_empty());
    // Unlinking installs a durable human pause on the card.
    let free = work_of(applied(set(&moved, None)));
    assert!(free.parent_id.is_none());
    let pause = holds(&t.id);
    assert_eq!(pause.len(), 1);
    assert_eq!(pause[0].cause, HoldCause::HumanPause);
    assert!(!fx.eligible(&t.id, Target::Root).is_empty());
    // Relinking never clears it.
    let relinked = work_of(applied(set(&free, Some(&f1))));
    assert_eq!(holds(&t.id).len(), 1);
    // Its own Continue (a human's) clears it even while linked.
    let control = {
        let r = RootId(t.id.clone());
        fx.read(move |s| devworks_core::services::runs::control_of(s.runs(), &r))
    };
    let frame = json!({"protocol": "1.0-draft.1", "requestKey": "continue-1", "command": {
        "kind": "runs.continue", "rootId": t.id.as_str(),
        "expectedControlRevision": control.revision.to_string(),
        "coveredHolds": [{"holdId": pause[0].id.as_str(), "revision": pause[0].revision.to_string()}]}});
    let authority = fx.authority();
    let gateway = Gateway {
        authority: &authority,
        store: &*fx.store,
        ids: &*fx.h.ids,
    };
    let r = fx
        .cases
        .command(
            &gateway,
            &fx.user(),
            "runs.continue",
            frame.to_string().as_bytes(),
        )
        .unwrap();
    assert!(matches!(recorded(r), DomainDecision::Applied { .. }));
    assert!(holds(&t.id).is_empty());
    // Exactly one of set/clear; a closed feature accepts no member; another project's feature
    // is not found.
    let bad = fx.command(
        &fx.agent(),
        "tasks.set_feature",
        json!({"taskId": relinked.id.as_str(), "expectedRevision": relinked.revision.as_str(),
            "clearFeature": false}),
    );
    assert!(!matches!(bad, CommandReply::Recorded { .. }));
    let q = fx.project("Beta", &[]);
    let fq = fx.feature(&q, "Elsewhere", &[]);
    let fault = refused(set(&fx.fresh(&relinked), Some(&fq)));
    assert_eq!(fault.code, DomainErrorCode::NotFound);
}

// ---------------------------------------------------------------------------------------------
// Plans and readiness

#[test]
fn pending_plans_keep_the_active_plan_until_a_human_approves_the_exact_draft() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &["repo-1", "repo-2"]);
    let f = fx.feature(&p, "Checkout", &["repo-1"]);
    let doc = fx.artifact(&p, 1);
    let f = fx.plan(&f, &doc, &[("E1", "pay"), ("E2", "refund")]);
    assert!(f.plan.is_none() && f.approval.is_none());
    let f = fx.approve(&f);
    let active = f.approval.clone().expect("approved");
    assert_eq!(f.requirements.0.len(), 2);
    assert!(f.pending_plan.is_none());
    // A replacement draft leaves the active plan, its approval and scope as they were.
    let doc2 = fx.artifact(&p, 2);
    let f = fx.plan(
        &f,
        &doc2,
        &[("E1", "pay"), ("E2", "refund"), ("E3", "receipts")],
    );
    assert_eq!(f.approval.as_ref().unwrap().plan_digest, active.plan_digest);
    assert_eq!(f.requirements.0.len(), 2);
    assert!(f.pending_plan.is_some());
    // An active requirement left out of a replacement needs an explicit Dropped decision.
    let fault = refused(fx.command(
        &fx.user(),
        "features.edit_plan",
        json!({"featureId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
            "plan": draft(&doc2, &[("E1", "pay")], &[])}),
    ));
    assert_eq!(kind_of(&fault), "requirement_missing");
    // Generic edits cannot change an approved feature's scope.
    let fault = refused(fx.command(
        &fx.user(),
        "features.edit",
        json!({"workId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
            "changes": {"repositoryIds": ["repo-2"]}}),
    ));
    assert_eq!(kind_of(&fault), "plan_scope");
    // A request, then a draft change: the request is stale and cannot approve unseen content.
    let pending = f.pending_plan.clone().unwrap();
    let proposal = match applied(fx.command(
        &fx.agent(),
        "features.request_plan_approval",
        json!({"featureId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
            "planRevision": pending.revision.as_str(), "planDigest": plan_digest(&pending.draft)}),
    )) {
        OperationResult::FeaturesRequestPlanApproval { value } => value,
        other => panic!("{other:?}"),
    };
    let f = fx.fresh(&f);
    let doc3 = fx.artifact(&p, 3);
    let f = fx.plan(
        &f,
        &doc3,
        &[("E1", "pay"), ("E2", "refund"), ("E3", "receipts!")],
    );
    let ctx = fx.user();
    let pid = proposal.id.clone();
    let stale = fx.write(move |tx| Ok(decide_plan_approval(tx, &ctx, &pid, 1, true).is_err()));
    assert!(stale);
    // An agent never decides; a rejection leaves the active plan approved.
    let pending = f.pending_plan.clone().unwrap();
    let proposal = match applied(fx.command(
        &fx.agent(),
        "features.request_plan_approval",
        json!({"featureId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
            "planRevision": pending.revision.as_str(), "planDigest": plan_digest(&pending.draft)}),
    )) {
        OperationResult::FeaturesRequestPlanApproval { value } => value,
        other => panic!("{other:?}"),
    };
    let agent = fx.agent();
    let pid = proposal.id.clone();
    assert!(fx.write(move |tx| Ok(decide_plan_approval(tx, &agent, &pid, 1, true).is_err())));
    let ctx = fx.user();
    let pid = proposal.id.clone();
    let rejected = fx.write(move |tx| {
        decide_plan_approval(tx, &ctx, &pid, 1, false)
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    });
    assert_eq!(rejected.approval.unwrap().plan_digest, active.plan_digest);
    assert_eq!(rejected.requirements.0.len(), 2);
    // A decision binds the exact draft it was requested for: a draft that changed without the
    // request being superseded (defence in depth) is refused, never approved unseen.
    let f = fx.fresh(&f);
    let pending = f.pending_plan.clone().unwrap();
    let proposal = match applied(fx.command(
        &fx.agent(),
        "features.request_plan_approval",
        json!({"featureId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
            "planRevision": pending.revision.as_str(), "planDigest": plan_digest(&pending.draft)}),
    )) {
        OperationResult::FeaturesRequestPlanApproval { value } => value,
        other => panic!("{other:?}"),
    };
    let fid = f.id.clone();
    fx.write(move |tx| {
        let mut slot = tx.work().feature_plan(&fid)?.unwrap();
        let before = slot.revision;
        let mut body = slot.pending.clone().unwrap();
        body["digest"] = json!("c".repeat(64));
        slot.pending = Some(body);
        slot.revision = before + 1;
        tx.work().put_feature_plan(&slot, Some(before))
    });
    let ctx = fx.user();
    let pid = proposal.id.clone();
    let refused_unseen = fx.write(move |tx| {
        Ok(match decide_plan_approval(tx, &ctx, &pid, 1, true) {
            Err(devworks_core::services::sessions::Failure::Refused(r)) => r.target_kind,
            other => panic!("{:?}", other.map(|w| w.revision)),
        })
    });
    assert_eq!(refused_unseen, "plan_changed");
}

#[test]
fn a_card_leaving_its_feature_keeps_the_next_draft_savable_and_surfaces_lost_coverage() {
    use devworks_core::protocol::records::FindingState;
    use devworks_core::services::runs::scope::Readiness;
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    let f = fx.feature(&p, "Checkout", &[]);
    let doc = fx.artifact(&p, 1);
    let pay = fx.task(&fx.user(), &p, Some(&f), "Pay", 0);
    let ship = fx.task(&fx.user(), &p, Some(&f), "Ship", 0);
    let stray = fx.task(&fx.user(), &p, None, "Stray", 0);
    // E1 is covered by Pay alone, E2 by Pay and Ship.
    let mut plan = draft(&doc, &[("E1", "pay"), ("E2", "ship")], &[]);
    plan["requirements"][0]["coveredByTaskIds"] = json!([pay.id.as_str()]);
    plan["requirements"][1]["coveredByTaskIds"] = json!([pay.id.as_str(), ship.id.as_str()]);
    let f = fx.fresh(&f);
    let f = work_of(applied(fx.command(
        &fx.user(),
        "features.edit_plan",
        json!({"featureId": f.id.as_str(), "expectedRevision": f.revision.as_str(), "plan": plan}),
    )));
    let f = fx.approve(&f);
    let active = f.approval.clone().unwrap();
    let f = work_of(applied(fx.command_at(
        &fx.agent(),
        "features.edit",
        json!({"workId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
            "changes": {"readiness": {"kind": "Assess",
                "planRevision": active.plan_revision.as_str(),
                "planDigest": String::from(active.plan_digest.clone()),
                "summary": "assessed", "findings": []}}}),
        Some(2),
    )));
    assert!(
        fx.get_at("features.get", &f.id, Some(2))
            .unwrap()
            .readiness
            .unwrap()
            .ready
    );
    // Pay leaves the feature.
    let pay = fx.fresh(&pay);
    applied(fx.command(
        &fx.user(),
        "tasks.set_feature",
        json!({"taskId": pay.id.as_str(), "expectedRevision": pay.revision.as_str(),
            "clearFeature": true}),
    ));
    // E1 lost its only covering card: an open blocking finding of E1 alone. E2 keeps Ship.
    let r = fx
        .get_at("features.get", &f.id, Some(2))
        .unwrap()
        .readiness
        .unwrap();
    let lost: Vec<_> = r
        .findings
        .0
        .iter()
        .filter(|x| x.blocking && matches!(x.state, FindingState::Open))
        .collect();
    assert_eq!(lost.len(), 1, "{:?}", r.findings);
    assert_eq!(lost[0].requirement_ids.0, vec![id("E1")]);
    assert_eq!(lost[0].scope_key.as_ref(), Some(&id("E1")));
    let plan_state = {
        let (runs, fid) = (fx.runs.clone(), RootId(f.id.clone()));
        fx.read(move |s| {
            let w = s.sessions().work_root(&fid)?.unwrap();
            runs.scope().plan(s.runs(), &w)
        })
    };
    assert!(matches!(
        plan_state.unit(Some("E1")).unwrap().readiness,
        Readiness::Blocked { .. }
    ));
    assert!(plan_state.unit(Some("E2")).unwrap().readiness.is_current());
    assert!(plan_state.unit(None).unwrap().readiness.is_current());
    // The next draft, submitted as an editor round-trips the stored requirements, is savable:
    // the departed card leaves its coverage; the active plan's history is as recorded.
    let f = fx.fresh(&f);
    let resubmit = json!({"content": doc, "acceptance": f.acceptance.0,
        "repositoryIds": [], "goals": ["ship it"], "requirements": f.requirements.0,
        "acceptanceTests": []});
    let saved = work_of(applied(fx.command(
        &fx.user(),
        "features.edit_plan",
        json!({"featureId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
            "plan": resubmit}),
    )));
    let draft_reqs = saved.pending_plan.unwrap().draft.requirements.0;
    assert!(draft_reqs[0].covered_by_task_ids.0.is_empty());
    assert_eq!(draft_reqs[1].covered_by_task_ids.0, vec![ship.id.clone()]);
    assert_eq!(
        saved.requirements.0[0].covered_by_task_ids.0,
        vec![pay.id.clone()]
    );
    // A card the stored drafts never recorded is still refused.
    let mut wrong = draft(&doc, &[("E1", "pay"), ("E2", "ship")], &[]);
    wrong["requirements"][0]["coveredByTaskIds"] = json!([stray.id.as_str()]);
    let fault = refused(fx.command(
        &fx.user(),
        "features.edit_plan",
        json!({"featureId": saved.id.as_str(), "expectedRevision": saved.revision.as_str(),
            "plan": wrong}),
    ));
    assert_eq!(fault.code, DomainErrorCode::NotFound);
}

/// Coverage-loss fixtures: a feature whose plan's E1 is covered by `covering`, approved and
/// assessed (no findings).
struct Coverage {
    fx: Fx,
    f: WorkRecord,
    doc: ArtifactRef,
}
impl Coverage {
    fn new(cards: &[&str], covering: &[usize]) -> (Self, Vec<WorkRecord>) {
        let fx = Fx::new();
        let p = fx.project("Alpha", &[]);
        let f = fx.feature(&p, "Checkout", &[]);
        let doc = fx.artifact(&p, 1);
        let cards: Vec<WorkRecord> = cards
            .iter()
            .map(|t| fx.task(&fx.user(), &p, Some(&f), t, 0))
            .collect();
        let c = Self {
            fx,
            f: f.clone(),
            doc,
        };
        let f = c.edit_plan(
            &c.fx.fresh(&f),
            &covering.iter().map(|i| &cards[*i]).collect::<Vec<_>>(),
        );
        let f = c.fx.approve(&f);
        let active = f.approval.clone().unwrap();
        let f = c.assess(
            &f,
            active.plan_revision.as_str(),
            String::from(active.plan_digest.clone()),
        );
        (Self { f, ..c }, cards)
    }
    /// Saves a draft whose E1 is covered by `covering`.
    fn edit_plan(&self, f: &WorkRecord, covering: &[&WorkRecord]) -> WorkRecord {
        let mut plan = draft(&self.doc, &[("E1", "pay")], &[]);
        plan["requirements"][0]["coveredByTaskIds"] =
            json!(covering.iter().map(|w| w.id.as_str()).collect::<Vec<_>>());
        work_of(applied(self.fx.command(
            &self.fx.user(),
            "features.edit_plan",
            json!({"featureId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
                "plan": plan}),
        )))
    }
    fn assess(&self, f: &WorkRecord, rev: &str, digest: String) -> WorkRecord {
        let mut change = json!({"kind": "Assess", "planRevision": rev, "planDigest": digest,
            "summary": "assessed", "findings": []});
        if let Some(r) = self.revision() {
            change["expectedAssessmentRevision"] = json!(r);
        }
        let f = self.fx.fresh(f);
        work_of(applied(self.fx.command_at(
            &self.fx.agent(),
            "features.edit",
            json!({"workId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
                "changes": {"readiness": change}}),
            Some(2),
        )))
    }
    fn shown(&self) -> WorkRecord {
        self.fx.get_at("features.get", &self.f.id, Some(2)).unwrap()
    }
    fn revision(&self) -> Option<String> {
        let f = self.shown();
        f.readiness
            .or(f.pending_readiness)
            .map(|r| r.revision.as_str().to_owned())
    }
    fn set_feature(&self, card: &WorkRecord, feature: Option<&WorkRecord>) {
        let card = self.fx.fresh(card);
        let mut body = json!({"taskId": card.id.as_str(),
            "expectedRevision": card.revision.as_str(), "clearFeature": feature.is_none()});
        if let Some(f) = feature {
            body["featureId"] = json!(f.id.as_str());
        }
        applied(self.fx.command(&self.fx.user(), "tasks.set_feature", body));
    }
    fn e1(&self) -> devworks_core::services::runs::scope::Readiness {
        let (runs, fid) = (self.fx.runs.clone(), RootId(self.f.id.clone()));
        self.fx
            .read(move |s| {
                let w = s.sessions().work_root(&fid)?.unwrap();
                runs.scope().plan(s.runs(), &w)
            })
            .unit(Some("E1"))
            .unwrap()
            .readiness
            .clone()
    }
    fn open_ids(r: &devworks_core::protocol::records::ReadinessAssessment) -> Vec<String> {
        r.findings
            .0
            .iter()
            .filter(|x| {
                matches!(
                    x.state,
                    devworks_core::protocol::records::FindingState::Open
                )
            })
            .map(|x| x.id.as_str().to_owned())
            .collect()
    }
    fn decide(&self, approve: bool) {
        let f = self.fx.fresh(&self.f);
        let pending = f.pending_plan.clone().expect("pending plan");
        let proposal = match applied(self.fx.command(
            &self.fx.agent(),
            "features.request_plan_approval",
            json!({"featureId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
                "planRevision": pending.revision.as_str(),
                "planDigest": plan_digest(&pending.draft)}),
        )) {
            OperationResult::FeaturesRequestPlanApproval { value } => value,
            other => panic!("{other:?}"),
        };
        let ctx = self.fx.user();
        self.fx.write(move |tx| {
            decide_plan_approval(tx, &ctx, &proposal.id, 1, approve)
                .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
        });
    }
}

#[test]
fn a_coverage_gap_only_the_pending_draft_has_never_holds_the_active_plan() {
    use devworks_core::services::runs::scope::Readiness;
    // The active plan covers E1 with Pay; a replacement draft covers it with Wallet instead.
    let (c, cards) = Coverage::new(&["Pay", "Wallet"], &[0]);
    let (pay, wallet) = (&cards[0], &cards[1]);
    let f = c.edit_plan(&c.fx.fresh(&c.f), &[wallet]);
    let pending = f.pending_plan.clone().unwrap();
    c.assess(&f, pending.revision.as_str(), plan_digest(&pending.draft));
    // Wallet leaves: only the replacement lost E1's coverage.
    c.set_feature(wallet, None);
    let shown = c.shown();
    assert!(
        Coverage::open_ids(&shown.readiness.clone().unwrap()).is_empty(),
        "the active plan still covers E1 with {}",
        pay.id.as_str()
    );
    assert_eq!(
        Coverage::open_ids(&shown.pending_readiness.clone().unwrap()).len(),
        1,
        "the replacement's gap is recorded on the replacement"
    );
    assert!(c.e1().is_current());
    // Rejecting the replacement leaves the active plan unblocked.
    c.decide(false);
    assert!(c.e1().is_current());
    assert!(Coverage::open_ids(&c.shown().readiness.unwrap()).is_empty());
    // Adopting it keeps the finding: E1 is blocked under the adopted plan.
    c.decide(true);
    let r = c.shown().readiness.unwrap();
    assert_eq!(Coverage::open_ids(&r).len(), 1, "{:?}", r.findings);
    assert!(matches!(c.e1(), Readiness::Blocked { .. }));
}

/// A settled, non-blocking finding (it fills an assessment's finding list).
fn settled_finding(i: usize) -> Value {
    json!({"id": format!("S{i}"), "disposition": "ResolvedDecision", "blocking": false,
        "prerequisite": false, "state": "Resolved", "requirementIds": [],
        "affectedWorkIds": [], "summary": "settled", "missing": "", "consequence": "",
        "attempts": [], "insufficiency": "", "closureCondition": "", "decision": "decided",
        "confidence": "High", "evidence": [], "closureEvidence": []})
}

#[test]
fn a_coverage_loss_the_full_assessment_cannot_record_refuses_the_membership_change() {
    let (c, cards) = Coverage::new(&["Pay"], &[0]);
    let pay = &cards[0];
    // The active assessment holds its limit of findings (all settled).
    let active = c.shown().approval.unwrap();
    let f = c.fx.fresh(&c.f);
    work_of(applied(c.fx.command_at(
        &c.fx.agent(),
        "features.edit",
        json!({"workId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
            "changes": {"readiness": {"kind": "Assess",
                "planRevision": active.plan_revision.as_str(),
                "planDigest": String::from(active.plan_digest.clone()),
                "expectedAssessmentRevision": c.revision().unwrap(), "summary": "full",
                "findings": (0..128).map(settled_finding).collect::<Vec<_>>()}}}),
        Some(2),
    )));
    assert_eq!(c.shown().readiness.unwrap().findings.0.len(), 128);
    let readiness = c.revision();
    // Pay is E1's only covering card: its departure cannot be recorded, so it is refused and
    // nothing of it is written (the card stays, unpaused; the requirement stays covered).
    let before = c.fx.fresh(pay);
    let fault = refused(c.fx.command(
        &c.fx.user(),
        "tasks.set_feature",
        json!({"taskId": before.id.as_str(), "expectedRevision": before.revision.as_str(),
            "clearFeature": true}),
    ));
    assert_eq!(kind_of(&fault), "finding_limit");
    let after = c.fx.fresh(pay);
    assert_eq!(
        (after.parent_id.clone(), after.revision.clone()),
        (Some(c.f.id.clone()), before.revision.clone())
    );
    let holds = {
        let w = RootId(pay.id.clone());
        c.fx.read(move |s| s.runs().blockers(&BlockerFilter::RootLevel(w), None, 100))
    };
    assert!(holds.is_empty(), "no unlink pause was written");
    assert_eq!(c.revision(), readiness);
    assert!(c.e1().is_current());
}

#[test]
fn adopting_a_replacement_whose_assessment_cannot_take_a_carried_finding_is_refused() {
    let (c, cards) = Coverage::new(&["Pay", "Wallet"], &[0, 1]);
    // An open finding of the active plan (Pay leaves, then Wallet: E1 lost its coverage).
    c.set_feature(&cards[0], None);
    c.set_feature(&cards[1], None);
    let open = Coverage::open_ids(&c.shown().readiness.unwrap());
    assert_eq!(open.len(), 1);
    // A replacement draft, assessed; its stored assessment is then made to hold the limit of
    // findings without the active plan's open one (defence in depth: the service keeps it).
    let f = c.edit_plan(&c.fx.fresh(&c.f), &[]);
    let pending = f.pending_plan.clone().unwrap();
    c.assess(&f, pending.revision.as_str(), plan_digest(&pending.draft));
    let fid = c.f.id.clone();
    c.fx.write(move |tx| {
        let stored = tx.work().readiness(&fid)?.unwrap();
        let mut body = stored.body.clone();
        body["pending"]["findings"] = json!((0..128).map(settled_finding).collect::<Vec<_>>());
        tx.work().put_readiness(
            &StoredReadiness {
                feature_id: fid.clone(),
                revision: stored.revision + 1,
                body,
            },
            Some(stored.revision),
        )
    });
    // Adoption would carry the open finding past the limit: it is refused, nothing adopted.
    let f = c.fx.fresh(&c.f);
    let proposal = match applied(c.fx.command(
        &c.fx.agent(),
        "features.request_plan_approval",
        json!({"featureId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
            "planRevision": pending.revision.as_str(),
            "planDigest": plan_digest(&pending.draft)}),
    )) {
        OperationResult::FeaturesRequestPlanApproval { value } => value,
        other => panic!("{other:?}"),
    };
    // The approvals seam's caller rolls its unit of work back on a refusal.
    let ctx = c.fx.user();
    let decided: Result<(), StorageError> = run_write(
        &*c.fx.store,
        WriteOptions::default(),
        move |tx| match decide_plan_approval(tx, &ctx, &proposal.id, 1, true) {
            Err(devworks_core::services::sessions::Failure::Refused(r)) => {
                Err(StorageError::InvalidRecord(r.target_kind.to_string()))
            }
            other => panic!("{:?}", other.map(|w| w.revision)),
        },
    );
    assert!(
        matches!(&decided, Err(StorageError::InvalidRecord(k)) if k == "finding_limit"),
        "{decided:?}"
    );
    let shown = c.shown();
    assert!(shown.pending_plan.is_some(), "nothing was adopted");
    assert_eq!(
        Coverage::open_ids(&shown.readiness.unwrap()),
        open,
        "the active plan keeps its open finding"
    );
}

#[test]
fn a_card_leaving_again_after_its_coverage_finding_closed_blocks_the_requirement_again() {
    use devworks_core::protocol::records::FindingState;
    use devworks_core::services::runs::scope::Readiness;
    let (c, cards) = Coverage::new(&["Pay"], &[0]);
    let pay = &cards[0];
    // Pay leaves: E1 is blocked by its coverage finding.
    c.set_feature(pay, None);
    assert!(matches!(c.e1(), Readiness::Blocked { .. }));
    let first = Coverage::open_ids(&c.shown().readiness.unwrap());
    assert_eq!(first.len(), 1);
    // Pay is linked back and the finding is closed: E1 is current again.
    c.set_feature(pay, Some(&c.f));
    let f = c.fx.fresh(&c.f);
    applied(c.fx.command_at(
        &c.fx.user(),
        "features.edit",
        json!({"workId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
            "changes": {"readiness": {"kind": "CloseFinding",
                "expectedAssessmentRevision": c.revision().unwrap(),
                "findingId": first[0], "note": "Pay is back in the feature", "evidence": []}}}),
        Some(2),
    ));
    assert!(c.e1().is_current());
    // Pay leaves again: changed scope again, so E1 is blocked again by a new occurrence.
    c.set_feature(pay, None);
    assert!(matches!(c.e1(), Readiness::Blocked { .. }));
    let r = c.shown().readiness.unwrap();
    let open = Coverage::open_ids(&r);
    assert_eq!(open.len(), 1);
    assert_ne!(open[0], first[0], "a new occurrence");
    // The earlier closure is kept, in the assessment and in history.
    let earlier = r
        .findings
        .0
        .iter()
        .find(|x| x.id.as_str() == first[0])
        .expect("the closed finding");
    assert!(matches!(earlier.state, FindingState::Resolved));
    assert_eq!(
        earlier.closure_note.as_ref().map(|n| n.0.as_str()),
        Some("Pay is back in the feature")
    );
    let fid = RootId(c.f.id.clone());
    let kinds: Vec<String> = c.fx.read(move |s| {
        Ok(s.history()
            .page(&EventFilter::Root(fid), 0, 500)?
            .events
            .into_iter()
            .map(|e| e.kind)
            .collect())
    });
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "ReadinessCoverageLost")
            .count(),
        2
    );
    assert!(kinds.iter().any(|k| k == "ReadinessFindingClosed"));
}

#[test]
fn readiness_is_bound_to_the_plan_and_invalidates_only_changed_units() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    let f = fx.feature(&p, "Checkout", &[]);
    let doc = fx.artifact(&p, 1);
    let f = fx.plan(&f, &doc, &[("E1", "pay"), ("E2", "refund")]);
    let card = fx.task(&fx.user(), &p, Some(&f), "Refund API", 0);
    let f = fx.fresh(&f);
    let pending = f.pending_plan.clone().unwrap();
    let finding = |fid: &str, scope: &str, evidence: Vec<ArtifactRef>| {
        json!({"id": fid, "disposition": "NeedsUserDecision", "blocking": true,
            "prerequisite": false, "state": "Open", "requirementIds": [scope], "scopeKey": scope,
            "affectedWorkIds": [], "summary": "processor unknown",
            "missing": "which payment processor", "consequence": "cannot verify refunds",
            "attempts": ["read the repository conventions"],
            "insufficiency": "a product choice", "closureCondition": "the processor is named",
            "evidence": evidence, "question": "Stripe or Adyen?", "recommendation": "Stripe",
            "closureEvidence": []})
    };
    let assess = |f: &WorkRecord,
                  plan_rev: &str,
                  digest: &str,
                  expected: Option<&str>,
                  findings: Vec<Value>| {
        let mut change = json!({"kind": "Assess", "planRevision": plan_rev, "planDigest": digest,
            "summary": "assessed", "findings": findings});
        if let Some(e) = expected {
            change["expectedAssessmentRevision"] = json!(e);
        }
        fx.command_at(
            &fx.agent(),
            "features.edit",
            json!({"workId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
                "changes": {"readiness": change}}),
            Some(2),
        )
    };
    // Readiness needs schema revision 2; an assessment of the pending draft is accepted.
    let digest = plan_digest(&pending.draft);
    let r1 = fx.command(
        &fx.agent(),
        "features.edit",
        json!({"workId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
            "changes": {"readiness": {"kind": "Assess", "planRevision": pending.revision.as_str(),
                "planDigest": digest, "summary": "", "findings": []}}}),
    );
    assert!(
        !matches!(r1, CommandReply::Recorded { .. })
            || matches!(recorded(r1), DomainDecision::Refused { .. })
    );
    let f = work_of(applied(assess(
        &f,
        pending.revision.as_str(),
        &digest,
        None,
        vec![finding("F1", "E2", vec![])],
    )));
    // The draft's assessment is its own generation: it gates nothing before adoption.
    assert!(f.readiness.is_none(), "no active plan, no gating readiness");
    let r = f
        .pending_readiness
        .clone()
        .expect("pending readiness at revision 2");
    assert!(!r.ready, "an unapproved plan is never ready");
    assert!(r.reviewed_pending);
    // Revision 1 readers do not see the member.
    assert!(
        fx.get("features.get", &f.id)
            .unwrap()
            .pending_readiness
            .is_none()
    );
    assert!(
        fx.get_at("features.get", &f.id, Some(2))
            .unwrap()
            .pending_readiness
            .is_some()
    );
    // Adoption promotes it: the evidence is kept (adopted content matches) and the finding stays
    // open.
    let f = fx.approve(&f);
    let f = fx.get_at("features.get", &f.id, Some(2)).unwrap();
    assert!(f.pending_readiness.is_none());
    let r = f.readiness.clone().unwrap();
    assert!(!r.reviewed_pending);
    assert!(matches!(
        r.freshness,
        devworks_core::protocol::records::ReadinessFreshness::Current
    ));
    assert!(!r.ready, "an open blocking finding remains after approval");
    // Run control: E2 is blocked by its finding; E1 and the feature-wide unit are current.
    let plan = {
        let (runs, fid) = (fx.runs.clone(), RootId(f.id.clone()));
        fx.read(move |s| {
            let w = s.sessions().work_root(&fid)?.unwrap();
            runs.scope().plan(s.runs(), &w)
        })
    };
    assert!(plan.unit(None).unwrap().readiness.is_current());
    assert!(plan.unit(Some("E1")).unwrap().readiness.is_current());
    assert!(matches!(
        plan.unit(Some("E2")).unwrap().readiness,
        devworks_core::services::runs::scope::Readiness::Blocked { .. }
    ));
    // A new blocker at the unchanged plan without new evidence is refused (no reopening).
    let active = f.approval.clone().unwrap();
    let fault = refused(assess(
        &f,
        active.plan_revision.as_str(),
        &String::from(active.plan_digest.clone()),
        Some(r.revision.as_str()),
        vec![finding("F1", "E2", vec![]), finding("F2", "E1", vec![])],
    ));
    assert_eq!(kind_of(&fault), "settled_without_new_evidence");
    // A re-assessment that leaves the open finding out keeps it (still open, still gating).
    let f = work_of(applied(assess(
        &f,
        active.plan_revision.as_str(),
        &String::from(active.plan_digest.clone()),
        Some(r.revision.as_str()),
        vec![],
    )));
    let r = fx
        .get_at("features.get", &f.id, Some(2))
        .unwrap()
        .readiness
        .unwrap();
    assert!(!r.ready);
    assert!(r.findings.0.iter().any(|x| x.id.as_str() == "F1"
        && matches!(
            x.state,
            devworks_core::protocol::records::FindingState::Open
        )));
    // It cannot be settled or weakened by restating it either.
    let mut settled = finding("F1", "E2", vec![]);
    settled["state"] = json!("Resolved");
    settled["closureNote"] = json!("done");
    let mut weaker = finding("F1", "E2", vec![]);
    weaker["blocking"] = json!(false);
    // Naming work narrows a scope-wide blocker to that work: the scope gate would lift.
    let mut narrowed = finding("F1", "E2", vec![]);
    narrowed["affectedWorkIds"] = json!([card.id.as_str()]);
    for restated in [settled, weaker, narrowed] {
        let fault = refused(assess(
            &f,
            active.plan_revision.as_str(),
            &String::from(active.plan_digest.clone()),
            Some(r.revision.as_str()),
            vec![restated],
        ));
        assert_eq!(kind_of(&fault), "open_finding_needs_closure");
    }
    let plan = {
        let (runs, fid) = (fx.runs.clone(), RootId(f.id.clone()));
        fx.read(move |s| {
            let w = s.sessions().work_root(&fid)?.unwrap();
            runs.scope().plan(s.runs(), &w)
        })
    };
    assert!(matches!(
        plan.unit(Some("E2")).unwrap().readiness,
        devworks_core::services::runs::scope::Readiness::Blocked { .. }
    ));
    // Closing the finding needs a note; an agent's closure of a blocker needs evidence.
    let close = |f: &WorkRecord, rev: &str, evidence: Vec<ArtifactRef>| {
        fx.command_at(
            &fx.agent(),
            "features.edit",
            json!({"workId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
                "changes": {"readiness": {"kind": "CloseFinding", "expectedAssessmentRevision": rev,
                    "findingId": "F1", "note": "user chose Stripe", "evidence": evidence}}}),
            Some(2),
        )
    };
    let fault = refused(close(&f, r.revision.as_str(), vec![]));
    assert_eq!(kind_of(&fault), "closure_evidence");
    let proof = fx.artifact(&p, 9);
    let f = work_of(applied(close(&f, r.revision.as_str(), vec![proof])));
    let f = fx.get_at("features.get", &f.id, Some(2)).unwrap();
    assert!(f.readiness.clone().unwrap().ready);
    // A replacement that rewords only E1 invalidates E1; E2 and the feature-wide unit stay.
    let f = fx.plan(&f, &doc, &[("E1", "pay with cards"), ("E2", "refund")]);
    let f = fx.approve(&f);
    let f = fx.get_at("features.get", &f.id, Some(2)).unwrap();
    let r = f.readiness.clone().unwrap();
    assert_eq!(r.stale_scope_keys.0, vec![id("E1")]);
    assert!(!r.ready);
    let plan = {
        let (runs, fid) = (fx.runs.clone(), RootId(f.id.clone()));
        fx.read(move |s| {
            let w = s.sessions().work_root(&fid)?.unwrap();
            runs.scope().plan(s.runs(), &w)
        })
    };
    assert!(matches!(
        plan.unit(Some("E1")).unwrap().readiness,
        devworks_core::services::runs::scope::Readiness::Stale { .. }
    ));
    assert!(plan.unit(Some("E2")).unwrap().readiness.is_current());
    assert!(plan.unit(None).unwrap().readiness.is_current());
    // Risk acceptance is a human, finding-specific decision that cannot waive a prerequisite.
    let active = f.approval.clone().unwrap();
    let mut pre = finding("F3", "E1", vec![fx.artifact(&p, 10)]);
    pre["prerequisite"] = json!(true);
    let f = work_of(applied(assess(
        &f,
        active.plan_revision.as_str(),
        &String::from(active.plan_digest.clone()),
        Some(r.revision.as_str()),
        vec![pre],
    )));
    let rr = fx
        .get_at("features.get", &f.id, Some(2))
        .unwrap()
        .readiness
        .unwrap();
    let (ctx, fid) = (fx.user(), f.id.clone());
    let rev = rr.revision.get();
    assert!(
        fx.write(move |tx| Ok(accept_finding_risk(tx, &ctx, &fid, &id("F3"), rev, "ok").is_err()))
    );
    // A plan the assessment did not review is refused.
    let fault = refused(assess(
        &f,
        "999",
        &String::from(active.plan_digest.clone()),
        Some(rr.revision.as_str()),
        vec![],
    ));
    assert_eq!(kind_of(&fault), "plan_changed");
}

// ---------------------------------------------------------------------------------------------
// Re-planning and atomic plans

#[test]
fn sync_plan_replans_by_reference_and_never_deletes_or_completes() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &["repo-1", "repo-2"]);
    let f = fx.feature(&p, "F", &[]);
    let doc = fx.artifact(&p, 1);
    let f = fx.plan(&f, &doc, &[("E1", "x")]);
    let f = fx.approve(&f);
    let slot = |f: &Id| {
        let f = f.clone();
        fx.read(move |s| s.work().feature_plan(&f))
            .unwrap()
            .revision
    };
    let sync = |tasks: Value, plan_rev: i64, set_rev: i64| {
        fx.command(
            &fx.agent(),
            "tasks.sync_plan",
            json!({"featureId": f.id.as_str(), "expectedPlanRevision": plan_rev.to_string(),
                "expectedTaskSetRevision": set_rev.to_string(), "tasks": tasks}),
        )
    };
    let entry = |key: &str, title: &str, deps: Vec<&str>, priority: Option<i32>| {
        let mut e = json!({"clientKey": key, "title": title, "description": "", "repositoryIds": [],
            "dependencyKeys": deps,
            "acceptance": [{"id": "C1", "text": "done", "evidenceIds": [], "state": "Unmet"}]});
        if let Some(p) = priority {
            e["priority"] = json!(p);
        }
        e
    };
    let q = feature_queue(&f.id);
    let cards = match applied(sync(
        json!([
            entry("a", "A", vec![], Some(3)),
            entry("b", "B", vec!["a"], None)
        ]),
        slot(&f.id),
        fx.queue(&p, &q),
    )) {
        OperationResult::TasksSyncPlan { value } => value.items.0,
        other => panic!("{other:?}"),
    };
    assert_eq!(ids(&cards), ["A", "B"]);
    let b = cards.iter().find(|c| c.title.0 == "B").unwrap().clone();
    assert_eq!(b.dependency_ids.0.len(), 1);
    let at = |e: Value, w: &WorkRecord| {
        let mut e = e;
        e["expectedRevision"] = json!(w.revision.as_str());
        e
    };
    // An existing entry must name the revision it was planned from.
    let fault = refused(sync(
        json!([entry("b", "B renamed", vec![], None)]),
        slot(&f.id),
        fx.queue(&p, &q),
    ));
    assert_eq!(
        (fault.code, kind_of(&fault)),
        (DomainErrorCode::InvalidArgument, "expected_revision")
    );
    // A newer edit of the card refuses a re-plan prepared before it (no silent overwrite).
    let edited = work_of(applied(fx.edit(
        &fx.user(),
        &b,
        json!({"title": "B by hand"}),
    )));
    let fault = refused(sync(
        json!([at(entry("b", "B renamed", vec![], None), &b)]),
        slot(&f.id),
        fx.queue(&p, &q),
    ));
    assert_eq!(fault.code, DomainErrorCode::RevisionConflict);
    assert_eq!(fx.fresh(&b).title.0, "B by hand");
    // A status move of a card advances the task set: a re-plan that saw the old set (and would
    // close the moved card) is refused.
    let set = fx.queue(&p, &q);
    let a_card = cards.iter().find(|c| c.title.0 == "A").unwrap().clone();
    let wf = fx.queue(&p, WORKFLOW_QUEUE).to_string();
    let a_card = work_of(applied(fx.command(
        &fx.user(),
        "tasks.move",
        json!({"taskId": a_card.id.as_str(), "expectedRevision": a_card.revision.as_str(),
            "statusId": Fx::status(&p, "in-progress"), "expectedWorkflowRevision": wf}),
    )));
    assert!(fx.queue(&p, &q) > set);
    let fault = refused(sync(
        json!([at(entry("b", "B renamed", vec![], None), &edited)]),
        slot(&f.id),
        set,
    ));
    assert_eq!(fault.code, DomainErrorCode::RevisionConflict);
    assert_eq!(fx.fresh(&a_card).status_id, a_card.status_id);
    let b = fx.fresh(&b);
    // A re-plan that omits A closes it (never deletes), keeps B's priority when omitted and
    // keeps a criterion's state while its ID and text are unchanged.
    let cards = match applied(sync(
        json!([
            at(entry("b", "B renamed", vec![], None), &b),
            entry("c", "C", vec!["b"], Some(1))
        ]),
        slot(&f.id),
        fx.queue(&p, &q),
    )) {
        OperationResult::TasksSyncPlan { value } => value.items.0,
        other => panic!("{other:?}"),
    };
    // The answer is the cards the plan names; the closed card is kept and read through lists.
    assert_eq!(ids(&cards), ["B renamed", "C"]);
    let a = fx.fresh(&a_card);
    assert_eq!(a.status_id.as_str(), Fx::status(&p, "closed"));
    let b2 = cards.iter().find(|c| c.title.0 == "B renamed").unwrap();
    assert_eq!(b2.id, b.id);
    assert_eq!(b2.priority, 0);
    // Stale revisions refuse; an out-of-scope repository refuses; a cycle refuses all of it.
    let fault = refused(sync(json!([]), slot(&f.id), 1));
    assert_eq!(fault.code, DomainErrorCode::RevisionConflict);
    let mut out_of_scope = entry("d", "D", vec![], None);
    out_of_scope["repositoryIds"] = json!(["repo-2"]);
    let fault = refused(sync(json!([out_of_scope]), slot(&f.id), fx.queue(&p, &q)));
    assert_eq!(kind_of(&fault), "plan_scope");
    // An existing card cannot be moved outside the scope by a re-plan either.
    let mut existing = at(entry("b", "B renamed", vec![], None), b2);
    existing["repositoryIds"] = json!(["repo-2"]);
    let fault = refused(sync(json!([existing]), slot(&f.id), fx.queue(&p, &q)));
    assert_eq!(kind_of(&fault), "plan_scope");
    let before = fx.list("tasks.list", &p, 50, None, None).unwrap().0.len();
    let c = cards.iter().find(|c| c.title.0 == "C").unwrap();
    let fault = refused(sync(
        json!([
            at(entry("b", "B renamed", vec!["c"], None), b2),
            at(entry("c", "C", vec!["b"], Some(1)), c),
            entry("e", "E new", vec![], None)
        ]),
        slot(&f.id),
        fx.queue(&p, &q),
    ));
    assert_eq!(kind_of(&fault), "dependency_cycle");
    assert_eq!(
        fx.list("tasks.list", &p, 50, None, None).unwrap().0.len(),
        before
    );
}

#[test]
fn atomic_and_initial_plans_are_all_or_nothing() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &["repo-1"]);
    let doc = fx.artifact(&p, 1);
    let plan = |second_dep: &str| {
        json!({"callerRef": "plan-1", "features": [{
            "callerRef": "feat-1", "title": "Feature", "description": "", "repositoryIds": ["repo-1"],
            "plan": draft(&doc, &[("E1", "x")], &["repo-1"]),
            "tasks": [
                {"clientKey": "t1", "title": "T1", "description": "", "repositoryIds": ["repo-1"],
                 "dependencyKeys": [], "acceptance": []},
                {"clientKey": "t2", "title": "T2", "description": "", "repositoryIds": [],
                 "dependencyKeys": [second_dep], "acceptance": []}]}],
            "dependencyEdges": []})
    };
    let created = match applied(fx.command(
        &fx.agent(),
        "features.create",
        json!({"projectId": p.0.as_str(), "creation": {"kind": "AtomicPlan", "plan": plan("t1")}}),
    )) {
        OperationResult::FeaturesCreate { value } => value.items.0,
        other => panic!("{other:?}"),
    };
    assert_eq!(ids(&created), ["Feature"]);
    assert!(created[0].pending_plan.is_some() && created[0].approval.is_none());
    let (cards, _) = fx.list("tasks.list", &p, 50, None, None).unwrap();
    assert_eq!(ids(&cards), ["T1", "T2"]);
    assert_eq!(cards[1].dependency_ids.0, vec![cards[0].id.clone()]);
    // An atomic plan's explicit edge cannot give completed work a new prerequisite: the plan's
    // links go through the same writer as tasks.dependency_links, and nothing is created.
    let done = fx.task(&fx.user(), &p, None, "Shipped", 0);
    let wf = fx.queue(&p, WORKFLOW_QUEUE).to_string();
    applied(fx.command(
        &fx.user(),
        "tasks.move",
        json!({"taskId": done.id.as_str(), "expectedRevision": done.revision.as_str(),
            "statusId": Fx::status(&p, "done"), "expectedWorkflowRevision": wf}),
    ));
    let before = fx.list("tasks.list", &p, 50, None, None).unwrap().0.len();
    let mut onto_done = plan("t1");
    onto_done["callerRef"] = json!("plan-9");
    onto_done["features"][0]["callerRef"] = json!("feat-9");
    onto_done["features"][0]["tasks"][0]["clientKey"] = json!("t91");
    onto_done["features"][0]["tasks"][1]["clientKey"] = json!("t92");
    onto_done["features"][0]["tasks"][1]["dependencyKeys"] = json!(["t91"]);
    onto_done["dependencyEdges"] = json!([{"predecessor": "t91", "successor": done.id.as_str()}]);
    let fault = refused(fx.command(
        &fx.agent(),
        "features.create",
        json!({"projectId": p.0.as_str(), "creation": {"kind": "AtomicPlan", "plan": onto_done}}),
    ));
    assert_eq!(kind_of(&fault), "work_completed");
    assert_eq!(
        fx.list("tasks.list", &p, 50, None, None).unwrap().0.len(),
        before
    );
    assert!(fx.fresh(&done).dependency_ids.0.is_empty());
    // An unknown reference creates nothing.
    let before = fx.list("tasks.list", &p, 50, None, None).unwrap().0.len();
    let mut bad = plan("nope");
    bad["callerRef"] = json!("plan-2");
    bad["features"][0]["callerRef"] = json!("feat-2");
    bad["features"][0]["tasks"][0]["clientKey"] = json!("t3");
    bad["features"][0]["tasks"][1]["clientKey"] = json!("t4");
    let fault = refused(fx.command(
        &fx.agent(),
        "features.create",
        json!({"projectId": p.0.as_str(), "creation": {"kind": "AtomicPlan", "plan": bad}}),
    ));
    assert_eq!(fault.code, DomainErrorCode::NotFound);
    assert_eq!(
        fx.list("tasks.list", &p, 50, None, None).unwrap().0.len(),
        before
    );
    // A staged plan reads the same fixed schema from an artifact.
    let mut staged = plan("t5");
    staged["callerRef"] = json!("plan-3");
    staged["features"][0]["callerRef"] = json!("feat-3");
    staged["features"][0]["tasks"][0]["clientKey"] = json!("t5");
    staged["features"][0]["tasks"][1]["clientKey"] = json!("t6");
    let bytes = serde_json::to_vec(&staged).unwrap();
    let digest = devworks_core::fingerprint::sha256_hex(&bytes);
    fx.staged.0.lock().unwrap().insert(digest, bytes.clone());
    let a = fx.artifact_bytes(&p, &bytes, 50);
    let made = match applied(fx.command(
        &fx.agent(),
        "features.create",
        json!({"projectId": p.0.as_str(), "creation": {"kind": "StagedPlan", "artifact": a}}),
    )) {
        OperationResult::FeaturesCreate { value } => value.items.0,
        other => panic!("{other:?}"),
    };
    assert_eq!(made.len(), 1);
    // A project's initial plan is created in the project's own unit of work.
    let mut initial = plan("t7");
    initial["callerRef"] = json!("plan-4");
    initial["features"][0]["callerRef"] = json!("feat-4");
    initial["features"][0]["tasks"][0]["clientKey"] = json!("t7");
    initial["features"][0]["tasks"][1]["clientKey"] = json!("t8");
    // Its plan content is a workspace-visible artifact (the project does not exist yet).
    initial["features"][0]["plan"]["repositoryIds"] = json!([]);
    initial["features"][0]["repositoryIds"] = json!([]);
    initial["features"][0]["tasks"][0]["repositoryIds"] = json!([]);
    let made: ProjectRecord = match applied(fx.command(
        &fx.user(),
        "projects.create",
        json!({"name": "Gamma", "description": "", "repositoryIds": [], "initialPlan": initial}),
    )) {
        OperationResult::ProjectsCreate { value } => value,
        other => panic!("{other:?}"),
    };
    let (features, _) = fx.list("features.list", &made.id, 50, None, None).unwrap();
    assert_eq!(ids(&features), ["Feature"]);
    // A failing initial plan creates no project.
    let mut broken = plan("missing");
    broken["callerRef"] = json!("plan-5");
    let before = fx.read(|s| {
        s.projects().projects(
            &ProjectFilter {
                only: None,
                lifecycles: vec!["Active".into()],
            },
            None,
            100,
        )
    });
    let fault = refused(fx.command(
        &fx.user(),
        "projects.create",
        json!({"name": "Delta", "description": "", "repositoryIds": [], "initialPlan": broken}),
    ));
    assert_eq!(fault.code, DomainErrorCode::NotFound);
    let after = fx.read(|s| {
        s.projects().projects(
            &ProjectFilter {
                only: None,
                lifecycles: vec!["Active".into()],
            },
            None,
            100,
        )
    });
    assert_eq!(before.len(), after.len());
}

// ---------------------------------------------------------------------------------------------
// Project integration

#[test]
fn work_names_block_repository_removal_and_archived_projects_close_to_work() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &["repo-1", "repo-2"]);
    let t = fx.task(&fx.user(), &p, None, "Card", 0);
    applied(fx.edit(&fx.user(), &t, json!({"repositoryIds": ["repo-2"]})));
    let project = match fx.query(
        &fx.user(),
        "projects.get",
        json!({"projectId": p.0.as_str()}),
    ) {
        DomainQueryReply::Result {
            result: OperationResult::ProjectsGet { value },
            ..
        } => value,
        other => panic!("{other:?}"),
    };
    let fault = refused(fx.command(
        &fx.user(),
        "projects.membership",
        json!({"projectId": p.0.as_str(), "expectedRevision": project.revision.as_str(),
            "repositoryIds": ["repo-1"]}),
    ));
    assert_eq!(kind_of(&fault), "repository_in_use");
    // Archive: writes are refused, reads still work.
    applied(fx.command(
        &fx.user(),
        "projects.archive",
        json!({"projectId": p.0.as_str(), "expectedRevision": project.revision.as_str()}),
    ));
    let t = fx.fresh(&t);
    let fault = refused(fx.edit(&fx.user(), &t, json!({"title": "late"})));
    assert_eq!(kind_of(&fault), "project_archived");
    let fault = refused(fx.command(
        &fx.agent(),
        "tasks.create",
        json!({"projectId": p.0.as_str(), "title": "late", "description": "",
            "repositoryIds": [], "statusId": Fx::status(&p, "backlog"), "priority": 0}),
    ));
    assert_eq!(kind_of(&fault), "project_archived");
    assert!(fx.get("tasks.get", &t.id).is_ok());
}

// ---------------------------------------------------------------------------------------------
// Migration 16

#[test]
fn migration_16_is_additive_and_closes_older_writers_out() {
    let m = MIGRATIONS
        .iter()
        .find(|m| m.version == 16)
        .expect("migration 16");
    assert_eq!(
        m.checksum(),
        MIGRATION_0016_SHA256,
        "an applied migration was edited"
    );
    assert_eq!((m.minimum_reader, m.minimum_writer), (8, 16));
    assert!(
        MIGRATIONS
            .iter()
            .filter(|e| e.version < 16)
            .all(|e| e.minimum_reader <= m.minimum_reader && e.minimum_writer <= m.minimum_writer)
    );
    assert!(!m.rebuilds_tables);
    let h = Harness::new();
    let old = SqliteStore::open_with(h.config(), &through(15)).unwrap();
    // A work row recorded before migration 16 (a positional migration 1 fixture row).
    old.with_writer_connection(|c| {
        super::super::conn::db(c.execute_batch(
            "INSERT INTO projects VALUES('project-old','Old','Active',1,1,5);
             INSERT INTO work_items VALUES('work-old','project-old','issue',NULL,NULL,NULL,'Legacy',2,0,3,1,7,'{}');",
        ))
    })
    .unwrap();
    drop(old);
    let s = SqliteStore::open_with(h.config(), &through(16)).unwrap();
    let report = s.compatibility().unwrap();
    assert_eq!(
        (
            report.store_schema_version,
            report.minimum_reader,
            report.minimum_writer
        ),
        (16, 8, 16)
    );
    let s: Arc<dyn ApplicationStore> = Arc::new(s);
    let w = read_snapshot(&*s, &ReadOptions::default(), |snap| {
        snap.work().work(&id("work-old"))
    })
    .unwrap()
    .unwrap();
    // Without its detail row it reads with the documented defaults; `issue` reads as a card.
    assert_eq!((w.kind.as_str(), w.revision, w.updated_at_ms), (TASK, 3, 7));
    assert!(w.search_key.is_empty() && w.caller_ref.is_none() && w.completion.is_none());
    // The store refuses records that break its invariants.
    let bad = StoredWork {
        kind: "epic".into(),
        revision: 4,
        ..w.clone()
    };
    assert!(
        run_write(&*s, WriteOptions::default(), move |tx| tx
            .work()
            .update_work(&bad, 3))
        .is_err()
    );
    let stale = StoredWork {
        revision: 3,
        ..w.clone()
    };
    assert!(matches!(
        run_write(&*s, WriteOptions::default(), move |tx| tx
            .work()
            .update_work(&stale, 2)),
        Err(StorageError::RevisionConflict(_))
    ));
}

// ---------------------------------------------------------------------------------------------
// Run-control fences of feature work

#[test]
fn a_cards_assignment_is_fenced_by_its_holds_membership_findings_and_prerequisites() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    let f = fx.feature(&p, "Checkout", &[]);
    let doc = fx.artifact(&p, 1);
    let f = fx.plan(&f, &doc, &[("E1", "pay")]);
    let card = fx.task(&fx.user(), &p, Some(&f), "Card", 0);
    let f = fx.approve(&f);
    // A worker assignment of the card under the feature (as delegation records it).
    let (fid, cid) = (f.id.clone(), card.id.clone());
    let assignment = fx.write(move |tx| {
        tx.sessions().insert_session(&StoredSession {
            id: id("session-f"),
            conversation_id: id("conversation-f"),
            agent_id: actor("agent-w"),
            project_id: None,
            root_id: Some(RootId(fid.clone())),
            title: "worker".into(),
            context_epoch: 0,
            revision: 1,
            created_by: actor("agent-w"),
            created_at_ms: super::T0,
            closed_at_ms: None,
            closed_by: None,
            next_sequence: 1,
            checkpoint_sequence: 0,
        })?;
        let a = StoredAssignment {
            id: devworks_core::protocol::AssignmentId(id("assignment-c")),
            root_id: RootId(fid),
            work_id: RootId(cid),
            parent_id: None,
            session_id: id("session-f"),
            execution_id: None,
            state: AssignmentState::Ready,
            generation: 0,
            slot_held: false,
            revision: 1,
            checkpoint: None,
            scope_key: Some("E1".into()),
            scope_digest: None,
            plan_revision: None,
            instruction_revision: 0,
            created_at_ms: super::T0,
        };
        tx.runs().insert_assignment(&a)?;
        Ok(a.id)
    });
    let reasons = |fx: &Fx| fx.eligible(&f.id, Target::Assignment(assignment.clone()));
    let before = reasons(&fx);
    assert!(
        !before
            .iter()
            .any(|r| r.contains("gates this work") || r.contains("no longer part")),
        "{before:?}"
    );
    // An open bounded investigation gating the card holds only its implementation.
    let f2 = fx.get_at("features.get", &f.id, Some(2)).unwrap();
    let active = f2.approval.clone().unwrap();
    let investigation = json!({"id": "I1", "disposition": "BoundedInvestigation", "blocking": true,
        "prerequisite": false, "state": "Open", "requirementIds": ["E1"], "scopeKey": "E1",
        "affectedWorkIds": [card.id.as_str()], "summary": "which API version",
        "missing": "the supported API version", "consequence": "the client may break",
        "attempts": ["read the changelog"], "insufficiency": "needs a probe",
        "closureCondition": "a probe against the sandbox passes", "evidence": [],
        "objective": "probe the sandbox", "limits": "one hour, read-only", "closureEvidence": []});
    applied(fx.command_at(
        &fx.agent(),
        "features.edit",
        json!({"workId": f2.id.as_str(), "expectedRevision": f2.revision.as_str(),
            "changes": {"readiness": {"kind": "Assess", "planRevision": active.plan_revision.as_str(),
                "planDigest": String::from(active.plan_digest.clone()), "summary": "probe first",
                "findings": [investigation]}}}),
        Some(2),
    ));
    assert!(reasons(&fx).iter().any(|r| r.contains("gates this work")));
    // An unsatisfied prerequisite of the card holds it too.
    let pre = fx.task(&fx.user(), &p, None, "Prerequisite", 0);
    applied(fx.links(&fx.fresh(&card), &[&pre]));
    assert!(reasons(&fx).iter().any(|r| r.contains("prerequisite")));
    // Removing the card from the feature fences the old assignment and pauses the card.
    let card = fx.fresh(&card);
    applied(fx.command(
        &fx.agent(),
        "tasks.set_feature",
        json!({"taskId": card.id.as_str(), "expectedRevision": card.revision.as_str(),
            "clearFeature": true}),
    ));
    let after = reasons(&fx);
    assert!(
        after
            .iter()
            .any(|r| r.contains("no longer part of this root")),
        "{after:?}"
    );
    assert!(
        after.iter().any(|r| r.contains("removed from its feature")),
        "{after:?}"
    );
    // The project's archival stops a card that keeps controls of its own (it is a root).
    let roots = {
        let p = p.clone();
        fx.read(move |s| s.runs().project_roots(&p, None, 100))
    };
    assert!(roots.contains(&RootId(card.id.clone())));
}

// ---------------------------------------------------------------------------------------------
// State a replacement does not restate is kept

#[test]
fn plan_revisions_keep_criterion_state_requirements_and_acceptance_tests() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    let f = fx.feature(&p, "Checkout", &[]);
    let doc = fx.artifact(&p, 1);
    let decision = fx.artifact(&p, 2);
    let ev = fx.evidence(&f.id);
    // A plan with a met, evidenced criterion, an acceptance test and a dropped requirement.
    let mut plan = draft(&doc, &[("E1", "pay")], &[]);
    plan["acceptance"] = json!([{"id": "A1", "text": "works", "evidenceIds": [ev.as_str()],
        "state": "Met"}]);
    plan["acceptanceTests"] =
        json!([{"id": "T1", "text": "checkout e2e", "evidenceIds": [], "state": "Unmet"}]);
    plan["requirements"].as_array_mut().unwrap().push(
        json!({"id": "E2", "statement": "refunds", "status": "Dropped",
            "dropDecision": decision, "coveredByTaskIds": []}),
    );
    let f = work_of(applied(fx.command(
        &fx.user(),
        "features.edit_plan",
        json!({"featureId": f.id.as_str(), "expectedRevision": f.revision.as_str(), "plan": plan}),
    )));
    let f = fx.approve(&f);
    // Revision 2 records carry the active plan's acceptance tests.
    let rec = fx.get_at("features.get", &f.id, Some(2)).unwrap();
    assert_eq!(rec.acceptance_tests.unwrap().0[0].id, id("T1"));
    // A replacement restating A1 with the same text keeps its state and evidence.
    let mut again = draft(&doc, &[("E1", "pay with cards")], &[]);
    again["requirements"].as_array_mut().unwrap().push(
        json!({"id": "E2", "statement": "refunds", "status": "Dropped",
            "dropDecision": decision, "coveredByTaskIds": []}),
    );
    let f = work_of(applied(fx.command(
        &fx.user(),
        "features.edit_plan",
        json!({"featureId": f.id.as_str(), "expectedRevision": f.revision.as_str(), "plan": again}),
    )));
    let kept = &f.pending_plan.clone().unwrap().draft.acceptance.0[0];
    assert!(matches!(
        kept.state,
        devworks_core::protocol::records::CriterionState::Met
    ));
    assert_eq!(kept.evidence_ids.0, vec![ev.clone()]);
    // A replacement that leaves out the dropped requirement (its recorded decision) is refused.
    let fault = refused(fx.command(
        &fx.user(),
        "features.edit_plan",
        json!({"featureId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
            "plan": draft(&doc, &[("E1", "pay")], &[])}),
    ));
    assert_eq!(kind_of(&fault), "requirement_missing");
    // A card's patch may not change its status once the workflow changed: that is tasks.move,
    // which checks the workflow revision.
    let t = fx.task(&fx.user(), &p, None, "Card", 0);
    let p2 = p.clone();
    fx.write(move |tx| tx.work().bump_queue(&p2, WORKFLOW_QUEUE));
    let fault = refused(fx.edit(&fx.user(), &t, json!({"statusId": Fx::status(&p, "ready")})));
    assert_eq!(kind_of(&fault), "use_tasks_move");
}

// ---------------------------------------------------------------------------------------------
// Round 2: hierarchy gates, fresh results, complete pages

/// A worker assignment of `work` under the root `root` (as delegation records it).
fn assign(fx: &Fx, root: &Id, work: &Id, n: u32) -> devworks_core::protocol::AssignmentId {
    assign_under(fx, root, work, n, None)
}
fn assign_under(
    fx: &Fx,
    root: &Id,
    work: &Id,
    n: u32,
    parent: Option<devworks_core::protocol::AssignmentId>,
) -> devworks_core::protocol::AssignmentId {
    let (root, work) = (root.clone(), work.clone());
    fx.write(move |tx| {
        let session = id(&format!("session-{n}"));
        tx.sessions().insert_session(&StoredSession {
            id: session.clone(),
            conversation_id: id(&format!("conversation-{n}")),
            agent_id: actor("agent-w"),
            project_id: None,
            root_id: Some(RootId(root.clone())),
            title: "worker".into(),
            context_epoch: 0,
            revision: 1,
            created_by: actor("agent-w"),
            created_at_ms: super::T0,
            closed_at_ms: None,
            closed_by: None,
            next_sequence: 1,
            checkpoint_sequence: 0,
        })?;
        let a = StoredAssignment {
            id: devworks_core::protocol::AssignmentId(id(&format!("assignment-{n}"))),
            root_id: RootId(root),
            work_id: RootId(work),
            parent_id: parent,
            session_id: session,
            execution_id: None,
            state: AssignmentState::Ready,
            generation: 0,
            slot_held: false,
            revision: 1,
            checkpoint: None,
            scope_key: None,
            scope_digest: None,
            plan_revision: None,
            instruction_revision: 0,
            created_at_ms: super::T0,
        };
        tx.runs().insert_assignment(&a)?;
        Ok(a.id)
    })
}

#[test]
fn a_features_prerequisites_hold_the_feature_its_cards_and_its_linked_cards() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    let first = fx.feature(&p, "Payments", &[]);
    let second = fx.feature(&p, "Refunds", &[]);
    // Payments blocks Refunds (features.dependencies).
    applied(fx.command(
        &fx.agent(),
        "features.dependencies",
        json!({"featureId": second.id.as_str(), "expectedRevision": second.revision.as_str(),
            "replacement": {"edges": [{"predecessor": first.id.as_str(),
                "successor": second.id.as_str()}]}}),
    ));
    let doc = fx.artifact(&p, 1);
    let second = fx.plan(&fx.fresh(&second), &doc, &[("E1", "refund")]);
    let card = fx.task(&fx.user(), &p, Some(&second), "Refund API", 0);
    let second = fx.approve(&second);
    let blocked_by_first =
        |reasons: &[String]| reasons.iter().any(|r| r.contains(first.id.as_str()));
    // The feature itself, and every assignment of its work, through the root.
    assert!(blocked_by_first(&fx.eligible(&second.id, Target::Root)));
    let a = assign(&fx, &second.id, &card.id, 1);
    assert!(blocked_by_first(
        &fx.eligible(&second.id, Target::Assignment(a))
    ));
    // A card linked to the feature that keeps controls of its own (it was unlinked, then
    // linked again) is held by its feature's prerequisites and plan gate as well.
    let other = fx.task(&fx.user(), &p, None, "Standalone", 0);
    let unlink = |w: &WorkRecord, f: Option<&WorkRecord>| {
        let mut body = json!({"taskId": w.id.as_str(), "expectedRevision": w.revision.as_str(),
            "clearFeature": f.is_none()});
        if let Some(f) = f {
            body["featureId"] = json!(f.id.as_str());
        }
        work_of(applied(fx.command(&fx.user(), "tasks.set_feature", body)))
    };
    let other = unlink(&other, Some(&second));
    let other = unlink(&other, None);
    unlink(&other, Some(&second));
    let reasons = fx.eligible(&other.id, Target::Root);
    assert!(blocked_by_first(&reasons), "{reasons:?}");
    assert!(
        reasons.iter().any(|r| r.contains("readiness")),
        "the feature's readiness gate applies: {reasons:?}"
    );
}

#[test]
fn atomic_results_are_read_after_their_links_and_replans_answer_one_complete_page() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    let doc = fx.artifact(&p, 1);
    let feature = |r: &str, t: &str| {
        json!({"callerRef": r, "title": t, "description": "", "repositoryIds": [],
            "plan": draft(&doc, &[("E1", "x")], &[]), "tasks": []})
    };
    // Feature A blocks feature B: writing that link advances B's revision.
    let plan = json!({"callerRef": "plan-1",
        "features": [feature("feat-a", "A"), feature("feat-b", "B")],
        "dependencyEdges": [{"predecessor": "feat-a", "successor": "feat-b"}]});
    let created = match applied(fx.command(
        &fx.agent(),
        "features.create",
        json!({"projectId": p.0.as_str(), "creation": {"kind": "AtomicPlan", "plan": plan}}),
    )) {
        OperationResult::FeaturesCreate { value } => value,
        other => panic!("{other:?}"),
    };
    assert!(!created.has_more && created.next_cursor.is_none());
    let b = created.items.0.iter().find(|f| f.title.0 == "B").unwrap();
    assert_eq!(b.revision, fx.fresh(b).revision);
    assert_eq!(b.dependency_ids.0.len(), 1);
    // The returned revision is the one the next edit names.
    applied(fx.command(
        &fx.user(),
        "features.edit",
        json!({"workId": b.id.as_str(), "expectedRevision": b.revision.as_str(),
            "changes": {"title": "B renamed"}}),
    ));
    // A feature with more than 500 (closed, historical) cards: its re-plan answers the cards
    // the plan names, one complete page.
    let f = fx.feature(&p, "Busy", &[]);
    let (pid, fid) = (p.clone(), f.id.clone());
    let closed = Fx::status(&p, "closed");
    fx.write(move |tx| {
        for n in 0..501 {
            tx.work().insert_work(&StoredWork {
                id: id(&format!("old-{n}")),
                project_id: pid.clone(),
                kind: TASK.into(),
                parent_feature_id: Some(fid.clone()),
                status_id: Some(id(&closed)),
                title: format!("Old {n}"),
                priority: 0,
                manual_order: n,
                revision: 1,
                scope_revision: 1,
                created_at_ms: super::T0,
                updated_at_ms: super::T0,
                created_by: None,
                updated_by: None,
                search_key: String::new(),
                caller_ref: None,
                completion: None,
                brief: json!({}),
            })?;
        }
        Ok(())
    });
    let slot = {
        let fid = f.id.clone();
        fx.read(move |s| s.work().feature_plan(&fid))
            .unwrap()
            .revision
    };
    let page = match applied(fx.command(
        &fx.agent(),
        "tasks.sync_plan",
        json!({"featureId": f.id.as_str(), "expectedPlanRevision": slot.to_string(),
            "expectedTaskSetRevision": fx.queue(&p, &feature_queue(&f.id)).to_string(),
            "tasks": [{"clientKey": "new", "title": "New card", "description": "",
                "repositoryIds": [], "dependencyKeys": [], "acceptance": []}]}),
    )) {
        OperationResult::TasksSyncPlan { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(ids(&page.items.0), ["New card"]);
    assert!(!page.has_more && page.next_cursor.is_none());
}

#[test]
fn full_detail_in_a_list_never_claims_more_without_a_cursor() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    let t = fx.task(&fx.user(), &p, None, "Chatty", 0);
    // 501 comments recorded on the card.
    let tid = t.id.clone();
    fx.write(move |tx| {
        let mut w = tx.work().work(&tid)?.unwrap();
        let comments: Vec<Value> = (0..501)
            .map(|n| {
                json!({"item": {"id": format!("c{n}"), "content": "note", "evidence": []},
                "actor": "user-a", "atMs": 1})
            })
            .collect();
        w.brief["comments"] = json!(comments);
        let before = w.revision;
        w.revision = before + 1;
        tx.work().update_work(&w, before)
    });
    let listed = match fx.query(
        &fx.user(),
        "tasks.list",
        json!({"projectId": p.0.as_str(), "page": {"limit": 10}, "statusIds": [],
            "repositoryIds": [], "detailLevel": "Full"}),
    ) {
        DomainQueryReply::Result {
            result: OperationResult::TasksList { value },
            ..
        } => value.items.0.into_iter().next().unwrap(),
        other => panic!("{other:?}"),
    };
    assert_eq!(listed.comments.0.len(), 500);
    let cursor = listed
        .detail_cursor
        .clone()
        .expect("a continuation for the rest");
    // The cursor continues through tasks.get.
    let rest = match fx.query(
        &fx.user(),
        "tasks.get",
        json!({"workId": t.id.as_str(), "detailLevel": "Full",
            "detailPage": {"limit": 500, "cursor": cursor.0}}),
    ) {
        DomainQueryReply::Result {
            result: OperationResult::TasksGet { value },
            ..
        } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(rest.comments.0.len(), 1);
    assert!(rest.detail_cursor.is_none());
}

// ---------------------------------------------------------------------------------------------
// Round 3: feature completion, work-specific gates, linked running agents

#[test]
fn a_feature_completes_verified_only_with_evidence_for_its_active_acceptance() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    let pre = fx.feature(&p, "Payments", &[]);
    let dep = fx.feature(&p, "Refunds", &[]);
    applied(fx.command(
        &fx.agent(),
        "features.dependencies",
        json!({"featureId": dep.id.as_str(), "expectedRevision": dep.revision.as_str(),
            "replacement": {"edges": [{"predecessor": pre.id.as_str(),
                "successor": dep.id.as_str()}]}}),
    ));
    let held = |fx: &Fx| {
        fx.eligible(&dep.id, Target::Root)
            .iter()
            .any(|r| r.contains(pre.id.as_str()))
    };
    // Recording results needs an active plan.
    let early = fx.command_at(
        &fx.user(),
        "features.edit",
        json!({"workId": pre.id.as_str(), "expectedRevision": pre.revision.as_str(),
            "changes": {"acceptanceEvidence": [{"id": "A1", "text": "works", "evidenceIds": [],
                "state": "Met"}]}}),
        Some(2),
    );
    assert_eq!(kind_of(&refused(early)), "no_active_plan");
    let doc = fx.artifact(&p, 1);
    let mut plan = draft(&doc, &[("E1", "pay")], &[]);
    plan["acceptanceTests"] =
        json!([{"id": "T1", "text": "checkout e2e", "evidenceIds": [], "state": "Unmet"}]);
    let pre = work_of(applied(fx.command(
        &fx.user(),
        "features.edit_plan",
        json!({"featureId": pre.id.as_str(), "expectedRevision": pre.revision.as_str(),
            "plan": plan}),
    )));
    let pre = fx.approve(&pre);
    let completion = |fx: &Fx| {
        let i = pre.id.clone();
        fx.read(move |s| s.work().work(&i)).unwrap().completion
    };
    let edit = |fx: &Fx, w: &WorkRecord, changes: Value| {
        fx.command_at(
            &fx.user(),
            "features.edit",
            json!({"workId": w.id.as_str(), "expectedRevision": w.revision.as_str(),
                "changes": changes}),
            Some(2),
        )
    };
    // A Done label alone is a manual completion: the dependent stays held.
    let pre = work_of(applied(edit(
        &fx,
        &pre,
        json!({"statusId": Fx::status(&p, "done")}),
    )));
    assert_eq!(completion(&fx).as_deref(), Some(MANUAL));
    assert!(held(&fx));
    // Results bind the approved criterion text and recorded evidence.
    let fault = refused(edit(
        &fx,
        &pre,
        json!({"acceptanceEvidence": [{"id": "A1", "text": "something else", "evidenceIds": [],
            "state": "Met"}]}),
    ));
    assert_eq!(kind_of(&fault), "criterion_text");
    let fault = refused(edit(
        &fx,
        &pre,
        json!({"acceptanceEvidence": [{"id": "A9", "text": "works", "evidenceIds": [],
            "state": "Met"}]}),
    ));
    assert_eq!(fault.code, DomainErrorCode::NotFound);
    // Revision 1 does not carry the member.
    let r1 = fx.command(
        &fx.user(),
        "features.edit",
        json!({"workId": pre.id.as_str(), "expectedRevision": pre.revision.as_str(),
            "changes": {"acceptanceEvidence": [{"id": "A1", "text": "works", "evidenceIds": [],
                "state": "Met"}]}}),
    );
    assert!(!matches!(r1, CommandReply::Recorded { .. }));
    // The criterion with evidence, the test met without evidence: still manual.
    let ev = fx.evidence(&pre.id);
    let pre = work_of(applied(edit(
        &fx,
        &pre,
        json!({"acceptanceEvidence": [
            {"id": "A1", "text": "works", "evidenceIds": [ev.as_str()], "state": "Met"},
            {"id": "T1", "text": "checkout e2e", "evidenceIds": [], "state": "Met"}]}),
    )));
    assert_eq!(completion(&fx).as_deref(), Some(MANUAL));
    assert!(held(&fx));
    assert!(matches!(
        pre.acceptance.0[0].state,
        devworks_core::protocol::records::CriterionState::Met
    ));
    // Every active criterion and test met with evidence: verified, and the dependent is free.
    work_of(applied(edit(
        &fx,
        &pre,
        json!({"acceptanceEvidence": [
            {"id": "T1", "text": "checkout e2e", "evidenceIds": [ev.as_str()], "state": "Met"}]}),
    )));
    assert_eq!(completion(&fx).as_deref(), Some(VERIFIED));
    assert!(!held(&fx));
}

#[test]
fn a_finding_that_names_its_work_gates_only_that_work() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    let f = fx.feature(&p, "Checkout", &[]);
    let doc = fx.artifact(&p, 1);
    let f = fx.plan(&f, &doc, &[("E1", "pay")]);
    let a = fx.task(&fx.user(), &p, Some(&f), "Card A", 0);
    let b = fx.task(&fx.user(), &p, Some(&f), "Card B", 0);
    let f = fx.approve(&f);
    let f = fx.get_at("features.get", &f.id, Some(2)).unwrap();
    let active = f.approval.clone().unwrap();
    // A blocking investigation of requirement E1 that names only card A, and one with no
    // scope key that names card A as well.
    let investigation = |id: &str, scope: Option<&str>| {
        let mut x = json!({"id": id, "disposition": "BoundedInvestigation", "blocking": true,
            "prerequisite": false, "state": "Open", "requirementIds": ["E1"],
            "affectedWorkIds": [a.id.as_str()], "summary": "which API version",
            "missing": "the supported API version", "consequence": "the client may break",
            "attempts": ["read the changelog"], "insufficiency": "needs a probe",
            "closureCondition": "a probe passes", "evidence": [],
            "objective": "probe the sandbox", "limits": "one hour, read-only",
            "closureEvidence": []});
        if let Some(k) = scope {
            x["scopeKey"] = json!(k);
        }
        x
    };
    applied(fx.command_at(
        &fx.agent(),
        "features.edit",
        json!({"workId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
            "changes": {"readiness": {"kind": "Assess",
                "planRevision": active.plan_revision.as_str(),
                "planDigest": String::from(active.plan_digest.clone()), "summary": "probe first",
                "findings": [investigation("I1", Some("E1")), investigation("I2", None)]}}}),
        Some(2),
    ));
    // No scope unit is held by work-specific findings.
    let plan = {
        let (runs, fid) = (fx.runs.clone(), RootId(f.id.clone()));
        fx.read(move |s| {
            let w = s.sessions().work_root(&fid)?.unwrap();
            runs.scope().plan(s.runs(), &w)
        })
    };
    assert!(plan.unit(Some("E1")).unwrap().readiness.is_current());
    assert!(plan.unit(None).unwrap().readiness.is_current());
    // Card A's assignment is gated; card B's is not.
    let ga = assign(&fx, &f.id, &a.id, 931);
    let gb = assign_under(&fx, &f.id, &b.id, 932, Some(ga.clone()));
    let gated = |x| {
        fx.eligible(&f.id, Target::Assignment(x))
            .iter()
            .any(|r| r.contains("gates this work"))
    };
    assert!(gated(ga));
    assert!(!gated(gb.clone()));
    // Widening a work-specific finding (more work named) is accepted, but on an unchanged plan
    // only with newly discovered evidence (AGENT-20).
    let f = fx.fresh(&f);
    let r = fx
        .get_at("features.get", &f.id, Some(2))
        .unwrap()
        .readiness
        .unwrap();
    let widen = |wider: Value| {
        fx.command_at(
            &fx.agent(),
            "features.edit",
            json!({"workId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
                "changes": {"readiness": {"kind": "Assess",
                    "planRevision": active.plan_revision.as_str(),
                    "planDigest": String::from(active.plan_digest.clone()), "summary": "wider",
                    "expectedAssessmentRevision": r.revision.as_str(),
                    "findings": [wider]}}}),
            Some(2),
        )
    };
    let mut wider = investigation("I1", Some("E1"));
    wider["affectedWorkIds"] = json!([a.id.as_str(), b.id.as_str()]);
    let fault = refused(widen(wider.clone()));
    assert_eq!(kind_of(&fault), "settled_without_new_evidence");
    wider["evidence"] = json!([fx.artifact(&p, 2)]);
    applied(widen(wider));
    assert!(gated(gb));
}

#[test]
fn an_issue_linked_while_running_keeps_its_activation_to_finish_and_archival_stops_it() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    let f = fx.feature(&p, "Checkout", &[]);
    let issue = fx.task(&fx.user(), &p, None, "Hotfix", 0);
    // The issue runs as a root of its own.
    let control = {
        let r = RootId(issue.id.clone());
        fx.read(move |s| devworks_core::services::runs::control_of(s.runs(), &r))
    };
    let frame = json!({"protocol": "1.0-draft.1", "requestKey": "start-hotfix", "command": {
        "kind": "runs.start", "rootId": issue.id.as_str(), "expectedWorkRevision": "1",
        "expectedControlRevision": control.revision.to_string()}});
    let authority = fx.authority();
    let gateway = Gateway {
        authority: &authority,
        store: &*fx.store,
        ids: &*fx.h.ids,
    };
    let started = fx
        .cases
        .command(
            &gateway,
            &fx.user(),
            "runs.start",
            frame.to_string().as_bytes(),
        )
        .unwrap();
    assert!(matches!(recorded(started), DomainDecision::Applied { .. }));
    let runs = fx.runs.clone();
    let (execution, epoch) = fx.write(move |tx| {
        let a = runs
            .claim_activation(tx, &id("supervisor-1"))?
            .expect("claimable");
        let _ = runs.start_activation(tx, &a.id, a.owner_epoch)?;
        Ok((a.id, a.owner_epoch))
    });
    // Linked to a feature while its agent runs (WORK-11: it continues).
    applied(fx.command(
        &fx.user(),
        "tasks.set_feature",
        json!({"taskId": issue.id.as_str(), "expectedRevision": issue.revision.as_str(),
            "featureId": f.id.as_str(), "clearFeature": false}),
    ));
    let (runs, e) = (fx.runs.clone(), execution.clone());
    let renewed = fx.write(move |tx| Ok(runs.renew_activation(tx, &e, epoch)?.is_ok()));
    assert!(renewed, "the running agent keeps its lease");
    let (runs, e, root) = (fx.runs.clone(), execution.clone(), RootId(issue.id.clone()));
    let dispatch = fx.write(move |tx| {
        Ok(
            match runs.begin_dispatch(
                tx,
                &devworks_core::services::runs::substrate::DispatchRequest {
                    root,
                    execution_id: e,
                    owner_epoch: epoch,
                    target: Target::Root,
                    purpose: ActivationPurpose::Implementation,
                },
            )? {
                Ok(_) => "admitted".to_owned(),
                Err(devworks_core::services::runs::substrate::Fenced::Blocked(b)) => format!(
                    "held: {}",
                    b.conditions
                        .iter()
                        .map(|c| c.reason.clone())
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
                Err(other) => format!("{other:?}"),
            },
        )
    });
    // Its dispatch is decided by the feature's gates (no approved plan yet), not refused as a
    // missing root.
    assert!(
        dispatch.starts_with("held: ") && dispatch.contains("feature"),
        "{dispatch}"
    );
    let (runs, e) = (fx.runs.clone(), execution.clone());
    let finished = fx.write(move |tx| {
        Ok(runs
            .finish_activation(tx, &e, epoch, ActivationOutcome::Completed, None)?
            .is_ok())
    });
    assert!(finished, "the running agent finishes normally");
    // The project's archival stops a linked card that keeps controls of its own.
    let project = match fx.query(
        &fx.user(),
        "projects.get",
        json!({"projectId": p.0.as_str()}),
    ) {
        DomainQueryReply::Result {
            result: OperationResult::ProjectsGet { value },
            ..
        } => value,
        other => panic!("{other:?}"),
    };
    applied(fx.command(
        &fx.user(),
        "projects.archive",
        json!({"projectId": p.0.as_str(), "expectedRevision": project.revision.as_str()}),
    ));
    let control = {
        let r = RootId(issue.id.clone());
        fx.read(move |s| devworks_core::services::runs::control_of(s.runs(), &r))
    };
    assert!(
        control.stop_operation_id.is_some(),
        "the archival stopped it"
    );
}

// ---------------------------------------------------------------------------------------------
// Round 4: one criterion namespace, bounded prerequisites, project-visible plan artifacts

#[test]
fn a_plan_draft_refuses_an_id_shared_by_a_criterion_and_a_test() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    let f = fx.feature(&p, "Checkout", &[]);
    let doc = fx.artifact(&p, 1);
    let mut plan = draft(&doc, &[("E1", "pay")], &[]);
    plan["acceptance"] =
        json!([{"id": "A1", "text": "works", "evidenceIds": [], "state": "Unmet"}]);
    plan["acceptanceTests"] =
        json!([{"id": "A1", "text": "checkout e2e", "evidenceIds": [], "state": "Unmet"}]);
    let fault = refused(fx.command(
        &fx.user(),
        "features.edit_plan",
        json!({"featureId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
            "plan": plan.clone()}),
    ));
    assert_eq!(kind_of(&fault), "acceptance_tests");
    plan["acceptanceTests"][0]["id"] = json!("T1");
    applied(fx.command(
        &fx.user(),
        "features.edit_plan",
        json!({"featureId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
            "plan": plan}),
    ));
}

#[test]
fn an_item_has_at_most_the_prerequisites_its_record_shows() {
    use devworks_core::services::work::deps::MAX_PREREQUISITES;
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    let target = fx.task(&fx.user(), &p, None, "Target", 0);
    let blockers: Vec<WorkRecord> = (0..=MAX_PREREQUISITES)
        .map(|n| fx.task(&fx.user(), &p, None, &format!("Blocker {n}"), 0))
        .collect();
    let edges = |n: usize| {
        blockers[..n]
            .iter()
            .map(|b| json!({"predecessor": b.id.as_str(), "successor": target.id.as_str()}))
            .collect::<Vec<_>>()
    };
    // One more than the bound is refused (the record could not show it all): through an atomic
    // plan, whose edges go through the same writer.
    let doc = fx.artifact(&p, 1);
    let plan = json!({"callerRef": "plan-1", "features": [{
        "callerRef": "feat-1", "title": "Feature", "description": "", "repositoryIds": [],
        "plan": draft(&doc, &[("E1", "x")], &[]),
        "tasks": [{"clientKey": "t1", "title": "T1", "description": "", "repositoryIds": [],
            "dependencyKeys": [], "acceptance": []}]}],
        "dependencyEdges": edges(MAX_PREREQUISITES + 1)});
    let fault = refused(fx.command(
        &fx.agent(),
        "features.create",
        json!({"projectId": p.0.as_str(), "creation": {"kind": "AtomicPlan", "plan": plan}}),
    ));
    assert_eq!(kind_of(&fault), "too_many_prerequisites");
    assert!(fx.fresh(&target).dependency_ids.0.is_empty());
    // At the bound, the record shows every prerequisite.
    let w = work_of(applied(fx.command(
        &fx.user(),
        "tasks.dependency_links",
        json!({"taskId": target.id.as_str(), "expectedRevision": target.revision.as_str(),
            "replacement": {"edges": edges(MAX_PREREQUISITES)}}),
    )));
    assert_eq!(w.dependency_ids.0.len(), MAX_PREREQUISITES);
}

#[test]
fn a_plan_names_only_artifacts_its_project_can_read() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    let f = fx.feature(&p, "Checkout", &[]);
    // A workspace artifact of the user: they see it, the project's agents do not.
    let doc = fx.artifact_in(
        ArtifactScope::Workspace,
        Some(actor("user-a")),
        b"plan text 1",
        1,
    );
    let edit = |f: &WorkRecord| {
        fx.command(
            &fx.user(),
            "features.edit_plan",
            json!({"featureId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
                "plan": draft(&doc, &[("E1", "pay")], &[])}),
        )
    };
    let fault = refused(edit(&f));
    assert_eq!(fault.code, DomainErrorCode::NotFound);
    // Shared with the project, it is readable there and accepted.
    let (aid, pid) = (doc.artifact_id.clone(), p.clone());
    fx.write(move |tx| tx.artifacts().add_share(&aid, &pid).map(|_| ()));
    applied(edit(&f));
}

#[test]
fn a_plan_key_that_is_another_items_id_is_refused_as_ambiguous() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    let existing = fx.task(&fx.user(), &p, None, "Existing", 0);
    let doc = fx.artifact(&p, 1);
    let key = existing.id.as_str();
    let plan = json!({"callerRef": "plan-1", "features": [{
        "callerRef": "feat-1", "title": "Feature", "description": "", "repositoryIds": [],
        "plan": draft(&doc, &[("E1", "x")], &[]),
        "tasks": [
            {"clientKey": key, "title": "T1", "description": "", "repositoryIds": [],
             "dependencyKeys": [], "acceptance": []},
            {"clientKey": "t2", "title": "T2", "description": "", "repositoryIds": [],
             "dependencyKeys": [key], "acceptance": []}]}],
        "dependencyEdges": []});
    let before = fx.list("tasks.list", &p, 50, None, None).unwrap().0.len();
    let fault = refused(fx.command(
        &fx.agent(),
        "features.create",
        json!({"projectId": p.0.as_str(), "creation": {"kind": "AtomicPlan", "plan": plan}}),
    ));
    assert_eq!(kind_of(&fault), "ambiguous_reference");
    assert_eq!(
        fx.list("tasks.list", &p, 50, None, None).unwrap().0.len(),
        before
    );
}

#[test]
fn a_finding_made_blocking_on_an_unchanged_plan_needs_new_evidence() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    let f = fx.feature(&p, "Checkout", &[]);
    let doc = fx.artifact(&p, 1);
    let f = fx.plan(&f, &doc, &[("E1", "pay")]);
    let f = fx.approve(&f);
    let f = fx.get_at("features.get", &f.id, Some(2)).unwrap();
    let active = f.approval.clone().unwrap();
    let finding = |blocking: bool, evidence: Vec<ArtifactRef>| {
        json!({"id": "F1", "disposition": "NeedsUserDecision", "blocking": blocking,
            "prerequisite": false, "state": "Open", "requirementIds": ["E1"], "scopeKey": "E1",
            "affectedWorkIds": [], "summary": "processor unknown",
            "missing": "which payment processor", "consequence": "cannot verify refunds",
            "attempts": ["read the repository conventions"],
            "insufficiency": "a product choice", "closureCondition": "the processor is named",
            "evidence": evidence, "question": "Stripe or Adyen?", "recommendation": "Stripe",
            "closureEvidence": []})
    };
    let assess = |expected: Option<&str>, finding: Value| {
        let f = fx.fresh(&f);
        let mut change = json!({"kind": "Assess",
            "planRevision": active.plan_revision.as_str(),
            "planDigest": String::from(active.plan_digest.clone()), "summary": "assessed",
            "findings": [finding]});
        if let Some(e) = expected {
            change["expectedAssessmentRevision"] = json!(e);
        }
        fx.command_at(
            &fx.agent(),
            "features.edit",
            json!({"workId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
                "changes": {"readiness": change}}),
            Some(2),
        )
    };
    applied(assess(None, finding(false, vec![])));
    let r = fx
        .get_at("features.get", &f.id, Some(2))
        .unwrap()
        .readiness
        .unwrap();
    assert!(r.ready, "a non-blocking finding does not gate");
    // The same plan, the same evidence: it cannot become a blocker.
    let fault = refused(assess(Some(r.revision.as_str()), finding(true, vec![])));
    assert_eq!(kind_of(&fault), "settled_without_new_evidence");
    // Newly discovered evidence makes it one.
    applied(assess(
        Some(r.revision.as_str()),
        finding(true, vec![fx.artifact(&p, 2)]),
    ));
    let r = fx
        .get_at("features.get", &f.id, Some(2))
        .unwrap()
        .readiness
        .unwrap();
    assert!(!r.ready);
}

#[test]
fn assessing_a_replacement_draft_leaves_the_active_plans_readiness_in_force_until_adoption() {
    use devworks_core::services::runs::scope::Readiness;
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    let f = fx.feature(&p, "Checkout", &[]);
    let doc = fx.artifact(&p, 1);
    let f = fx.plan(&f, &doc, &[("E1", "pay")]);
    let f = fx.approve(&f);
    let f = fx.get_at("features.get", &f.id, Some(2)).unwrap();
    let active = f.approval.clone().unwrap();
    let assess =
        |f: &WorkRecord, rev: &str, digest: String, expected: Option<&str>, findings: Value| {
            let mut change = json!({"kind": "Assess", "planRevision": rev, "planDigest": digest,
            "summary": "assessed", "findings": findings});
            if let Some(e) = expected {
                change["expectedAssessmentRevision"] = json!(e);
            }
            fx.command_at(
                &fx.agent(),
                "features.edit",
                json!({"workId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
                "changes": {"readiness": change}}),
                Some(2),
            )
        };
    // The active plan is assessed ready.
    let f = work_of(applied(assess(
        &f,
        active.plan_revision.as_str(),
        String::from(active.plan_digest.clone()),
        None,
        json!([]),
    )));
    assert!(f.readiness.as_ref().unwrap().ready);
    let unit = |fx: &Fx| {
        let (runs, fid) = (fx.runs.clone(), RootId(f.id.clone()));
        fx.read(move |s| {
            let w = s.sessions().work_root(&fid)?.unwrap();
            runs.scope().plan(s.runs(), &w)
        })
        .unit(Some("E1"))
        .unwrap()
        .readiness
        .clone()
    };
    assert!(unit(&fx).is_current());
    // A replacement draft is assessed with a blocker of E1.
    let f = fx.plan(&f, &doc, &[("E1", "pay by card")]);
    let pending = f.pending_plan.clone().unwrap();
    let r = fx
        .get_at("features.get", &f.id, Some(2))
        .unwrap()
        .readiness
        .unwrap();
    let blocker = json!({"id": "F1", "disposition": "NeedsUserDecision", "blocking": true,
        "prerequisite": false, "state": "Open", "requirementIds": ["E1"], "scopeKey": "E1",
        "affectedWorkIds": [], "summary": "which card network",
        "missing": "the card networks", "consequence": "payments may fail",
        "attempts": ["read the docs"], "insufficiency": "a product choice",
        "closureCondition": "the networks are named", "evidence": [],
        "question": "Visa only?", "recommendation": "Visa and Mastercard",
        "closureEvidence": []});
    let f = work_of(applied(assess(
        &f,
        pending.revision.as_str(),
        plan_digest(&pending.draft),
        Some(r.revision.as_str()),
        json!([blocker]),
    )));
    // The active plan's readiness is untouched: still ready, E1 still current.
    assert!(f.readiness.as_ref().unwrap().ready, "{:?}", f.readiness);
    assert!(!f.pending_readiness.as_ref().unwrap().ready);
    assert!(
        unit(&fx).is_current(),
        "the draft's blocker gates nothing yet"
    );
    // Adopting the draft promotes its assessment: now the blocker holds E1.
    fx.approve(&f);
    assert!(
        matches!(unit(&fx), Readiness::Blocked { .. }),
        "{:?}",
        unit(&fx)
    );
    let f = fx.get_at("features.get", &f.id, Some(2)).unwrap();
    assert!(f.pending_readiness.is_none());
    assert!(!f.readiness.unwrap().ready);
}

/// A feature with an approved, assessed plan (E1) and an assessed replacement draft, plus an
/// `Assess` helper bound to schema revision 2 and a blocking finding of E1.
struct Generations {
    fx: Fx,
    p: ProjectId,
    f: WorkRecord,
}
impl Generations {
    fn new(active_findings: Value) -> Self {
        let fx = Fx::new();
        let p = fx.project("Alpha", &[]);
        let f = fx.feature(&p, "Checkout", &[]);
        let doc = fx.artifact(&p, 1);
        let f = fx.plan(&f, &doc, &[("E1", "pay")]);
        let f = fx.approve(&f);
        let f = fx.get_at("features.get", &f.id, Some(2)).unwrap();
        let active = f.approval.clone().unwrap();
        let g = Self {
            fx,
            p,
            f: f.clone(),
        };
        let f = work_of(applied(g.assess(
            &f,
            active.plan_revision.as_str(),
            String::from(active.plan_digest.clone()),
            None,
            active_findings,
        )));
        let f = g.fx.plan(&f, &doc, &[("E1", "pay by card")]);
        let pending = f.pending_plan.clone().unwrap();
        let r = g.readiness_revision(&f);
        let f = work_of(applied(g.assess(
            &f,
            pending.revision.as_str(),
            plan_digest(&pending.draft),
            Some(&r),
            json!([]),
        )));
        Self { f, ..g }
    }
    fn assess(
        &self,
        f: &WorkRecord,
        rev: &str,
        digest: String,
        expected: Option<&str>,
        findings: Value,
    ) -> DomainCommandReply {
        let mut change = json!({"kind": "Assess", "planRevision": rev, "planDigest": digest,
            "summary": "assessed", "findings": findings});
        if let Some(e) = expected {
            change["expectedAssessmentRevision"] = json!(e);
        }
        self.fx.command_at(
            &self.fx.agent(),
            "features.edit",
            json!({"workId": f.id.as_str(), "expectedRevision": f.revision.as_str(),
                "changes": {"readiness": change}}),
            Some(2),
        )
    }
    fn readiness_revision(&self, f: &WorkRecord) -> String {
        let f = self.fx.get_at("features.get", &f.id, Some(2)).unwrap();
        f.readiness
            .or(f.pending_readiness)
            .unwrap()
            .revision
            .as_str()
            .to_owned()
    }
    fn blocker(evidence: Vec<ArtifactRef>) -> Value {
        json!({"id": "F1", "disposition": "NeedsUserDecision", "blocking": true,
            "prerequisite": false, "state": "Open", "requirementIds": ["E1"], "scopeKey": "E1",
            "affectedWorkIds": [], "summary": "which card network",
            "missing": "the card networks", "consequence": "payments may fail",
            "attempts": ["read the docs"], "insufficiency": "a product choice",
            "closureCondition": "the networks are named", "evidence": evidence,
            "question": "Visa only?", "recommendation": "Visa and Mastercard",
            "closureEvidence": []})
    }
    fn open_f1(r: &devworks_core::protocol::records::ReadinessAssessment) -> bool {
        r.findings.0.iter().any(|x| {
            x.id.as_str() == "F1"
                && matches!(
                    x.state,
                    devworks_core::protocol::records::FindingState::Open
                )
        })
    }
    fn e1(&self) -> devworks_core::services::runs::scope::Readiness {
        let (runs, fid) = (self.fx.runs.clone(), RootId(self.f.id.clone()));
        self.fx
            .read(move |s| {
                let w = s.sessions().work_root(&fid)?.unwrap();
                runs.scope().plan(s.runs(), &w)
            })
            .unit(Some("E1"))
            .unwrap()
            .readiness
            .clone()
    }
}

#[test]
fn a_blocker_found_on_the_active_plan_after_its_replacement_was_assessed_survives_adoption() {
    use devworks_core::services::runs::scope::Readiness;
    let g = Generations::new(json!([]));
    let (fx, f) = (&g.fx, g.f.clone());
    // The draft was assessed first (no finding). Then the active plan's re-assessment finds a
    // blocker of E1, with new evidence.
    let active = fx
        .get_at("features.get", &f.id, Some(2))
        .unwrap()
        .approval
        .unwrap();
    let r = g.readiness_revision(&f);
    let f = work_of(applied(g.assess(
        &f,
        active.plan_revision.as_str(),
        String::from(active.plan_digest.clone()),
        Some(&r),
        json!([Generations::blocker(vec![fx.artifact(&g.p, 2)])]),
    )));
    let shown = fx.get_at("features.get", &f.id, Some(2)).unwrap();
    assert!(!shown.readiness.clone().unwrap().ready);
    // The open finding holds the pending replacement as well...
    let pending = shown.pending_readiness.clone().unwrap();
    assert!(Generations::open_f1(&pending), "{:?}", pending.findings);
    // ...so the draft's re-assessment cannot settle it by restating it resolved.
    let draft = shown.pending_plan.clone().unwrap();
    let mut settled = Generations::blocker(vec![]);
    settled["state"] = json!("Resolved");
    settled["closureNote"] = json!("the draft no longer needs it");
    let fault = refused(g.assess(
        &shown,
        draft.revision.as_str(),
        plan_digest(&draft.draft),
        Some(pending.revision.as_str()),
        json!([settled]),
    ));
    assert_eq!(kind_of(&fault), "open_finding_needs_closure");
    // Approving the draft does not clear the blocker: the adopted plan's E1 stays blocked.
    fx.approve(&shown);
    let after = fx.get_at("features.get", &f.id, Some(2)).unwrap();
    let r = after.readiness.unwrap();
    assert!(Generations::open_f1(&r), "{:?}", r.findings);
    assert!(!r.ready);
    assert!(matches!(g.e1(), Readiness::Blocked { .. }), "{:?}", g.e1());
}

#[test]
fn adoption_keeps_an_open_active_finding_the_pending_assessment_does_not_hold() {
    use devworks_core::services::runs::scope::Readiness;
    let g = Generations::new(json!([Generations::blocker(vec![])]));
    let (fx, f) = (&g.fx, g.f.clone());
    // A pending assessment stored without the active plan's open finding (as one written before
    // the generations were reconciled): adoption still keeps the finding open.
    let fid = f.id.clone();
    fx.write(move |tx| {
        let s = tx.work().readiness(&fid)?.unwrap();
        let mut body = s.body.clone();
        assert_eq!(body["pending"]["findings"][0]["id"], json!("F1"));
        body["pending"]["findings"] = json!([]);
        tx.work().put_readiness(
            &StoredReadiness {
                feature_id: fid.clone(),
                revision: s.revision + 1,
                body,
            },
            Some(s.revision),
        )
    });
    let shown = fx.get_at("features.get", &f.id, Some(2)).unwrap();
    assert!(!Generations::open_f1(
        &shown.pending_readiness.clone().unwrap()
    ));
    fx.approve(&shown);
    let r = fx
        .get_at("features.get", &f.id, Some(2))
        .unwrap()
        .readiness
        .unwrap();
    assert!(Generations::open_f1(&r), "{:?}", r.findings);
    assert!(!r.ready);
    assert!(matches!(g.e1(), Readiness::Blocked { .. }), "{:?}", g.e1());
}

#[test]
fn a_placement_moves_one_card_in_a_bucket_larger_than_an_ordered_list_can_name() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    let f = fx.feature(&p, "F", &[]);
    let cards: Vec<WorkRecord> = (0..130)
        .map(|n| fx.task(&fx.user(), &p, Some(&f), &format!("c{n:03}"), 1))
        .collect();
    let q = feature_queue(&f.id);
    let place = |work: &WorkRecord, anchor: &WorkRecord, side: &str, at: Option<u32>| {
        let body = json!({"projectId": p.0.as_str(), "featureId": f.id.as_str(),
            "expectedQueueRevision": fx.queue(&p, &q).to_string(), "priority": 1,
            "placement": {"workId": work.id.as_str(), "anchorId": anchor.id.as_str(),
                "side": side}});
        fx.command_at(&fx.agent(), "tasks.prioritize", body, at)
    };
    // The 130th card moves before the 129th: one bounded request.
    applied(place(&cards[129], &cards[128], "Before", Some(2)));
    let order = |fx: &Fx| {
        let (all, _) = fx.list("tasks.list", &p, 200, None, None).unwrap();
        ids(&all)
    };
    let now = order(&fx);
    assert_eq!(&now[127..130], ["c127", "c129", "c128"]);
    assert_eq!(
        &now[..2],
        ["c000", "c001"],
        "the rest of the bucket keeps its order"
    );
    // And after the first card: it lands second.
    applied(place(&cards[129], &cards[0], "After", Some(2)));
    assert_eq!(&order(&fx)[..3], ["c000", "c129", "c001"]);
    // Revision 1 does not carry the member; an anchor of another bucket is refused.
    let r1 = place(&cards[1], &cards[0], "Before", None);
    assert!(!matches!(r1, CommandReply::Recorded { .. }));
    let other = fx.task(&fx.user(), &p, Some(&f), "high", 9);
    assert_eq!(
        kind_of(&refused(place(&cards[1], &other, "Before", Some(2)))),
        "priority_bucket"
    );
    // Exactly one of ordered IDs and placement.
    let both = json!({"projectId": p.0.as_str(), "featureId": f.id.as_str(),
        "expectedQueueRevision": fx.queue(&p, &q).to_string(), "priority": 1,
        "orderedTaskIds": [cards[0].id.as_str()],
        "placement": {"workId": cards[1].id.as_str(), "anchorId": cards[0].id.as_str(),
            "side": "Before"}});
    let r = fx.command_at(&fx.agent(), "tasks.prioritize", both, Some(2));
    assert!(
        !matches!(r, CommandReply::Recorded { .. })
            || matches!(recorded(r), DomainDecision::Refused { .. })
    );
}
