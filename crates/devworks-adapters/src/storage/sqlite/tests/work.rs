//! Features, standalone issues and task cards end to end on the real SQLite store: every
//! `features.*`, `issues.*` and `tasks.*` operation through the gateway with host authority and
//! receipts; one identity across the task and issue views; revision-checked edits with no write
//! on refusal; Unicode search and scope-bound cursors; project-wide blocking links with cycle,
//! self, duplicate and cross-project refusal; priority and bucket ordering; workflow moves and
//! manual versus verified completion; membership link/unlink/transfer with the unlink pause; the
//! feature plan slot, approval requests and the human decision seam; readiness assessments and
//! unit-level invalidation feeding run control; re-planning by reference; atomic plans and a
//! project's initial plan; repository-in-use; archived projects; and migration 15. No inference.
use super::{Harness, through};
use crate::storage::sqlite::{MIGRATION_0015_SHA256, MIGRATIONS, SqliteStore};
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
        let digest = devworks_core::fingerprint::sha256_hex(bytes);
        let len = bytes.len() as i64;
        let (aid, vid) = (id(&format!("art-{n}")), id(&format!("ver-{n}")));
        let (p2, a2, v2, d2) = (p.clone(), aid.clone(), vid.clone(), digest.clone());
        self.write(move |tx| {
            tx.artifacts()
                .put_blob(&d2, len, "gen-1", &format!("blobs/{d2}.gen-1"))?;
            let mut a = StoredArtifact {
                id: a2.clone(),
                origin_project_id: Some(p2.clone()),
                origin_work_id: None,
                title: "plan".into(),
                revision: 1,
                scope: ArtifactScope::Project(p2.clone()),
                owner: None,
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
fn readiness_is_bound_to_the_plan_and_invalidates_only_changed_units() {
    let fx = Fx::new();
    let p = fx.project("Alpha", &[]);
    let f = fx.feature(&p, "Checkout", &[]);
    let doc = fx.artifact(&p, 1);
    let f = fx.plan(&f, &doc, &[("E1", "pay"), ("E2", "refund")]);
    let t = fx.task(&fx.user(), &p, Some(&f), "Refund API", 0);
    let f = fx.fresh(&f);
    let pending = f.pending_plan.clone().unwrap();
    let finding = |fid: &str, scope: &str, evidence: Vec<ArtifactRef>| {
        json!({"id": fid, "disposition": "NeedsUserDecision", "blocking": true,
            "prerequisite": false, "state": "Open", "requirementIds": [scope], "scopeKey": scope,
            "affectedWorkIds": [t.id.as_str()], "summary": "processor unknown",
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
    let r = f.readiness.clone().expect("readiness at revision 2");
    assert!(!r.ready, "an unapproved plan is never ready");
    assert!(r.reviewed_pending);
    // Revision 1 readers do not see the member.
    assert!(fx.get("features.get", &f.id).unwrap().readiness.is_none());
    assert!(
        fx.get_at("features.get", &f.id, Some(2))
            .unwrap()
            .readiness
            .is_some()
    );
    // Approval keeps the evidence (adopted content matches) and does not close the finding.
    let f = fx.approve(&f);
    let f = fx.get_at("features.get", &f.id, Some(2)).unwrap();
    let r = f.readiness.clone().unwrap();
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
    for restated in [settled, weaker] {
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
    let a = cards.iter().find(|c| c.title.0 == "A").expect("kept");
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
// Migration 15

#[test]
fn migration_15_is_additive_and_closes_older_writers_out() {
    let m = MIGRATIONS
        .iter()
        .find(|m| m.version == 15)
        .expect("migration 15");
    assert_eq!(
        m.checksum(),
        MIGRATION_0015_SHA256,
        "an applied migration was edited"
    );
    assert_eq!((m.minimum_reader, m.minimum_writer), (8, 15));
    assert!(
        MIGRATIONS
            .iter()
            .filter(|e| e.version < 15)
            .all(|e| e.minimum_reader <= m.minimum_reader && e.minimum_writer <= m.minimum_writer)
    );
    assert!(!m.rebuilds_tables);
    let h = Harness::new();
    let old = SqliteStore::open_with(h.config(), &through(14)).unwrap();
    // A work row recorded before migration 15 (a positional migration 1 fixture row).
    old.with_writer_connection(|c| {
        super::super::conn::db(c.execute_batch(
            "INSERT INTO projects VALUES('project-old','Old','Active',1,1,5);
             INSERT INTO work_items VALUES('work-old','project-old','issue',NULL,NULL,NULL,'Legacy',2,0,3,1,7,'{}');",
        ))
    })
    .unwrap();
    drop(old);
    let s = SqliteStore::open_with(h.config(), &through(15)).unwrap();
    let report = s.compatibility().unwrap();
    assert_eq!(
        (
            report.store_schema_version,
            report.minimum_reader,
            report.minimum_writer
        ),
        (15, 8, 15)
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
