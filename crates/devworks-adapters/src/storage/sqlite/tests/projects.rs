//! Projects, memberships and repository registrations end to end on the real SQLite store:
//! `projects.*` and the repository registration operations through the gateway with host
//! authority and receipts; visibility of project invocations; revisioned edits and the access
//! profile as the project's setting; atomic membership that preserves shared registrations;
//! archival that fences and stops work in its own unit of work; restore that leaves work
//! stopped; deletion that tombstones only app records after quiescence; exact approval for
//! agents; index removal that keeps the registration; listing, search, paging and redacted
//! remotes; and migration 14. No inference.
use super::{Harness, T0, through};
use crate::storage::sqlite::{MIGRATION_0014_SHA256, MIGRATIONS, SqliteStore};
use devworks_core::admission::{self, Selection};
use devworks_core::authority::HostAuthority;
use devworks_core::authority::policy::resolve_policy;
use devworks_core::authority::service::{self as auth, GrantBinding, GrantBounds};
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::gateway::Gateway;
use devworks_core::ports::storage::*;
use devworks_core::protocol::records::{AccessMode, LifeState, ProjectRecord, RepositoryRecord};
use devworks_core::protocol::results::{
    DomainCommandReply, DomainDecision, DomainQueryReply, OperationResult,
};
use devworks_core::protocol::{
    ActorId, CommandReply, Counter, DomainErrorCode, DomainFault, Id, ProjectId, Rejection,
    Revision, RevisionDimension, RootId,
};
use devworks_core::registry::Mode;
use devworks_core::services::UseCases;
use devworks_core::services::projects::{ProjectService, RepositoryService};
use devworks_core::services::runs::{ControlService, Target};
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

struct Fx {
    h: Harness,
    store: Arc<SqliteStore>,
    runs: Arc<ControlService>,
    cases: UseCases,
    n: AtomicU64,
}

impl Fx {
    fn new() -> Self {
        let h = Harness::new();
        let store = Arc::new(h.store());
        let runs = Arc::new(ControlService::new(h.ids.clone()));
        let cases = UseCases::new()
            .with_runs(runs.clone())
            .with_projects(Arc::new(ProjectService::new(h.ids.clone(), runs.clone())))
            .with_repositories(Arc::new(RepositoryService::new(h.ids.clone())))
            .with_resources(Arc::new(
                devworks_core::services::resources::ResourceService::new(
                    h.ids.clone(),
                    h.clock.clone(),
                ),
            ));
        let fx = Self {
            h,
            store,
            runs,
            cases,
            n: AtomicU64::new(0),
        };
        fx.write(|tx| {
            for (a, k) in [
                ("user-a", PrincipalKind::Human),
                ("agent-w", PrincipalKind::Agent),
                ("agent-p", PrincipalKind::Agent),
            ] {
                tx.actors().register(&actor(a), k, a)?;
            }
            Ok(())
        });
        for r in ["repo-1", "repo-2", "repo-3"] {
            fx.register(r, &format!("/Users/me/src/{r}"), None, &[]);
        }
        fx
    }
    /// Binds the agents once the project they are bound to exists: `agent-w` works at the
    /// workspace, `agent-p` inside `p`.
    fn bind_agents(&self, p: &ProjectId) {
        let user = self.user();
        for (who, bound) in [("agent-w", None), ("agent-p", Some(p.clone()))] {
            let by = user.clone();
            let binding = NewPrincipalBinding {
                actor: actor(who),
                kind: PrincipalKind::Agent,
                role: Some(AgentRole::Worker),
                project_id: bound,
                tool_families: None,
                bound_by: actor("user-a"),
            };
            self.write(move |tx| {
                auth::bind_principal(tx, &by, binding, None)
                    .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
            });
        }
    }
    fn register(&self, rid: &str, location: &str, remote: Option<&str>, languages: &[&str]) {
        let r = StoredRepository {
            id: id(rid),
            location: location.into(),
            name: rid.into(),
            remote_url: remote.map(str::to_owned),
            primary: true,
            lifecycle: "Active".into(),
            languages: languages.iter().map(|l| (*l).to_owned()).collect(),
            revision: 1,
            registered_at_ms: T0,
        };
        self.write(move |tx| tx.projects().insert_repository(&r));
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
    fn workspace_agent(&self) -> InvocationContext {
        self.ctx("agent-w", PrincipalKind::Agent, None)
    }
    fn project_agent(&self, p: &ProjectId) -> InvocationContext {
        self.ctx("agent-p", PrincipalKind::Agent, Some(p))
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
    fn frame(op: &str, key: &str, mut body: Value) -> String {
        body["kind"] = json!(op);
        json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": key, "body": body})
            .to_string()
    }
    fn submit(&self, ctx: &InvocationContext, op: &str, frame: &str) -> DomainCommandReply {
        let authority = self.authority();
        let gateway = Gateway {
            authority: &authority,
            store: &*self.store,
            ids: &*self.h.ids,
        };
        self.cases
            .command(&gateway, ctx, op, frame.as_bytes())
            .expect("addressable")
    }
    fn command_keyed(
        &self,
        ctx: &InvocationContext,
        op: &str,
        key: &str,
        body: Value,
    ) -> DomainCommandReply {
        self.submit(ctx, op, &Self::frame(op, key, body))
    }
    fn command(&self, ctx: &InvocationContext, op: &str, body: Value) -> DomainCommandReply {
        let key = format!("key-{}", self.n.fetch_add(1, Ordering::SeqCst));
        self.command_keyed(ctx, op, &key, body)
    }
    fn query(&self, ctx: &InvocationContext, op: &str, mut body: Value) -> DomainQueryReply {
        body["kind"] = json!(op);
        let frame =
            json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": "q-1", "body": body});
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
    fn create(&self, name: &str, repos: &[&str]) -> ProjectRecord {
        project_of(applied(self.command(
            &self.user(),
            "projects.create",
            json!({"name": name, "description": format!("{name} work"), "repositoryIds": repos}),
        )))
    }
    fn get(&self, ctx: &InvocationContext, p: &ProjectId) -> Result<ProjectRecord, DomainFault> {
        match self.query(ctx, "projects.get", json!({"projectId": p.0.as_str()})) {
            DomainQueryReply::Result {
                result: OperationResult::ProjectsGet { value },
                ..
            } => Ok(value),
            DomainQueryReply::Rejected { fault, .. } => Err(fault),
            other => panic!("{other:?}"),
        }
    }
    fn repo(&self, ctx: &InvocationContext, r: &str) -> Result<RepositoryRecord, DomainFault> {
        match self.query(ctx, "repositories.get", json!({"repositoryId": r})) {
            DomainQueryReply::Result {
                result: OperationResult::RepositoriesGet { value },
                ..
            } => Ok(value),
            DomainQueryReply::Rejected { fault, .. } => Err(fault),
            other => panic!("{other:?}"),
        }
    }
    fn lifecycle(
        &self,
        ctx: &InvocationContext,
        op: &str,
        p: &ProjectRecord,
    ) -> DomainCommandReply {
        self.command(
            ctx,
            op,
            json!({"projectId": p.id.0.as_str(), "expectedRevision": p.revision.as_str()}),
        )
    }
    fn events(&self, kind: &str) -> Vec<EventRecord> {
        let mut out = Vec::new();
        let mut after = 0;
        loop {
            let page = self.read(move |r| r.history().page(&EventFilter::All, after, 500));
            out.extend(page.events.into_iter().filter(|e| e.kind == kind));
            if !page.has_more {
                return out;
            }
            after = page.next_after;
        }
    }
    /// Seeds work roots of `p` (as the work-planning item would record them).
    fn seed_roots(&self, p: &ProjectId, roots: &[&str]) {
        let mut sql = String::new();
        for r in roots {
            sql.push_str(&format!(
                "INSERT INTO work_items VALUES('{r}','{}','issue',NULL,NULL,NULL,'{r}',1,0,1,1,1,'{{}}');",
                p.0.as_str()
            ));
        }
        self.store
            .with_writer_connection(move |c| super::super::conn::db(c.execute_batch(&sql)))
            .unwrap();
    }
    fn eligible(&self, root: &str) -> bool {
        let (runs, root) = (self.runs.clone(), RootId(id(root)));
        self.read(move |s| {
            let w = s.sessions().work_root(&root)?.expect("root");
            Ok(runs
                .evaluate(
                    s.runs(),
                    &w,
                    &Target::Root,
                    ActivationPurpose::Implementation,
                )?
                .eligible)
        })
    }
    /// `runs.start` (its specialized envelope) of `root` at its current control revision.
    fn start(&self, ctx: &InvocationContext, root: &str) -> DomainCommandReply {
        let key = format!("start-{}", self.n.fetch_add(1, Ordering::SeqCst));
        let frame = json!({"protocol": "1.0-draft.1", "requestKey": key, "command": {
            "kind": "runs.start", "rootId": root, "expectedWorkRevision": "1",
            "expectedControlRevision": self.control(root).revision.to_string()}});
        self.submit(ctx, "runs.start", &frame.to_string())
    }
    /// What the engine does after an archival commits: its project-wide stop ran.
    fn stop_ran(&self, p: &ProjectId) {
        let p = p.clone();
        self.write(move |tx| {
            let epoch = tx.projects().project(&p)?.expect("project").archive_epoch;
            tx.projects().finish_archive_stop(&p, epoch)
        });
    }
    fn control(&self, root: &str) -> StoredRootControl {
        let root = RootId(id(root));
        self.read(move |s| devworks_core::services::runs::control_of(s.runs(), &root))
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
        DomainDecision::Applied { result } => result,
        other => panic!("expected Applied, got {other:?}"),
    }
}
fn unchanged(reply: DomainCommandReply) -> OperationResult {
    match recorded(reply).0 {
        DomainDecision::NoChange { result } => result,
        other => panic!("expected NoChange, got {other:?}"),
    }
}
fn refused(reply: DomainCommandReply) -> DomainFault {
    match recorded(reply).0 {
        DomainDecision::Refused { fault } => fault,
        other => panic!("expected Refused, got {other:?}"),
    }
}
fn kind(f: &DomainFault) -> (DomainErrorCode, String) {
    (
        f.code,
        f.target_kind
            .as_ref()
            .map(|k| k.0.clone())
            .unwrap_or_default(),
    )
}
fn project_of(r: OperationResult) -> ProjectRecord {
    match r {
        OperationResult::ProjectsCreate { value }
        | OperationResult::ProjectsEdit { value }
        | OperationResult::ProjectsMembership { value }
        | OperationResult::ProjectsArchive { value }
        | OperationResult::ProjectsRestore { value }
        | OperationResult::ProjectsDelete { value }
        | OperationResult::ProjectsGet { value } => value,
        other => panic!("{other:?}"),
    }
}
fn members(p: &ProjectRecord) -> Vec<&str> {
    p.repository_ids.0.iter().map(Id::as_str).collect()
}
fn page_ids(r: DomainQueryReply) -> (Vec<String>, Option<String>) {
    match r {
        DomainQueryReply::Result { result, .. } => match result {
            OperationResult::ProjectsList { value } => (
                value
                    .items
                    .0
                    .iter()
                    .map(|p| p.id.0.as_str().to_owned())
                    .collect(),
                value.next_cursor.map(|c| c.0),
            ),
            OperationResult::RepositoriesList { value }
            | OperationResult::RepositoriesSearch { value } => (
                value
                    .items
                    .0
                    .iter()
                    .map(|p| p.id.as_str().to_owned())
                    .collect(),
                value.next_cursor.map(|c| c.0),
            ),
            other => panic!("{other:?}"),
        },
        DomainQueryReply::Rejected { fault, .. } => panic!("{fault:?}"),
    }
}
fn query_fault(r: DomainQueryReply) -> (DomainErrorCode, String) {
    match r {
        DomainQueryReply::Rejected { fault, .. } => kind(&fault),
        other => panic!("expected a fault, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Create, list and get

#[test]
fn create_records_one_project_and_a_retry_replays_it() {
    let fx = Fx::new();
    let user = fx.user();
    let body = json!({"name": "Engine", "description": "The engine",
        "repositoryIds": ["repo-2", "repo-1"]});
    let first = project_of(applied(fx.command_keyed(
        &user,
        "projects.create",
        "create-1",
        body.clone(),
    )));
    assert_eq!(first.revision.as_str(), "1");
    assert!(matches!(first.lifecycle, LifeState::Active));
    assert!(matches!(first.access_mode, AccessMode::Full));
    assert_eq!(members(&first), ["repo-1", "repo-2"]);
    // The same request key replays the recorded decision: one project, the same identity.
    let (again, replayed) = recorded(fx.command_keyed(&user, "projects.create", "create-1", body));
    assert!(replayed);
    let DomainDecision::Applied { result } = again else {
        panic!("{again:?}")
    };
    assert_eq!(project_of(result).id, first.id);
    let listed = page_ids(fx.query(
        &user,
        "projects.list",
        json!({"scope": {"kind": "Workspace"}, "page": {"limit": 50}}),
    ));
    assert_eq!(listed.0, [first.id.0.as_str()]);
    assert_eq!(fx.events("ProjectCreated").len(), 1);
    // The same key with another intent is an idempotency conflict, never a second project.
    assert!(matches!(
        fx.command_keyed(
            &user,
            "projects.create",
            "create-1",
            json!({"name": "Other", "description": "", "repositoryIds": []}),
        ),
        CommandReply::Rejected {
            code: Rejection::IdempotencyConflict,
            ..
        }
    ));
    let got = fx.get(&user, &first.id).unwrap();
    assert_eq!(got.name.0, "Engine");
    assert_eq!(got.description.0, "The engine");
}

#[test]
fn create_refuses_invalid_unknown_and_unsupported_inputs() {
    let fx = Fx::new();
    let user = fx.user();
    let create = |name: &str, repos: Value, extra: Value| {
        let mut body = json!({"name": name, "description": "", "repositoryIds": repos});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        kind(&refused(fx.command(&user, "projects.create", body)))
    };
    assert_eq!(
        create("  ", json!([]), json!({})),
        (DomainErrorCode::InvalidArgument, "name".into())
    );
    // A repository set with a duplicate is refused before admission (a set identity).
    assert!(matches!(
        fx.command(
            &user,
            "projects.create",
            json!({"name": "A", "description": "", "repositoryIds": ["repo-1", "repo-1"]}),
        ),
        CommandReply::Rejected {
            code: Rejection::InvalidRequest,
            ..
        }
    ));
    assert_eq!(
        create("A", json!(["repo-9"]), json!({})),
        (DomainErrorCode::NotFound, "repository".into())
    );
    let plan = json!({"initialPlan": {"callerRef": "plan-1", "features": [{"callerRef": "f-1",
        "title": "F", "description": "", "plan": {"content": {"artifactId": "a", "versionId": "v",
        "sha256": "a".repeat(64), "byteLength": "1", "mediaType": "text/plain"}, "acceptance": [],
        "repositoryIds": [], "goals": [], "requirements": [], "acceptanceTests": []},
        "tasks": []}], "dependencyEdges": []}});
    let planned = fx.command(&user, "projects.create", {
        let mut b = json!({"name": "A", "description": "", "repositoryIds": []});
        b["initialPlan"] = plan["initialPlan"].clone();
        b
    });
    // A strict decoder may refuse a draft shape before the use case; either way nothing is
    // created and the plan is never silently ignored.
    match planned {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Refused { fault } => assert_eq!(
                kind(&fault),
                (
                    DomainErrorCode::UnsupportedCapability,
                    "initial_plan".into()
                )
            ),
            other => panic!("{other:?}"),
        },
        CommandReply::Rejected { .. } => {}
        other => panic!("{other:?}"),
    }
    let all = fx.read(|s| {
        s.projects().projects(
            &ProjectFilter {
                only: None,
                lifecycles: vec!["Active".into(), "Archived".into(), "Deleted".into()],
            },
            None,
            50,
        )
    });
    assert!(all.is_empty(), "nothing was created: {all:?}");
}

#[test]
fn a_project_invocation_sees_only_its_own_project_and_members() {
    let fx = Fx::new();
    let a = fx.create("A", &["repo-1", "repo-2"]);
    let b = fx.create("B", &["repo-2", "repo-3"]);
    fx.bind_agents(&a.id);
    let agent = fx.project_agent(&a.id);
    assert_eq!(
        query_fault(fx.query(
            &agent,
            "projects.list",
            json!({"scope": {"kind": "Workspace"}, "page": {"limit": 50}})
        )),
        (DomainErrorCode::PermissionDenied, "workspace_scope".into())
    );
    let own = page_ids(fx.query(
        &agent,
        "projects.list",
        json!({"scope": {"kind": "Project", "projectId": a.id.0.as_str()}, "page": {"limit": 50}}),
    ));
    assert_eq!(own.0, [a.id.0.as_str()]);
    // Another project does not exist for it.
    assert_eq!(
        kind(&fx.get(&agent, &b.id).unwrap_err()),
        (DomainErrorCode::NotFound, "project".into())
    );
    assert_eq!(
        query_fault(fx.query(
            &agent,
            "repositories.list",
            json!({"scope": {"kind": "Project", "projectId": b.id.0.as_str()}, "page": {"limit": 50}})
        )),
        (DomainErrorCode::NotFound, "project".into())
    );
    assert_eq!(
        kind(&fx.repo(&agent, "repo-3").unwrap_err()),
        (DomainErrorCode::NotFound, "repository".into())
    );
    // A shared repository names only the projects the invocation may see.
    let shared = fx.repo(&agent, "repo-2").unwrap();
    assert_eq!(shared.project_ids.0, std::slice::from_ref(&a.id));
    let mut both = vec![a.id.0.as_str().to_owned(), b.id.0.as_str().to_owned()];
    both.sort();
    let seen: Vec<String> = fx
        .repo(&fx.user(), "repo-2")
        .unwrap()
        .project_ids
        .0
        .iter()
        .map(|p| p.0.as_str().to_owned())
        .collect();
    assert_eq!(seen, both);
    // A project invocation cannot create a project outside its boundary.
    assert_eq!(
        kind(&refused(fx.command(
            &agent,
            "projects.create",
            json!({"name": "C", "description": "", "repositoryIds": []}),
        ))),
        (DomainErrorCode::PermissionDenied, "workspace_scope".into())
    );
    // A workspace agent may create one.
    let made = project_of(applied(fx.command(
        &fx.workspace_agent(),
        "projects.create",
        json!({"name": "C", "description": "", "repositoryIds": ["repo-3"]}),
    )));
    assert_eq!(members(&made), ["repo-3"]);
}

// ---------------------------------------------------------------------------------------------
// Edit and access mode

#[test]
fn edits_are_revisioned_and_the_access_mode_is_the_projects_setting() {
    let fx = Fx::new();
    let p = fx.create("A", &[]);
    fx.bind_agents(&p.id);
    let user = fx.user();
    let stale = refused(fx.command(
        &user,
        "projects.edit",
        json!({"projectId": p.id.0.as_str(), "expectedRevision": "7", "changes": {"name": "B"}}),
    ));
    assert_eq!(stale.code, DomainErrorCode::RevisionConflict);
    assert!(matches!(
        stale.revision_dimension,
        Some(RevisionDimension::Scope)
    ));
    assert_eq!(stale.current_revision.as_ref().unwrap().as_str(), "1");
    let renamed = project_of(applied(fx.command(
        &user,
        "projects.edit",
        json!({"projectId": p.id.0.as_str(), "expectedRevision": "1", "changes": {"name": "B"}}),
    )));
    assert_eq!(
        (renamed.name.0.as_str(), renamed.revision.as_str()),
        ("B", "2")
    );
    // An equal patch changes nothing.
    let same = project_of(unchanged(fx.command(
        &user,
        "projects.edit",
        json!({"projectId": p.id.0.as_str(), "expectedRevision": "2",
            "changes": {"name": "B", "accessMode": "Full"}}),
    )));
    assert_eq!(same.revision.as_str(), "2");
    // An agent may rename, but never change the access profile.
    let agent = fx.workspace_agent();
    assert_eq!(
        kind(&refused(fx.command(
            &agent,
            "projects.edit",
            json!({"projectId": p.id.0.as_str(), "expectedRevision": "2",
                "changes": {"name": "C", "accessMode": "Project"}}),
        ))),
        (DomainErrorCode::PermissionDenied, "access_mode".into())
    );
    assert_eq!(
        fx.get(&user, &p.id).unwrap().name.0,
        "B",
        "refused as a whole"
    );
    let scoped = project_of(applied(fx.command(
        &user,
        "projects.edit",
        json!({"projectId": p.id.0.as_str(), "expectedRevision": "2",
            "changes": {"accessMode": "Project"}}),
    )));
    assert!(matches!(scoped.access_mode, AccessMode::Project));
    // The project's effective access policy is what authority enforces.
    let pid = p.id.clone();
    let profile = fx.read(move |s| resolve_policy(s.configuration(), Some(&pid)));
    assert_eq!(
        profile.profile,
        Some(devworks_core::authority::AccessProfile::ProjectScope)
    );
    assert_eq!(fx.events("SettingsRevisionSaved").len(), 1);
    // The workspace keeps its own profile.
    let ws = fx.read(|s| resolve_policy(s.configuration(), None));
    assert_eq!(
        ws.profile,
        Some(devworks_core::authority::AccessProfile::Full)
    );
}

// ---------------------------------------------------------------------------------------------
// Membership

#[test]
fn membership_is_replaced_atomically_and_shared_registrations_survive() {
    let fx = Fx::new();
    let a = fx.create("A", &["repo-1", "repo-2"]);
    let b = fx.create("B", &["repo-2"]);
    fx.bind_agents(&a.id);
    let user = fx.user();
    let set = |ctx: &InvocationContext, p: &ProjectRecord, repos: Value| {
        fx.command(
            ctx,
            "projects.membership",
            json!({"projectId": p.id.0.as_str(), "expectedRevision": p.revision.as_str(),
                "repositoryIds": repos}),
        )
    };
    let after = project_of(applied(set(&user, &a, json!(["repo-1", "repo-3"]))));
    assert_eq!(members(&after), ["repo-1", "repo-3"]);
    assert_eq!(after.revision.as_str(), "2");
    let stored = fx.read({
        let pid = a.id.clone();
        move |s| s.projects().project(&pid)
    });
    assert_eq!(
        stored.unwrap().scope_revision,
        2,
        "membership is the boundary"
    );
    // The shared repository left A only: still registered, still in B, same location.
    let shared = fx.repo(&user, "repo-2").unwrap();
    assert_eq!(shared.project_ids.0, std::slice::from_ref(&b.id));
    let location = fx.read(|s| s.projects().repository(&id("repo-2"))).unwrap();
    assert_eq!(location.location, "/Users/me/src/repo-2");
    assert_eq!(members(&fx.get(&user, &b.id).unwrap()), ["repo-2"]);
    // The same set changes nothing.
    assert_eq!(
        members(&project_of(unchanged(set(
            &user,
            &after,
            json!(["repo-3", "repo-1"])
        )))),
        ["repo-1", "repo-3"]
    );
    // A repository the project's work names cannot leave it, and nothing else changes.
    fx.seed_roots(&a.id, &["issue-a"]);
    fx.store
        .with_writer_connection(|c| {
            super::super::conn::db(
                c.execute_batch("INSERT INTO work_repositories VALUES('issue-a','repo-1');"),
            )
        })
        .unwrap();
    assert_eq!(
        kind(&refused(set(&user, &after, json!(["repo-2"])))),
        (DomainErrorCode::Conflict, "repository_in_use".into())
    );
    assert_eq!(
        members(&fx.get(&user, &a.id).unwrap()),
        ["repo-1", "repo-3"]
    );
    // A project invocation can only change its boundary with repositories it can see.
    let agent = fx.project_agent(&a.id);
    assert_eq!(
        kind(&refused(set(
            &agent,
            &after,
            json!(["repo-1", "repo-2", "repo-3"])
        ))),
        (DomainErrorCode::NotFound, "repository".into())
    );
    // A stale revision is a conflict on the project's boundary.
    assert_eq!(
        refused(set(&user, &a, json!(["repo-1"]))).code,
        DomainErrorCode::RevisionConflict
    );
}

// ---------------------------------------------------------------------------------------------
// Archive, restore and delete

#[test]
fn archival_fences_and_stops_every_root_in_its_own_unit_of_work() {
    let fx = Fx::new();
    let a = fx.create("A", &["repo-1"]);
    let b = fx.create("B", &[]);
    fx.seed_roots(&a.id, &["issue-a1", "issue-a2"]);
    fx.seed_roots(&b.id, &["issue-b1"]);
    let user = fx.user();
    let start = |root: &str| fx.start(&user, root);
    let _ = applied(start("issue-a1"));
    let _ = applied(start("issue-b1"));
    assert!(fx.eligible("issue-a1") && fx.eligible("issue-a2"));
    let archived = project_of(applied(fx.lifecycle(&user, "projects.archive", &a)));
    assert!(matches!(archived.lifecycle, LifeState::Archived));
    assert_eq!(archived.revision.as_str(), "2");
    // From the archive's commit: no root of A is eligible, each is paused under a stop with a
    // manual-resume hold, and the stops are journaled.
    for r in ["issue-a1", "issue-a2"] {
        assert!(!fx.eligible(r), "{r}");
        let c = fx.control(r);
        assert_eq!(c.desired, DesiredExecution::Paused, "{r}");
        assert!(c.stop_operation_id.is_some(), "{r}");
    }
    let event = &fx.events("ProjectArchived")[0];
    assert_eq!(event.payload["stoppedRoots"].as_array().unwrap().len(), 2);
    // Other projects continue.
    assert!(fx.eligible("issue-b1"));
    assert_eq!(fx.control("issue-b1").desired, DesiredExecution::Enabled);
    // No work is allocated in an archived project, and its boundary is frozen.
    let (code, target) = kind(&refused(start("issue-a2")));
    assert_eq!(
        (code, target.as_str()),
        (DomainErrorCode::Conflict, "archived_project")
    );
    assert_eq!(
        kind(&refused(fx.command(
            &user,
            "projects.edit",
            json!({"projectId": a.id.0.as_str(), "expectedRevision": "2",
                "changes": {"name": "X"}}),
        ))),
        (DomainErrorCode::Conflict, "project_archived".into())
    );
    // Nor a shared resource: an archived project allocates nothing.
    let in_a = fx.ctx("user-a", PrincipalKind::Human, Some(&a.id));
    let frame = json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": "acquire-a",
        "schemaRevision": 2, "body": {"kind": "resources.acquire", "resourceId": "gpu",
        "expectedRevision": "1", "units": 1, "leaseMs": "60000"}});
    assert_eq!(
        kind(&refused(fx.submit(
            &in_a,
            "resources.acquire",
            &frame.to_string()
        ))),
        (DomainErrorCode::Conflict, "archived_project".into())
    );
    let in_b = fx.ctx("user-a", PrincipalKind::Human, Some(&b.id));
    let frame = json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": "acquire-b",
        "schemaRevision": 2, "body": {"kind": "resources.acquire", "resourceId": "gpu",
        "expectedRevision": "1", "units": 1, "leaseMs": "60000"}});
    let _ = applied(fx.submit(&in_b, "resources.acquire", &frame.to_string()));
    // Archiving again changes nothing; the listing filters by lifecycle.
    assert!(matches!(
        project_of(unchanged(fx.lifecycle(
            &user,
            "projects.archive",
            &archived
        )))
        .lifecycle,
        LifeState::Archived
    ));
    let only_archived = page_ids(fx.query(
        &user,
        "projects.list",
        json!({"scope": {"kind": "Workspace"}, "page": {"limit": 50}, "lifecycle": "Archived"}),
    ));
    assert_eq!(only_archived.0, [a.id.0.as_str()]);
}

#[test]
fn restore_returns_the_project_but_leaves_its_work_stopped() {
    let fx = Fx::new();
    let a = fx.create("A", &[]);
    fx.seed_roots(&a.id, &["issue-a1"]);
    let user = fx.user();
    let archived = project_of(applied(fx.lifecycle(&user, "projects.archive", &a)));
    // Until the archival's project-wide stop has run, restore waits for it (a restore before
    // that stop read the lifecycle would let the archived epoch's trees run on).
    let waiting = refused(fx.lifecycle(&user, "projects.restore", &archived));
    assert_eq!(
        kind(&waiting),
        (DomainErrorCode::Conflict, "project_stopping".into())
    );
    assert!(matches!(
        waiting.retry,
        devworks_core::protocol::RetryAdvice::AfterBlockerClears
    ));
    let pending = fx.read(|s| s.projects().pending_archive_stops(10));
    assert_eq!(pending, std::slice::from_ref(&a.id));
    fx.stop_ran(&a.id);
    assert!(
        fx.read(|s| s.projects().pending_archive_stops(10))
            .is_empty()
    );
    let restored = project_of(applied(fx.lifecycle(&user, "projects.restore", &archived)));
    assert!(matches!(restored.lifecycle, LifeState::Active));
    assert_eq!(restored.revision.as_str(), "3");
    // The root keeps the hold the archival left: Start explains that Continue is required.
    assert_eq!(fx.control("issue-a1").desired, DesiredExecution::Paused);
    assert!(!fx.eligible("issue-a1"));
    let started = refused(fx.start(&user, "issue-a1"));
    assert_eq!(started.code, DomainErrorCode::ContinueRequired);
    assert!(!started.blocker_ids.0.is_empty());
    // Restoring an active project changes nothing.
    assert!(matches!(
        project_of(unchanged(fx.lifecycle(
            &user,
            "projects.restore",
            &restored
        )))
        .lifecycle,
        LifeState::Active
    ));
}

#[test]
fn deletion_follows_archival_and_quiescence_and_tombstones_only_app_records() {
    let fx = Fx::new();
    let a = fx.create("A", &["repo-1", "repo-2"]);
    let b = fx.create("B", &["repo-2"]);
    let busy = fx.create("Busy", &[]);
    fx.seed_roots(&busy.id, &["issue-busy"]);
    let user = fx.user();
    assert_eq!(
        kind(&refused(fx.lifecycle(&user, "projects.delete", &a))),
        (DomainErrorCode::Conflict, "project_active".into())
    );
    // A project whose work is still stopping is not quiescent.
    let _ = applied(fx.start(&user, "issue-busy"));
    let runs = fx.runs.clone();
    fx.write(move |tx| {
        let a = runs
            .claim_activation(tx, &id("supervisor-1"))?
            .expect("claimable");
        let _ = runs.start_activation(tx, &a.id, a.owner_epoch)?;
        Ok(())
    });
    let busy = project_of(applied(fx.lifecycle(&user, "projects.archive", &busy)));
    fx.stop_ran(&busy.id);
    let pending = refused(fx.lifecycle(&user, "projects.delete", &busy));
    assert_eq!(
        kind(&pending),
        (DomainErrorCode::Conflict, "project_not_quiescent".into())
    );
    assert!(matches!(
        pending.retry,
        devworks_core::protocol::RetryAdvice::AfterBlockerClears
    ));
    let archived = project_of(applied(fx.lifecycle(&user, "projects.archive", &a)));
    // Deletion waits for the archival's stop too.
    assert_eq!(
        kind(&refused(fx.lifecycle(&user, "projects.delete", &archived))),
        (DomainErrorCode::Conflict, "project_stopping".into())
    );
    fx.stop_ran(&a.id);
    let deleted = project_of(applied(fx.lifecycle(&user, "projects.delete", &archived)));
    assert!(matches!(deleted.lifecycle, LifeState::Deleted));
    assert!(deleted.repository_ids.0.is_empty());
    // Source registrations are untouched: same location, still registered, still shared.
    for r in ["repo-1", "repo-2"] {
        let stored = fx.read(move |s| s.projects().repository(&id(r))).unwrap();
        assert_eq!(stored.location, format!("/Users/me/src/{r}"));
        assert_eq!(stored.lifecycle, "Active");
    }
    assert_eq!(
        fx.repo(&user, "repo-2").unwrap().project_ids.0,
        std::slice::from_ref(&b.id)
    );
    assert!(fx.repo(&user, "repo-1").unwrap().project_ids.0.is_empty());
    let event = &fx.events("ProjectDeleted")[0];
    assert_eq!(event.payload["sourceResourcesTouched"], json!(false));
    // The tombstone stays readable; listings leave it out unless asked for.
    assert!(matches!(
        fx.get(&user, &a.id).unwrap().lifecycle,
        LifeState::Deleted
    ));
    let listed = page_ids(fx.query(
        &user,
        "projects.list",
        json!({"scope": {"kind": "Workspace"}, "page": {"limit": 50}}),
    ));
    assert!(!listed.0.contains(&a.id.0.as_str().to_owned()));
    let tombstones = page_ids(fx.query(
        &user,
        "projects.list",
        json!({"scope": {"kind": "Workspace"}, "page": {"limit": 50}, "lifecycle": "Deleted"}),
    ));
    assert_eq!(tombstones.0, [a.id.0.as_str()]);
    // A tombstone takes no configuration and cannot be restored; deleting again is a no-op.
    let pid = a.id.clone();
    assert!(!fx.read(move |s| s.configuration().scope_exists(&ConfigScope::Project(pid))));
    assert_eq!(
        kind(&refused(fx.lifecycle(&user, "projects.restore", &deleted))),
        (DomainErrorCode::Conflict, "project_deleted".into())
    );
    assert!(matches!(
        project_of(unchanged(fx.lifecycle(&user, "projects.delete", &deleted))).lifecycle,
        LifeState::Deleted
    ));
}

#[test]
fn a_deleted_projects_work_stays_closed_to_dispatch() {
    let fx = Fx::new();
    let a = fx.create("A", &[]);
    fx.seed_roots(&a.id, &["issue-a1"]);
    let user = fx.user();
    let archived = project_of(applied(fx.lifecycle(&user, "projects.archive", &a)));
    fx.stop_ran(&a.id);
    let _ = applied(fx.lifecycle(&user, "projects.delete", &archived));
    assert!(!fx.eligible("issue-a1"));
    let (code, target) = kind(&refused(fx.start(&user, "issue-a1")));
    assert_eq!(
        (code, target.as_str()),
        (DomainErrorCode::Conflict, "archived_project")
    );
}

#[test]
fn agents_archive_delete_and_remove_indexes_only_with_an_exact_approval() {
    let fx = Fx::new();
    let a = fx.create("A", &["repo-1"]);
    fx.bind_agents(&a.id);
    let agent = fx.workspace_agent();
    let frame = Fx::frame(
        "projects.archive",
        "agent-archive",
        json!({"projectId": a.id.0.as_str(), "expectedRevision": "1"}),
    );
    assert!(matches!(
        fx.submit(&agent, "projects.archive", &frame),
        CommandReply::Rejected {
            code: Rejection::PermissionDenied,
            ..
        }
    ));
    let denied = fx.events("AuthorityDenied");
    assert_eq!(
        denied.last().unwrap().payload["reason"],
        json!("ApprovalRequired")
    );
    assert!(matches!(
        fx.get(&fx.user(), &a.id).unwrap().lifecycle,
        LifeState::Active
    ));
    for (op, body) in [
        (
            "projects.delete",
            json!({"projectId": a.id.0.as_str(), "expectedRevision": "1"}),
        ),
        (
            "repositories.remove_index",
            json!({"repositoryId": "repo-1", "expectedRevision": "1"}),
        ),
    ] {
        assert!(
            matches!(
                fx.command(&agent, op, body),
                CommandReply::Rejected {
                    code: Rejection::PermissionDenied,
                    ..
                }
            ),
            "{op}"
        );
    }
    // The user's exact approval of this request lets the agent run it once.
    let digest = admission::decode(
        Selection {
            operation: "projects.archive",
            mode: Mode::Command,
        },
        frame.as_bytes(),
    )
    .unwrap()
    .fingerprint
    .as_str()
    .to_owned();
    let user = fx.user();
    let grant = fx
        .write(move |tx| {
            Ok(auth::mint_grant(
                tx,
                &user,
                actor("agent-w"),
                GrantKind::Operation,
                GrantBinding {
                    operation: "projects.archive".into(),
                    action: None,
                    request_digest: Some(digest),
                    project_id: None,
                    root_id: None,
                    policy_digest: None,
                },
                GrantBounds {
                    expires_at_ms: T0 + 60_000,
                    uses: 1,
                },
            ))
        })
        .unwrap();
    let approved = agent.clone().with_authority_source(grant.id.clone());
    let archived = project_of(applied(fx.submit(&approved, "projects.archive", &frame)));
    fx.stop_ran(&a.id);
    assert!(matches!(archived.lifecycle, LifeState::Archived));
    // Restore is not destructive: the agent may restore within its scope.
    assert!(matches!(
        project_of(applied(fx.lifecycle(&agent, "projects.restore", &archived))).lifecycle,
        LifeState::Active
    ));
    // A proposal reference is not a confirmation (no approval proposal exists).
    let user = fx.user();
    let restored = fx.get(&user, &a.id).unwrap();
    assert_eq!(
        kind(&refused(fx.command(
            &user,
            "projects.archive",
            json!({"projectId": a.id.0.as_str(), "expectedRevision": restored.revision.as_str(),
                "proposalId": "proposal-1"}),
        ))),
        (DomainErrorCode::NotFound, "proposal".into())
    );
}

// ---------------------------------------------------------------------------------------------
// Shared resources of an archived project

impl Fx {
    fn resource(&self, ctx: &InvocationContext, op: &str, body: Value) -> DomainCommandReply {
        let mut body = body;
        body["kind"] = json!(op);
        let key = format!("res-{}", self.n.fetch_add(1, Ordering::SeqCst));
        let frame = json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": key,
            "schemaRevision": 2, "body": body});
        self.submit(ctx, op, &frame.to_string())
    }
    fn acquire(&self, ctx: &InvocationContext) -> devworks_core::protocol::records::LeaseRecord {
        match recorded(self.resource(
            ctx,
            "resources.acquire",
            json!({"resourceId": "pool", "expectedRevision": "1", "units": 1,
                "leaseMs": "600000"}),
        ))
        .0
        {
            DomainDecision::Applied {
                result: OperationResult::ResourcesAcquire { value },
            }
            | DomainDecision::NoChange {
                result: OperationResult::ResourcesAcquire { value },
            } => value,
            other => panic!("{other:?}"),
        }
    }
    fn lease_phase(&self, l: &devworks_core::protocol::records::LeaseRecord) -> String {
        let id = l.id.clone();
        let stored = self.read(move |s| s.resources().lease(&id)).expect("lease");
        format!("{:?}/{:?}", stored.phase, stored.end_reason)
    }
}

/// A shared machine resource (capacity one) held by project B, with project A waiting.
fn queued(
    fx: &Fx,
) -> (
    ProjectRecord,
    devworks_core::protocol::records::LeaseRecord,
    devworks_core::protocol::records::LeaseRecord,
    InvocationContext,
) {
    let a = fx.create("A", &[]);
    let b = fx.create("B", &[]);
    fx.bind_agents(&a.id);
    fx.write(|tx| {
        devworks_core::services::resources::register_machine(
            tx,
            &devworks_core::services::resources::MachineResource {
                name: "pool".into(),
                capacity: 1,
                physical: false,
                labels: Vec::new(),
            },
        )
    });
    let in_b = fx.ctx("user-a", PrincipalKind::Human, Some(&b.id));
    let held = fx.acquire(&in_b);
    let waiting = fx.acquire(&fx.project_agent(&a.id));
    assert_eq!(fx.lease_phase(&held), "Held/None");
    assert_eq!(fx.lease_phase(&waiting), "Waiting/None");
    (a, held, waiting, in_b)
}

#[test]
fn archival_cancels_the_projects_waiting_leases_in_its_unit_of_work() {
    let fx = Fx::new();
    let (a, held, waiting, in_b) = queued(&fx);
    let _ = applied(fx.lifecycle(&fx.user(), "projects.archive", &a));
    assert_eq!(fx.lease_phase(&waiting), "Released/Some(Cancelled)");
    // The holder's release grants nothing to the archived project.
    let _ = recorded(fx.resource(
        &in_b,
        "resources.release",
        json!({"leaseId": held.id.as_str(), "expectedFence": held.fence.as_str()}),
    ));
    assert_eq!(fx.lease_phase(&waiting), "Released/Some(Cancelled)");
    assert_eq!(
        fx.events("ProjectArchived")[0].payload["cancelledWaitingLeases"],
        json!(1)
    );
}

#[test]
fn a_waiter_of_a_closed_project_is_never_promoted() {
    let fx = Fx::new();
    let (a, held, waiting, in_b) = queued(&fx);
    // A waiter that is still queued when its project closes (here closed beneath the archival
    // command, so nothing cancelled it) leaves the queue at the next promotion instead of being
    // granted.
    let pid = a.id.clone();
    fx.write(move |tx| {
        let mut p = tx.projects().project(&pid)?.expect("project");
        let revision = p.revision;
        p.revision += 1;
        p.lifecycle = "Archived".into();
        tx.projects().update_project(&p, revision)
    });
    let _ = recorded(fx.resource(
        &in_b,
        "resources.release",
        json!({"leaseId": held.id.as_str(), "expectedFence": held.fence.as_str()}),
    ));
    assert_eq!(fx.lease_phase(&waiting), "Released/Some(Cancelled)");
}

#[test]
fn a_hold_of_an_archived_project_is_not_renewed() {
    let fx = Fx::new();
    let (a, held, _, in_b) = queued(&fx);
    let _ = recorded(fx.resource(
        &in_b,
        "resources.release",
        json!({"leaseId": held.id.as_str(), "expectedFence": held.fence.as_str()}),
    ));
    // A now holds the resource; after its archival the hold is not extended.
    let agent = fx.project_agent(&a.id);
    let mine = fx.acquire(&agent);
    assert_eq!(fx.lease_phase(&mine), "Held/None");
    let _ = applied(fx.lifecycle(&fx.user(), "projects.archive", &a));
    assert_eq!(
        kind(&refused(fx.resource(
            &agent,
            "resources.renew",
            json!({"leaseId": mine.id.as_str(), "expectedFence": mine.fence.as_str(),
                "leaseMs": "600000"}),
        ))),
        (DomainErrorCode::Conflict, "archived_project".into())
    );
}

// ---------------------------------------------------------------------------------------------
// Repository texts, index removal, listing and search

fn publish(fx: &Fx, view: &str, repo: &str, at: i64, readme: bool) {
    let (v, r) = (
        StoredIndexView {
            view_id: id(view),
            repository_id: id(repo),
            source_revision: format!("rev-{view}"),
            coverage: "Complete".into(),
            published_at_ms: at,
        },
        readme,
    );
    fx.write(move |tx| {
        tx.projects().publish_index_view(&v)?;
        if r {
            tx.projects().put_repository_text(&StoredRepositoryText {
                view_id: v.view_id.clone(),
                repository_id: v.repository_id.clone(),
                kind: TextKind::Readme,
                artifact_id: id(&format!("artifact-{}", v.view_id.as_str())),
                version_id: id("version-1"),
                sha256: "b".repeat(64),
                byte_length: 12,
                media_type: "text/markdown".into(),
                coverage: "Complete".into(),
            })?;
        }
        Ok(())
    });
}

#[test]
fn readme_and_summary_read_the_selected_view_and_index_removal_keeps_the_registration() {
    let fx = Fx::new();
    let a = fx.create("A", &["repo-1"]);
    let user = fx.user();
    let text = |op: &str, views: Value| {
        fx.query(&user, op, json!({"repositoryId": "repo-1", "views": views}))
    };
    let published = json!({"kind": "Published", "repositoryIds": ["repo-1"]});
    // Nothing is indexed yet: an honest unavailable view, never an empty text.
    assert_eq!(
        query_fault(text("repositories.readme", published.clone())),
        (DomainErrorCode::ViewUnavailable, "repository_view".into())
    );
    publish(&fx, "view-1", "repo-1", T0, true);
    publish(&fx, "view-2", "repo-1", T0 + 10, true);
    let readme = match text("repositories.readme", published.clone()) {
        DomainQueryReply::Result {
            result: OperationResult::RepositoriesReadme { value },
            ..
        } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        readme.view_id.as_str(),
        "view-2",
        "the latest published view"
    );
    assert_eq!(readme.content.artifact_id.as_str(), "artifact-view-2");
    let exact = match text(
        "repositories.readme",
        json!({"kind": "Exact", "viewIds": ["view-1"]}),
    ) {
        DomainQueryReply::Result {
            result: OperationResult::RepositoriesReadme { value },
            ..
        } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(exact.view_id.as_str(), "view-1");
    assert_eq!(
        query_fault(text("repositories.summary", published.clone())),
        (
            DomainErrorCode::ViewUnavailable,
            "repository_summary".into()
        )
    );
    assert_eq!(
        query_fault(text(
            "repositories.readme",
            json!({"kind": "Published", "repositoryIds": ["repo-2"]})
        )),
        (DomainErrorCode::InvalidArgument, "views".into())
    );
    assert_eq!(
        query_fault(text(
            "repositories.readme",
            json!({"kind": "Pinned", "pinId": "pin-1"})
        )),
        (DomainErrorCode::UnsupportedCapability, "view_pin".into())
    );
    let record = fx.repo(&user, "repo-1").unwrap();
    assert_eq!(
        record
            .indexed_view_ids
            .0
            .iter()
            .map(Id::as_str)
            .collect::<Vec<_>>(),
        ["view-2", "view-1"]
    );
    // Index removal is revisioned on the registration.
    let stale = refused(fx.command(
        &user,
        "repositories.remove_index",
        json!({"repositoryId": "repo-1", "expectedRevision": "4"}),
    ));
    assert!(matches!(
        stale.revision_dimension,
        Some(RevisionDimension::Registry)
    ));
    let removed = match applied(fx.command(
        &user,
        "repositories.remove_index",
        json!({"repositoryId": "repo-1", "expectedRevision": "1"}),
    )) {
        OperationResult::RepositoriesRemoveIndex { value } => value,
        other => panic!("{other:?}"),
    };
    assert!(removed.changed);
    assert_eq!(removed.revision.as_str(), "2");
    // The registration, its membership and its location are kept; only the index went.
    let after = fx.repo(&user, "repo-1").unwrap();
    assert!(after.indexed_view_ids.0.is_empty());
    assert_eq!(after.project_ids.0, std::slice::from_ref(&a.id));
    assert_eq!(members(&fx.get(&user, &a.id).unwrap()), ["repo-1"]);
    let stored = fx.read(|s| s.projects().repository(&id("repo-1"))).unwrap();
    assert_eq!(stored.location, "/Users/me/src/repo-1");
    assert_eq!(
        query_fault(text("repositories.readme", published)),
        (DomainErrorCode::ViewUnavailable, "repository_view".into())
    );
    assert_eq!(fx.events("RepositoryIndexRemoved").len(), 1);
    // With nothing left to remove the command changes nothing.
    match unchanged(fx.command(
        &user,
        "repositories.remove_index",
        json!({"repositoryId": "repo-1", "expectedRevision": "2"}),
    )) {
        OperationResult::RepositoriesRemoveIndex { value } => assert!(!value.changed),
        other => panic!("{other:?}"),
    }
}

#[test]
fn repository_listing_and_search_filter_page_and_never_show_credentials() {
    let fx = Fx::new();
    // A registration never stores credentials ...
    let leaky = StoredRepository {
        id: id("repo-9"),
        location: "/Users/me/src/leaky".into(),
        name: "leaky".into(),
        remote_url: Some("https://ghp_TOKEN@github.com/acme/leaky.git".into()),
        primary: true,
        lifecycle: "Active".into(),
        languages: Vec::new(),
        revision: 1,
        registered_at_ms: T0,
    };
    assert!(matches!(
        run_write(&*fx.store, WriteOptions::default(), move |tx| tx
            .projects()
            .insert_repository(&leaky)),
        Err(StorageError::InvalidRecord(_))
    ));
    // ... and one recorded before that rule is shown without them.
    fx.store
        .with_writer_connection(|c| {
            super::super::conn::db(c.execute_batch(
                "INSERT INTO repositories VALUES('repo-4','/Users/me/src/engine',1);
                 INSERT INTO repository_details VALUES('repo-4','repo-4',
                   'https://me:ghp_TOKEN@github.com/acme/engine.git?x=1',1,'Active',
                   '[\"rust\",\"python\"]',1);",
            ))
        })
        .unwrap();
    fx.register(
        "repo-5",
        "/Users/me/src/site",
        Some("git@github.com:acme/site.git"),
        &["typescript"],
    );
    let user = fx.user();
    let list = |extra: Value| {
        let mut body = json!({"scope": {"kind": "Workspace"}, "page": {"limit": 50}});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        page_ids(fx.query(&user, "repositories.list", body)).0
    };
    assert_eq!(
        list(json!({})),
        ["repo-1", "repo-2", "repo-3", "repo-4", "repo-5"]
    );
    assert_eq!(list(json!({"text": "ENGINE"})), ["repo-4"]);
    assert_eq!(list(json!({"text": "acme/site"})), ["repo-5"]);
    // LIKE wildcards in the text are literal.
    assert!(list(json!({"text": "%"})).is_empty());
    let search = |q: &str, languages: Value| {
        page_ids(fx.query(
            &user,
            "repositories.search",
            json!({"scope": {"kind": "Workspace"}, "query": q, "languageIds": languages,
                "page": {"limit": 50}}),
        ))
        .0
    };
    assert_eq!(search("src", json!(["python", "rust"])), ["repo-4"]);
    assert_eq!(
        search("src", json!(["rust", "typescript"])),
        Vec::<String>::new()
    );
    assert_eq!(search("acme", json!([])), ["repo-4", "repo-5"]);
    // Case folding is the same Unicode folding on the query and on every stored value (SQLite's
    // own lower() and LIKE fold only ASCII).
    fx.register("repo-6", "/Users/me/src/Äpp-tool", None, &["Rust"]);
    let r6 = fx.read(|s| s.projects().repository(&id("repo-6"))).unwrap();
    assert_eq!(r6.name, "repo-6");
    for q in ["äpp", "ÄPP", "äPp-TOOL"] {
        assert_eq!(list(json!({"text": q})), ["repo-6"], "{q}");
        assert_eq!(search(q, json!(["RUST"])), ["repo-6"], "{q}");
    }
    // A pathless remote keeps no query in its authority: the token is neither stored nor shown.
    let pathless = StoredRepository {
        id: id("repo-7"),
        location: "/Users/me/src/pathless".into(),
        name: "pathless".into(),
        remote_url: Some("https://git.example.com?access_token=secret".into()),
        primary: true,
        lifecycle: "Active".into(),
        languages: Vec::new(),
        revision: 1,
        registered_at_ms: T0,
    };
    assert!(matches!(
        run_write(&*fx.store, WriteOptions::default(), move |tx| tx
            .projects()
            .insert_repository(&pathless)),
        Err(StorageError::InvalidRecord(_))
    ));
    fx.store
        .with_writer_connection(|c| {
            super::super::conn::db(c.execute_batch(
                "INSERT INTO repositories VALUES('repo-8','/Users/me/src/legacy-pathless',1);
                 INSERT INTO repository_details VALUES('repo-8','pathless',
                   'https://git.example.com?access_token=secret',1,'Active','[]',1);",
            ))
        })
        .unwrap();
    assert_eq!(
        fx.repo(&user, "repo-8").unwrap().remote_url.unwrap().0,
        "https://git.example.com"
    );
    // The stored secret cannot be probed through the search text either.
    assert!(list(json!({"text": "access_token"})).is_empty());
    // Pages continue with a cursor bound to the query and the principal.
    let first = page_ids(fx.query(
        &user,
        "repositories.list",
        json!({"scope": {"kind": "Workspace"}, "page": {"limit": 2}}),
    ));
    assert_eq!(first.0, ["repo-1", "repo-2"]);
    let cursor = first.1.expect("more");
    let second = page_ids(fx.query(
        &user,
        "repositories.list",
        json!({"scope": {"kind": "Workspace"}, "page": {"limit": 2, "cursor": cursor}}),
    ));
    assert_eq!(second.0, ["repo-3", "repo-4"]);
    assert_eq!(
        query_fault(fx.query(
            &user,
            "repositories.list",
            json!({"scope": {"kind": "Workspace"}, "text": "src",
                "page": {"limit": 2, "cursor": cursor}}),
        )),
        (DomainErrorCode::CursorExpired, "repository".into())
    );
    // Credentials, queries and SSH user names never leave the store.
    let engine = fx.repo(&user, "repo-4").unwrap();
    assert_eq!(
        engine.remote_url.unwrap().0,
        "https://github.com/acme/engine.git"
    );
    let site = fx.repo(&user, "repo-5").unwrap();
    assert_eq!(
        site.remote_url.unwrap().0,
        "https://github.com/acme/site.git"
    );
    let text = serde_json::to_string(&fx.query(
        &user,
        "repositories.list",
        json!({"scope": {"kind": "Workspace"}, "page": {"limit": 50}}),
    ))
    .unwrap();
    assert!(!text.contains("ghp_TOKEN") && !text.contains("me:"));
    // Projects page by creation with a cursor too.
    let p1 = fx.create("One", &[]);
    fx.h.clock.0.fetch_add(5, Ordering::SeqCst);
    let p2 = fx.create("Two", &[]);
    let one = page_ids(fx.query(
        &user,
        "projects.list",
        json!({"scope": {"kind": "Workspace"}, "page": {"limit": 1}}),
    ));
    assert_eq!(one.0, [p1.id.0.as_str()]);
    let two = page_ids(fx.query(
        &user,
        "projects.list",
        json!({"scope": {"kind": "Workspace"}, "page": {"limit": 1, "cursor": one.1.unwrap()}}),
    ));
    assert_eq!(two.0, [p2.id.0.as_str()]);
    assert!(two.1.is_none());
}

#[test]
fn a_membership_change_between_pages_expires_a_project_scoped_cursor() {
    let fx = Fx::new();
    let p = fx.create("A", &["repo-1", "repo-2"]);
    let user = fx.user();
    let scoped = json!({"kind": "Project", "projectId": p.id.0.as_str()});
    let page = |op: &str, cursor: Option<&str>| {
        let mut body = json!({"scope": scoped, "page": {"limit": 1}});
        if op == "repositories.search" {
            body["query"] = json!("repo");
            body["languageIds"] = json!([]);
        }
        if let Some(c) = cursor {
            body["page"]["cursor"] = json!(c);
        }
        fx.query(&user, op, body)
    };
    for op in ["repositories.list", "repositories.search"] {
        let first = page(op, None);
        let DomainQueryReply::Result { stamp, .. } = &first else {
            panic!("{first:?}")
        };
        let at = stamp.scope_revision.get();
        let (ids, cursor) = page_ids(first);
        assert_eq!(ids, ["repo-1"]);
        let cursor = cursor.expect("more");
        // The same boundary continues ...
        assert_eq!(page_ids(page(op, Some(&cursor))).0, ["repo-2"]);
        // ... a changed one expires the cursor rather than continuing another list.
        let current = fx.get(&user, &p.id).unwrap();
        // Add a member the first time, take it away the second.
        let repos: Vec<&str> = if members(&current).len() == 2 {
            vec!["repo-1", "repo-2", "repo-3"]
        } else {
            vec!["repo-1", "repo-2"]
        };
        let _ = applied(fx.command(
            &user,
            "projects.membership",
            json!({"projectId": p.id.0.as_str(), "expectedRevision": current.revision.as_str(),
                "repositoryIds": repos}),
        ));
        assert_eq!(
            query_fault(page(op, Some(&cursor))),
            (DomainErrorCode::CursorExpired, "repository".into()),
            "{op}"
        );
        let DomainQueryReply::Result { stamp, .. } = page(op, None) else {
            panic!()
        };
        assert!(
            stamp.scope_revision.get() > at,
            "{op}: stamped with the scope revision"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Migration 14

#[test]
fn migration_14_is_additive_and_closes_older_writers_out() {
    let m = MIGRATIONS
        .iter()
        .find(|m| m.version == 14)
        .expect("migration 14");
    assert_eq!(
        m.checksum(),
        MIGRATION_0014_SHA256,
        "an applied migration was edited"
    );
    assert_eq!((m.minimum_reader, m.minimum_writer), (8, 14));
    assert!(
        MIGRATIONS
            .iter()
            .filter(|e| e.version < 14)
            .all(|e| e.minimum_reader <= m.minimum_reader && e.minimum_writer <= m.minimum_writer)
    );
    assert!(!m.rebuilds_tables);
    let h = Harness::new();
    let old = SqliteStore::open_with(h.config(), &through(13)).unwrap();
    // A project and a repository recorded before migration 14 (positional migration 1 rows).
    old.with_writer_connection(|c| {
        super::super::conn::db(c.execute_batch(
            "INSERT INTO projects VALUES('project-old','Old','Active',3,2,5);
             INSERT INTO repositories VALUES('repo-old','/Users/me/src/legacy/',1);
             INSERT INTO project_repositories VALUES('project-old','repo-old');",
        ))
    })
    .unwrap();
    drop(old);
    let s = SqliteStore::open_with(h.config(), &through(14)).unwrap();
    let report = s.compatibility().unwrap();
    assert_eq!(
        (
            report.store_schema_version,
            report.minimum_reader,
            report.minimum_writer
        ),
        (14, 8, 14)
    );
    // Rows without side rows read with the documented defaults.
    let s: Arc<dyn ApplicationStore> = Arc::new(s);
    let (p, r) = read_snapshot(&*s, &ReadOptions::default(), |snap| {
        Ok((
            snap.projects().project(&project("project-old"))?.unwrap(),
            snap.projects().repository(&id("repo-old"))?.unwrap(),
        ))
    })
    .unwrap();
    assert_eq!(
        (p.description.as_str(), p.revision, p.scope_revision),
        ("", 3, 2)
    );
    assert_eq!(p.updated_at_ms, 5);
    assert_eq!(
        (r.name.as_str(), r.primary, r.lifecycle.as_str()),
        ("legacy", true, "Active")
    );
    assert!(r.languages.is_empty());
    // The store refuses records that break its invariants.
    let bad = StoredProject {
        lifecycle: "Gone".into(),
        ..p.clone()
    };
    assert!(
        run_write(&*s, WriteOptions::default(), move |tx| tx
            .projects()
            .update_project(&bad, 3))
        .is_err()
    );
    let stale = StoredProject { revision: 3, ..p };
    assert!(matches!(
        run_write(&*s, WriteOptions::default(), move |tx| tx
            .projects()
            .update_project(&stale, 2)),
        Err(StorageError::RevisionConflict(_))
    ));
}

// Receipt-only fixture for the DW83 terminal wire/storage path. Full project purge has
// separate ownership/byte-effect acceptance; this deliberately replaces only the receipt.
fn compact_receipt_for_test(
    fx: &Fx,
    ctx: &InvocationContext,
    key: &str,
) -> devworks_core::protocol::records::PurgedReceiptOutcome {
    use devworks_core::protocol::records::{PurgedDecisionKind, PurgedReceiptOutcome};
    let scope = devworks_core::context::ScopeKey::of(ctx);
    let (operation,digest,result):(String,String,String)=fx.h.raw().query_row("SELECT command_kind,payload_digest,result_json FROM command_receipts WHERE actor_id=?1 AND scope_key=?2 AND idempotency_key=?3",rusqlite::params![ctx.principal().0.as_str(),scope.as_str(),key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    let body: Value = serde_json::from_str(&result).unwrap();
    let stored = devworks_core::ports::StoredReceipt {
        operation,
        contract: body["contract"].as_str().unwrap().into(),
        schema_revision: devworks_core::registry::SchemaRevision(
            body["schemaRevision"].as_u64().unwrap().try_into().unwrap(),
        ),
        fingerprint: devworks_core::fingerprint::Fingerprint::from_hex(&digest).unwrap(),
        receipt: serde_json::from_value(body["receipt"].clone()).unwrap(),
    };
    let (original_decision, original_fault_code) = match &stored.receipt.decision {
        DomainDecision::Applied { .. } => (PurgedDecisionKind::Applied, None),
        DomainDecision::NoChange { .. } => (PurgedDecisionKind::NoChange, None),
        DomainDecision::Accepted { .. } => (PurgedDecisionKind::Accepted, None),
        DomainDecision::Refused { fault } => (PurgedDecisionKind::Refused, Some(fault.code)),
    };
    let outcome = PurgedReceiptOutcome {
        command_id: stored.receipt.command_id,
        committed_sequence: stored.receipt.committed_sequence,
        accepted_at_ms: stored.receipt.accepted_at_ms,
        request_digest: devworks_core::protocol::provider::Digest::try_from(
            stored.fingerprint.as_str().to_owned(),
        )
        .unwrap(),
        operation_id: stored.receipt.operation_id,
        original_decision,
        original_fault_code,
        purge_id: id("purge-receipt-fixture"),
    };
    let body = json!({"contract":stored.contract,"schemaRevision":stored.schema_revision.0,"purged":outcome});
    assert!(!body.to_string().contains("PRIVATE_RECEIPT_CONTENT"));
    assert_eq!(fx.h.raw().execute("UPDATE command_receipts SET result_json=?1 WHERE actor_id=?2 AND scope_key=?3 AND idempotency_key=?4",rusqlite::params![body.to_string(),ctx.principal().0.as_str(),scope.as_str(),key]).unwrap(),1);
    outcome
}
fn receipt_lookup_at(
    fx: &Fx,
    ctx: &InvocationContext,
    key: &str,
    revision: u32,
) -> DomainQueryReply {
    let authority = fx.authority();
    let gateway = Gateway {
        authority: &authority,
        store: &*fx.store,
        ids: &*fx.h.ids,
    };
    let frame=json!({"protocol":"1.0-draft.1","schemaRevision":revision,"queryId":"purged-receipt-lookup","query":{"kind":"history.command_receipt","requestKey":key}}).to_string();
    gateway
        .query(
            ctx,
            "history.command_receipt",
            frame.as_bytes(),
            &devworks_core::services::history::HistoryService::new(fx.h.ids.clone()),
        )
        .unwrap()
}
#[test]
fn purged_receipt_keeps_legacy_replay_fences_and_current_observation_after_restart() {
    let fx = Fx::new();
    let user = fx.user();
    let key = "purged-legacy-create";
    let body = json!({"name":"PRIVATE_RECEIPT_CONTENT","description":"private original description","repositoryIds":["repo-1"]});
    let first = fx.command_keyed(&user, "projects.create", key, body.clone());
    let CommandReply::Recorded { receipt, .. } = first else {
        panic!("original receipt")
    };
    let outcome = compact_receipt_for_test(&fx, &user, key);
    assert_eq!(outcome.command_id, receipt.command_id);
    let Fx {
        h,
        store,
        runs,
        cases,
        n,
    } = fx;
    drop(store);
    let store = Arc::new(h.store());
    let fx = Fx {
        h,
        store,
        runs,
        cases,
        n,
    };
    let user = fx.user();
    assert!(matches!(
        fx.command_keyed(&user, "projects.create", key, body),
        CommandReply::Rejected {
            code: Rejection::UnsupportedSchema,
            retry: devworks_core::protocol::RetryAdvice::ResolveReceipt,
            ..
        }
    ));
    assert!(matches!(
        fx.command_keyed(
            &user,
            "projects.create",
            key,
            json!({"name":"another intent","description":"","repositoryIds":[]})
        ),
        CommandReply::Rejected {
            code: Rejection::IdempotencyConflict,
            ..
        }
    ));
    assert_eq!(
        fx.events("ProjectCreated").len(),
        1,
        "a purged result never reexecutes its original command"
    );
    let DomainQueryReply::Result {
        result: OperationResult::HistoryCommandReceipt { value },
        ..
    } = receipt_lookup_at(&fx, &user, key, 6)
    else {
        panic!("current purged observation")
    };
    assert!(value.reply_artifact.is_none() && value.reply_schema.is_none());
    assert_eq!(value.command_id, Some(receipt.command_id));
    assert_eq!(value.committed_sequence, Some(receipt.committed_sequence));
    assert_eq!(value.purged.unwrap().request_digest, outcome.request_digest);
    assert!(
        matches!(receipt_lookup_at(&fx,&user,key,1),DomainQueryReply::Rejected{fault,..} if fault.code==DomainErrorCode::ViewUnavailable && fault.target_kind.as_ref().is_some_and(|k|k.0=="receipt_content_purged"))
    );
    assert!(
        matches!(receipt_lookup_at(&fx,&user,"never-recorded-key",6),DomainQueryReply::Result{result:OperationResult::HistoryCommandReceipt{value},..} if matches!(value.state,devworks_core::protocol::records::ReceiptLookupState::NotFound) && value.purged.is_none())
    );
}
#[test]
fn purged_receipt_never_bypasses_current_principal_revocation() {
    let fx = Fx::new();
    let p = fx.create("Purge principal fixture", &[]);
    fx.bind_agents(&p.id);
    let principal = fx.project_agent(&p.id);
    let key = "purged-agent-edit";
    let body = json!({"projectId":p.id,"expectedRevision":"1","changes":{"name":"PRIVATE_RECEIPT_CONTENT"}});
    let reply = fx.command_keyed(&principal, "projects.edit", key, body.clone());
    assert!(
        matches!(reply,CommandReply::Recorded{receipt,..} if matches!(receipt.decision,DomainDecision::Applied{..})),
        "original edit"
    );
    compact_receipt_for_test(&fx, &principal, key);
    let user = fx.user();
    fx.write(move |tx| {
        auth::revoke_principal(tx, &user, &actor("agent-p"), 1)
            .map(|_| ())
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    });
    assert!(matches!(
        fx.command_keyed(&principal, "projects.edit", key, body),
        CommandReply::Rejected {
            code: Rejection::PermissionDenied,
            ..
        }
    ));
    assert!(
        matches!(receipt_lookup_at(&fx,&principal,key,6),DomainQueryReply::Rejected{fault,..} if fault.code==DomainErrorCode::PermissionDenied)
    );
}

#[test]
fn purged_receipt_revision6_preserves_an_original_refusal_as_a_terminal_outcome() {
    struct RefusalProbe(std::sync::atomic::AtomicUsize);
    impl devworks_core::ports::CommandHandler for RefusalProbe {
        fn decide(
            &self,
            _ctx: &InvocationContext,
            _command: &devworks_core::gateway::AdmittedCommand,
            _tx: &mut dyn WriteTransaction,
        ) -> Result<devworks_core::ports::Decided, devworks_core::ports::HandlerFailure> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(devworks_core::ports::Decided { decision:DomainDecision::Refused {fault:serde_json::from_value(json!({"code":"Conflict","retry":"Never","correlationId":"refusal-probe","blockerIds":[]})).unwrap()},operation_id:None })
        }
    }
    let fx = Fx::new();
    let p = fx.create("Active project", &[]);
    let user = fx.user();
    let key = "purged-current-refusal";
    // The explicit probe records a real revision6 refusal without claiming the purge
    // use case is already implemented. Its decision must never run a second time.
    let mut frame: Value = serde_json::from_str(&Fx::frame(
        "projects.prepare_purge",
        key,
        json!({"projectId":p.id,"expectedRevision":p.revision}),
    ))
    .unwrap();
    frame["schemaRevision"] = json!(6);
    let raw = frame.to_string();
    let authority = fx.authority();
    let gateway = Gateway {
        authority: &authority,
        store: &*fx.store,
        ids: &*fx.h.ids,
    };
    let probe = Arc::new(RefusalProbe(std::sync::atomic::AtomicUsize::new(0)));
    let first = gateway
        .command(
            &user,
            "projects.prepare_purge",
            raw.as_bytes(),
            probe.clone(),
        )
        .unwrap();
    assert!(
        matches!(&first,CommandReply::Recorded{receipt,..} if matches!(receipt.decision,DomainDecision::Refused{..})),
        "original refusal: {first:?}"
    );
    let original = compact_receipt_for_test(&fx, &user, key);
    let replay = gateway
        .command(
            &user,
            "projects.prepare_purge",
            raw.as_bytes(),
            probe.clone(),
        )
        .unwrap();
    assert_eq!(
        probe.0.load(Ordering::SeqCst),
        1,
        "a terminal purged outcome never reaches the handler again"
    );
    assert!(
        matches!(replay,CommandReply::ReceiptPurged{outcome,..} if outcome.command_id==original.command_id && matches!(outcome.original_decision,devworks_core::protocol::records::PurgedDecisionKind::Refused) && outcome.original_fault_code==original.original_fault_code)
    );
    assert_eq!(
        fx.get(&user, &p.id).unwrap().revision,
        p.revision,
        "terminal replay does not prepare or mutate the project"
    );
}

fn purge_storage_fixture(store: &SqliteStore) -> StoredProjectPurge {
    let info = store.info();
    run_write(store, WriteOptions::default(), |tx| {
        tx.actors()
            .register(&actor("purge-human"), PrincipalKind::Human, "Human")?;
        for name in ["purge-a", "purge-b"] {
            tx.projects().insert_project(&StoredProject {
                id: project(name),
                name: "private project title".into(),
                description: "private project body".into(),
                lifecycle: DELETED.into(),
                revision: 1,
                scope_revision: 2,
                created_at_ms: T0,
                updated_at_ms: T0,
                archived_at_ms: Some(T0),
                deleted_at_ms: Some(T0),
                archive_epoch: 1,
                archive_stop_pending: false,
            })?;
        }
        Ok(())
    })
    .unwrap();
    let mut prepared=StoredProjectPurge {
        manifest:serde_json::from_value(json!({"id":"purge-manifest","projectId":"purge-a","revision":"1","projectRevision":"1","digest":"a".repeat(64),"phase":"Prepared","recordCount":"2","managedByteCount":"0","keptRecordCount":"1","keptByteCount":"0","pendingEffects":"0","createdAtMs":T0.to_string()})).unwrap(),
        actor_id:actor("purge-human"),store_id:info.store_id,restore_epoch:info.restore_epoch,scope_revision:2,policy_digest:"b".repeat(64),witness_digest:"c".repeat(64),
        entries:vec![serde_json::from_value(json!({"id":"d".repeat(64),"kind":"RecordClass","recordClass":"work_items","disposition":"Remove","recordCount":"2","byteCount":"0","reason":"OwnedProjectRecords"})).unwrap()],rows:vec![]
    };
    prepared.manifest.digest = project_purge_digest(&prepared).try_into().unwrap();
    prepared
}

#[test]
fn purge_manifest_storage_survives_restart_and_requires_its_exact_project() {
    let h = Harness::new();
    let store = h.store();
    let prepared = purge_storage_fixture(&store);
    let expected = prepared.clone();
    run_write(&store, WriteOptions::default(), move |tx| {
        tx.projects().insert_purge_manifest(&prepared)
    })
    .unwrap();
    assert!(
        read_snapshot(&store, &ReadOptions::default(), |r| r
            .projects()
            .purge_manifest(&project("purge-b"), &id("purge-manifest")))
        .unwrap()
        .is_none()
    );
    drop(store);
    let store = h.store();
    let found = read_snapshot(&store, &ReadOptions::default(), |r| {
        r.projects()
            .purge_manifest(&project("purge-a"), &id("purge-manifest"))
    })
    .unwrap()
    .unwrap();
    assert_eq!(
        serde_json::to_value(&found.manifest).unwrap(),
        serde_json::to_value(&expected.manifest).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&found.entries).unwrap(),
        serde_json::to_value(&expected.entries).unwrap()
    );
    assert_eq!(found.policy_digest, expected.policy_digest);
    assert_eq!(found.witness_digest, expected.witness_digest);
    assert_eq!(found.actor_id, expected.actor_id);
    assert_eq!(found.store_id, expected.store_id);
    assert!(
        run_write(&store, WriteOptions::default(), move |tx| tx
            .projects()
            .insert_purge_manifest(&expected))
        .is_err(),
        "a reviewed ID cannot be replaced"
    );
}

#[test]
fn purge_row_witnesses_survive_restart_bind_the_review_and_refuse_duplicates_atomically() {
    let h = Harness::new();
    let store = h.store();
    let mut prepared = purge_storage_fixture(&store);
    let private_key = "private source locator that must never be copied to audit";
    prepared.rows = vec![
        ProjectPurgeRowWitness {
            table: "work_items".into(),
            key_digest: devworks_core::fingerprint::sha256_hex(private_key.as_bytes()),
            row_digest: "e".repeat(64),
            action: ProjectPurgeRowAction::Delete,
            reason: ProjectPurgeRowReason::OwnedProjectRecords,
        },
        ProjectPurgeRowWitness {
            table: "projects".into(),
            key_digest: "f".repeat(64),
            row_digest: "d".repeat(64),
            action: ProjectPurgeRowAction::Scrub,
            reason: ProjectPurgeRowReason::MinimalDeletedIdentity,
        },
    ];
    prepared.manifest.digest = project_purge_digest(&prepared).try_into().unwrap();
    let mut duplicate = prepared.clone();
    duplicate.rows.push(duplicate.rows[0].clone());
    duplicate.manifest.digest = project_purge_digest(&duplicate).try_into().unwrap();
    assert!(
        run_write(&store, WriteOptions::default(), move |tx| tx
            .projects()
            .insert_purge_manifest(&duplicate))
        .is_err()
    );
    assert_eq!(
        h.raw()
            .query_row("SELECT count(*) FROM project_purge_manifests", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let expected = prepared.clone();
    run_write(&store, WriteOptions::default(), move |tx| {
        tx.projects().insert_purge_manifest(&prepared)
    })
    .unwrap();
    let raw = h.raw();
    let text:String=raw.query_row("SELECT group_concat(table_name||key_digest||row_digest||action||reason) FROM project_purge_rows",[],|r|r.get(0)).unwrap();
    assert!(!text.contains(private_key));
    drop(raw);
    drop(store);
    let store = h.store();
    let found = read_snapshot(&store, &ReadOptions::default(), |r| {
        r.projects()
            .purge_manifest(&project("purge-a"), &id("purge-manifest"))
    })
    .unwrap()
    .unwrap();
    let mut expected_rows = expected.rows.clone();
    expected_rows.sort_by(|a, b| (&a.table, &a.key_digest).cmp(&(&b.table, &b.key_digest)));
    assert_eq!(found.rows, expected_rows);
    assert_eq!(found.manifest.digest, expected.manifest.digest);
    h.raw()
        .execute(
            "UPDATE project_purge_rows SET action='Keep' WHERE table_name='work_items'",
            [],
        )
        .unwrap();
    assert!(
        read_snapshot(&store, &ReadOptions::default(), |r| r
            .projects()
            .purge_manifest(&project("purge-a"), &id("purge-manifest")))
        .is_err(),
        "a changed row decision invalidates the reviewed manifest"
    );
}

#[test]
fn purge_candidate_reader_hashes_full_scoped_rows_without_copying_keys_or_other_project_changes() {
    let h = Harness::new();
    let store = h.store();
    let _ = purge_storage_fixture(&store);
    let snapshot = || {
        read_snapshot(&store, &ReadOptions::default(), |r| {
            r.projects().purge_candidates(&project("purge-a"))
        })
        .unwrap()
        .unwrap()
    };
    let original = snapshot();
    assert!(original.rows.iter().any(|row| row.table == "projects"));
    let encoded = serde_json::to_string(&original.rows).unwrap();
    assert!(!encoded.contains("private project title"));
    assert!(!encoded.contains("purge-a"));
    h.raw()
        .execute(
            "UPDATE projects SET title='another project changed' WHERE id='purge-b'",
            [],
        )
        .unwrap();
    assert_eq!(snapshot().digest, original.digest);
    // Detect a full-row change even when no revision counter was advanced.
    h.raw()
        .execute(
            "UPDATE projects SET title='new private project title' WHERE id='purge-a'",
            [],
        )
        .unwrap();
    let changed = snapshot();
    assert_ne!(changed.digest, original.digest);
    assert_eq!(changed.rows.len(), original.rows.len());
    assert_eq!(changed.policy_digest, original.policy_digest);
    assert!(
        read_snapshot(&store, &ReadOptions::default(), |r| r
            .projects()
            .purge_candidates(&project("missing-project")))
        .unwrap()
        .is_none()
    );
}

#[test]
fn purge_candidate_snapshot_tracks_shared_and_moved_content_without_selecting_it_for_deletion() {
    let h = Harness::new();
    let store = h.store();
    let _ = purge_storage_fixture(&store);
    store.with_writer_connection(|c| {
        super::super::conn::db(c.execute_batch(
            "INSERT INTO artifacts(id,title,revision,scope_revision,origin_project_id,current_project_id,scope_kind,owner_actor_id) VALUES
             ('owned-private','private sentinel',1,1,'purge-a','purge-a','Project',NULL),
             ('moved-in','currently owned sentinel',1,1,'purge-b','purge-a','Project',NULL),
             ('shared-out','shared sentinel',1,1,'purge-a','purge-a','Project',NULL),
             ('moved-workspace','independent workspace sentinel',1,1,'purge-a',NULL,'Workspace','purge-human'),
             ('shared-in','other project sentinel',1,1,'purge-b','purge-b','Project',NULL),
             ('unrelated','unrelated sentinel',1,1,'purge-b','purge-b','Project',NULL);
             INSERT INTO artifact_shares VALUES('shared-out','purge-b'),('shared-in','purge-a');
             INSERT INTO knowledge_entries VALUES
             ('knowledge-owned','purge-a','fact','Project',1,'Active','{\"private\":\"sentinel\"}',1),
             ('knowledge-shared','purge-a','fact','Project',1,'Active','{\"shared\":\"sentinel\"}',1),
             ('knowledge-other','purge-b','fact','Project',1,'Active','{}',1);
             INSERT INTO knowledge_shares VALUES('knowledge-shared','purge-b');
             INSERT INTO knowledge_revisions VALUES('knowledge-owned',1,'purge-human','{}'),('knowledge-shared',1,'purge-human','{}');
             INSERT INTO knowledge_tags VALUES('knowledge-owned','owned-private-tag'),('knowledge-shared','shared-tag');
             INSERT INTO knowledge_links VALUES('knowledge-owned','knowledge-shared','supports','{}'),('knowledge-shared','knowledge-other','supports','{}');"
        ))
    }).unwrap();
    let snapshot = || {
        read_snapshot(&store, &ReadOptions::default(), |r| {
            r.projects().purge_candidates(&project("purge-a"))
        })
        .unwrap()
        .unwrap()
    };
    let before = snapshot();
    let count = |table: &str| before.rows.iter().filter(|r| r.table == table).count();
    assert_eq!(count("artifacts"), 5);
    assert_eq!(count("artifact_shares"), 2);
    for edge in before
        .references
        .iter()
        .filter(|e| e.from_table == "artifact_shares" && e.to_table == "artifacts")
    {
        assert!(
            before
                .rows
                .iter()
                .any(|r| r.table == "artifact_shares" && r.key_digest == edge.from_key_digest),
            "composite source keys use the native guard encoding"
        );
        assert!(
            before
                .rows
                .iter()
                .any(|r| r.table == "artifacts" && r.key_digest == edge.to_key_digest)
        );
    }

    assert_eq!(count("knowledge_entries"), 2);
    assert_eq!(count("knowledge_shares"), 1);
    assert!(
        !serde_json::to_string(&before.rows)
            .unwrap()
            .contains("sentinel")
    );
    let planning = || {
        read_snapshot(&store, &ReadOptions::default(), |r| {
            r.projects().purge_planning(&project("purge-a"))
        })
        .unwrap()
        .unwrap()
    };
    let plan = planning();
    assert!(plan.is_resolved(), "{:?}", plan.unresolved);
    for (table, key, action) in [
        (
            "artifacts",
            json!(["owned-private"]),
            ProjectPurgeRowAction::Delete,
        ),
        (
            "artifacts",
            json!(["moved-in"]),
            ProjectPurgeRowAction::Delete,
        ),
        (
            "artifacts",
            json!(["shared-out"]),
            ProjectPurgeRowAction::Keep,
        ),
        (
            "artifacts",
            json!(["moved-workspace"]),
            ProjectPurgeRowAction::Keep,
        ),
        (
            "artifacts",
            json!(["shared-in"]),
            ProjectPurgeRowAction::Keep,
        ),
        (
            "artifact_shares",
            json!(["shared-in", "purge-a"]),
            ProjectPurgeRowAction::Delete,
        ),
        (
            "artifact_shares",
            json!(["shared-out", "purge-b"]),
            ProjectPurgeRowAction::Keep,
        ),
        (
            "knowledge_entries",
            json!(["knowledge-owned"]),
            ProjectPurgeRowAction::Delete,
        ),
        (
            "knowledge_entries",
            json!(["knowledge-shared"]),
            ProjectPurgeRowAction::Keep,
        ),
        (
            "knowledge_revisions",
            json!(["knowledge-owned", 1]),
            ProjectPurgeRowAction::Delete,
        ),
        (
            "knowledge_revisions",
            json!(["knowledge-shared", 1]),
            ProjectPurgeRowAction::Keep,
        ),
        (
            "knowledge_tags",
            json!(["knowledge-owned", "owned-private-tag"]),
            ProjectPurgeRowAction::Delete,
        ),
        (
            "knowledge_tags",
            json!(["knowledge-shared", "shared-tag"]),
            ProjectPurgeRowAction::Keep,
        ),
        (
            "knowledge_links",
            json!(["knowledge-owned", "knowledge-shared", "supports"]),
            ProjectPurgeRowAction::Delete,
        ),
        (
            "knowledge_links",
            json!(["knowledge-shared", "knowledge-other", "supports"]),
            ProjectPurgeRowAction::Keep,
        ),
    ] {
        let digest = devworks_core::fingerprint::sha256_hex(key.to_string().as_bytes());
        assert!(
            plan.rows
                .iter()
                .any(|r| r.table == table && r.key_digest == digest && r.action == action),
            "{table} {key}: {action:?}"
        );
    }
    assert!(
        !serde_json::to_string(&plan.rows)
            .unwrap()
            .contains("owned-private-tag")
    );
    h.raw()
        .execute(
            "UPDATE artifacts SET title='unrelated changed' WHERE id='unrelated'",
            [],
        )
        .unwrap();
    assert_eq!(snapshot().digest, before.digest);
    h.raw()
        .execute(
            "DELETE FROM artifact_shares WHERE artifact_id='shared-out'",
            [],
        )
        .unwrap();
    let changed = planning();
    let shared_key =
        devworks_core::fingerprint::sha256_hex(json!(["shared-out"]).to_string().as_bytes());
    assert!(
        changed.rows.iter().any(|r| r.table == "artifacts"
            && r.key_digest == shared_key
            && r.action == ProjectPurgeRowAction::Delete),
        "removing a keeper changes the exact content decision"
    );
    assert_ne!(
        snapshot().digest,
        before.digest,
        "a changed keeper invalidates the snapshot"
    );
    assert_eq!(
        h.raw()
            .query_row("SELECT count(*) FROM artifacts", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        6
    );
    assert_eq!(
        h.raw()
            .query_row(
                "SELECT title FROM artifacts WHERE id='moved-workspace'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "independent workspace sentinel"
    );
}

#[test]
fn purge_candidate_receipts_include_actual_events_and_explicit_creation_snapshots() {
    let fx = Fx::new();
    let p = fx.create("Receipt project", &[]);
    let other = fx.create("Other receipt project", &[]);
    applied(fx.command(&fx.user(), "projects.edit", json!({"projectId":p.id,"expectedRevision":"1","changes":{"name":"Renamed private project"}})));
    applied(fx.command(
        &fx.user(),
        "projects.edit",
        json!({"projectId":p.id,"expectedRevision":"2","changes":{"name":"Renamed again"}}),
    ));
    applied(fx.command(
        &fx.user(),
        "projects.edit",
        json!({"projectId":other.id,"expectedRevision":"1","changes":{"name":"Other renamed"}}),
    ));
    assert_eq!(fx.h.raw().query_row("SELECT count(*) FROM events WHERE kind='CommandDecided' AND project_id IS NULL AND json_extract(payload_json,'$.operation')='projects.create'", [], |r| r.get::<_,i64>(0)).unwrap(), 2, "workspace creation receipts need typed snapshot redaction, not invented project ownership");
    let snapshot = |p: &ProjectId| {
        read_snapshot(fx.store.as_ref(), &ReadOptions::default(), |r| {
            r.projects().purge_candidates(p)
        })
        .unwrap()
        .unwrap()
    };
    let count = |p: &ProjectId| {
        snapshot(p)
            .rows
            .iter()
            .filter(|r| r.table == "command_receipts")
            .count()
    };
    assert_eq!(count(&p.id), 3);
    assert_eq!(count(&other.id), 2);
    // Every command used the same current Human/store scope. Attribution still
    // follows actual CommandDecided targets plus explicit typed creation snapshots.
    assert_eq!(fx.h.raw().query_row("SELECT count(DISTINCT scope_key) FROM command_receipts WHERE command_kind IN('projects.create','projects.edit')",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    assert_eq!(count(&p.id), 3);
}

#[test]
fn purge_candidate_snapshot_witnesses_independent_references_without_expanding_ownership() {
    let h = Harness::new();
    let store = h.store();
    let _ = purge_storage_fixture(&store);
    store.with_writer_connection(|c| super::super::conn::db(c.execute_batch(
        "INSERT INTO operations(id,capability,schema_version,args_digest,payload_json,state,revision,created_at) VALUES('owned-operation','schemas.index',5,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','{}','Completed',1,0);
         INSERT INTO events(id,actor_id,project_id,operation_id,aggregate_kind,aggregate_id,aggregate_revision,kind,created_at,payload_json) VALUES('owned-command-event','purge-human','purge-a','owned-operation','command','owned-command',1,'CommandDecided',0,'{}');"
    ))).unwrap();
    let snapshot = || {
        read_snapshot(&store, &ReadOptions::default(), |r| {
            r.projects().purge_candidates(&project("purge-a"))
        })
        .unwrap()
        .unwrap()
    };
    let before = snapshot();
    let operation = before
        .rows
        .iter()
        .find(|r| r.table == "operations")
        .unwrap();
    store.with_writer_connection(|c| super::super::conn::db(c.execute(
        "INSERT INTO usage_price_status(provider,revision,refresh_pending,refresh_operation_id) VALUES('independent-provider',1,0,'owned-operation')",[]
    )).map(|_|())).unwrap();
    store.with_writer_connection(|c| super::super::conn::db(c.execute_batch(
        "INSERT INTO events(id,actor_id,aggregate_kind,aggregate_id,aggregate_revision,kind,created_at,payload_json) VALUES('global-package-event','purge-human','package','global-package',1,'GlobalFixture',0,'{}');
         INSERT INTO package_handler_commits(package_id,subscription_id,event_id,receipt_id,payload_digest,state_generation,state_revision,checkpoint,intents_json) VALUES('independent-package','subscription','global-package-event','processing-receipt','aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',1,1,1,'[{\"actionKey\":\"private-action-key\",\"operationId\":\"owned-operation\"}]');
         INSERT INTO artifacts(id,title,revision,scope_revision,scope_kind,owner_actor_id) VALUES('independent-artifact','independent artifact sentinel',1,1,'Workspace','purge-human');
         INSERT INTO blobs(digest,byte_count,relative_path,state,generation) VALUES('cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc',1,'controlled-reference-fixture','Live','fixture');
         INSERT INTO artifact_versions(id,artifact_id,version_no,blob_digest,media_type,created_at,source_json) VALUES('independent-version','independent-artifact',1,'cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc','text/plain',0,'{}');
         INSERT INTO artifact_links VALUES('independent-version','operation','owned-operation','Origin');"
    ))).unwrap();
    let after = snapshot();
    assert_eq!(
        before.rows, after.rows,
        "a reference does not establish ownership of its source row"
    );
    assert_ne!(
        before.digest, after.digest,
        "a new surviving reference invalidates the reviewed identity closure"
    );
    let edge = after
        .references
        .iter()
        .find(|edge| edge.from_table == "usage_price_status")
        .unwrap();
    assert_eq!(edge.kind, ProjectPurgeReferenceKind::ForeignKey);
    assert_eq!(edge.to_table, "operations");
    assert_eq!(edge.to_key_digest, operation.key_digest);
    assert_eq!(edge.from_columns, ["refresh_operation_id"]);
    assert_eq!(
        edge.to_columns,
        ["id"],
        "implicit REFERENCES resolves the actual parent primary key"
    );
    for (kind, table) in [
        (
            ProjectPurgeReferenceKind::PackageIntent,
            "package_handler_commits",
        ),
        (ProjectPurgeReferenceKind::ArtifactSubject, "artifact_links"),
    ] {
        let edge = after.references.iter().find(|e| e.kind == kind).unwrap();
        assert_eq!(edge.from_table, table);
        assert_eq!(edge.to_table, "operations");
        assert_eq!(edge.to_key_digest, operation.key_digest);
    }
    assert!(
        !after.rows.iter().any(|r| matches!(
            r.table.as_str(),
            "package_handler_commits" | "artifact_links" | "artifacts"
        )),
        "typed references cannot claim independently owned content"
    );
    assert_eq!(
        h.raw()
            .query_row(
                "SELECT title FROM artifacts WHERE id='independent-artifact'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "independent artifact sentinel"
    );
    let audit = serde_json::to_string(&after.references).unwrap();
    assert!(!audit.contains("private-action-key"));
    assert!(!audit.contains("independent artifact sentinel"));
    assert!(!audit.contains("independent-provider"));
    assert!(!audit.contains("owned-operation"));
    let plan = read_snapshot(&store, &ReadOptions::default(), |r| {
        r.projects().purge_planning(&project("purge-a"))
    })
    .unwrap()
    .unwrap();
    assert!(plan.rows.iter().any(|r| r.table == "operations"
        && r.key_digest == operation.key_digest
        && r.action == ProjectPurgeRowAction::Scrub
        && r.reason == ProjectPurgeRowReason::MinimalDeletedIdentity));
    assert!(
        !plan.is_resolved(),
        "control-receipt ownership is still an explicit unresolved partition"
    );
    assert_eq!(
        h.raw()
            .query_row(
                "SELECT state FROM operations WHERE id='owned-operation'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "Completed"
    );
    let original_intents=h.raw().query_row("SELECT intents_json FROM package_handler_commits WHERE receipt_id='processing-receipt'",[],|r|r.get::<_,String>(0)).unwrap();
    for unknown in [
        json!({"actionKey":"key","operationId":"owned-operation"}),
        json!([{"actionKey":"key","operationId":"owned-operation","arguments":"unmapped private arguments"}]),
        json!([{"actionKey":"","operationId":"owned-operation"}]),
        json!([{"actionKey":"key","operationId":42}]),
    ] {
        h.raw().execute("UPDATE package_handler_commits SET intents_json=?1 WHERE receipt_id='processing-receipt'",[unknown.to_string()]).unwrap();
        let refused = read_snapshot(&store, &ReadOptions::default(), |r| {
            r.projects().purge_candidates(&project("purge-a"))
        });
        assert!(
            refused.is_err(),
            "unmapped intent formats must hold ownership planning"
        );
        assert_eq!(
            h.raw()
                .query_row(
                    "SELECT payload_json FROM operations WHERE id='owned-operation'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "{}"
        );
    }
    h.raw().execute("UPDATE package_handler_commits SET intents_json=?1 WHERE receipt_id='processing-receipt'",[original_intents]).unwrap();
    assert_eq!(snapshot().digest, after.digest);
    assert_eq!(h.raw().query_row("SELECT refresh_operation_id FROM usage_price_status WHERE provider='independent-provider'",[],|r|r.get::<_,String>(0)).unwrap(),"owned-operation");
}

#[test]
fn purge_planning_classifies_records_read_only_and_never_enables_ordinary_delete_guards() {
    let h = Harness::new();
    let store = h.store();
    let _ = purge_storage_fixture(&store);
    h.raw().execute("INSERT INTO allocation_replays(project_id,key,intent_digest,result_json) VALUES('purge-a',?1,?2,'{}')",rusqlite::params!["a".repeat(64),"b".repeat(64)]).unwrap();
    let plan = read_snapshot(&store, &ReadOptions::default(), |r| {
        r.projects().purge_planning(&project("purge-a"))
    })
    .unwrap()
    .unwrap();
    assert!(plan.is_resolved(), "{:?}", plan.unresolved);
    assert_eq!(plan.rows.len(), 3);
    assert!(
        plan.rows
            .iter()
            .filter(|r| matches!(r.table.as_str(), "projects" | "project_details"))
            .all(|r| r.action == ProjectPurgeRowAction::Scrub
                && r.reason == ProjectPurgeRowReason::MinimalDeletedIdentity)
    );
    assert!(
        plan.rows
            .iter()
            .any(|r| r.table == "allocation_replays" && r.action == ProjectPurgeRowAction::Delete)
    );
    assert!(
        h.raw()
            .execute("DELETE FROM allocation_replays", [])
            .is_err(),
        "an assessment is not a native purge permit"
    );
    assert_eq!(
        h.raw()
            .query_row("SELECT count(*) FROM allocation_replays", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        h.raw()
            .query_row("SELECT title FROM projects WHERE id='purge-a'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
        "private project title"
    );
}

#[test]
fn purge_planning_compacts_typed_creation_snapshots_and_holds_unmapped_legacy_mappings() {
    let fx = Fx::new();
    let p = fx.create("Private copied project name", &[]);
    let plan = read_snapshot(fx.store.as_ref(), &ReadOptions::default(), |r| {
        r.projects().purge_planning(&p.id)
    })
    .unwrap()
    .unwrap();
    assert!(plan.rows.iter().any(|r| r.table == "command_receipts"
        && r.action == ProjectPurgeRowAction::Scrub
        && r.reason == ProjectPurgeRowReason::ReceiptReplayFence));
    assert!(
        !plan
            .unresolved
            .iter()
            .any(|r| r.table == "command_receipts"),
        "{:?}",
        plan.unresolved
    );
    let audit = serde_json::to_string(&plan.unresolved).unwrap();
    assert!(!audit.contains("Private copied project name"));
    assert_eq!(
        fx.get(&fx.user(), &p.id).unwrap().name.0,
        "Private copied project name"
    );
    let original=fx.h.raw().query_row("SELECT result_json FROM command_receipts WHERE command_kind='projects.create' AND json_extract(result_json,'$.receipt.decision.result.value.id')=?1",[p.id.0.as_str()],|r|r.get::<_,String>(0)).unwrap();
    let mut malformed: Value = serde_json::from_str(&original).unwrap();
    malformed["unmappedPrivateBody"] = json!("unrecognized receipt body");
    fx.h.raw().execute("UPDATE command_receipts SET result_json=?1 WHERE command_kind='projects.create' AND json_extract(result_json,'$.receipt.decision.result.value.id')=?2",rusqlite::params![malformed.to_string(),p.id.0.as_str()]).unwrap();
    let refused = read_snapshot(fx.store.as_ref(), &ReadOptions::default(), |r| {
        r.projects().purge_planning(&p.id)
    })
    .unwrap()
    .unwrap();
    assert!(
        refused
            .unresolved
            .iter()
            .any(|r| r.table == "command_receipts")
    );
    assert!(
        !serde_json::to_string(&refused.unresolved)
            .unwrap()
            .contains("unrecognized receipt body")
    );
    fx.h.raw().execute("UPDATE command_receipts SET result_json=?1 WHERE command_kind='projects.create' AND json_extract(result_json,'$.receipt.decision.result.value.id')=?2",rusqlite::params![original,p.id.0.as_str()]).unwrap();
    fx.h.raw().execute("INSERT INTO import_mappings VALUES('legacy-import','unknown-kind','private-source-key','opaque-local-id','digest')",[]).unwrap();
    let plan = read_snapshot(fx.store.as_ref(), &ReadOptions::default(), |r| {
        r.projects().purge_planning(&p.id)
    })
    .unwrap()
    .unwrap();
    assert!(
        plan.unresolved.iter().any(|r| r.table == "import_mappings"
            && r.reason == ProjectPurgeUnresolvedReason::LegacyMapping)
    );
    assert!(
        !serde_json::to_string(&plan.unresolved)
            .unwrap()
            .contains("private-source-key")
    );
}

#[test]
fn purge_planning_business_digest_excludes_only_validated_minimal_control_audit() {
    struct PrepareProbe(devworks_core::protocol::records::ProjectPurgeManifest);
    impl devworks_core::ports::CommandHandler for PrepareProbe {
        fn decide(
            &self,
            _: &InvocationContext,
            _: &devworks_core::gateway::AdmittedCommand,
            _: &mut dyn WriteTransaction,
        ) -> Result<devworks_core::ports::Decided, devworks_core::ports::HandlerFailure> {
            Ok(devworks_core::ports::Decided {
                decision: DomainDecision::Applied {
                    result: OperationResult::ProjectsPreparePurge {
                        value: self.0.clone(),
                    },
                },
                operation_id: None,
            })
        }
    }
    let fx = Fx::new();
    let p = fx.create("private control digest project", &[]);
    let assess = || {
        read_snapshot(fx.store.as_ref(), &ReadOptions::default(), |r| {
            r.projects().purge_planning(&p.id)
        })
        .unwrap()
        .unwrap()
    };
    let before = assess();
    let manifest=serde_json::from_value(json!({"id":"control-manifest","projectId":p.id,"revision":"1","projectRevision":p.revision,"digest":"a".repeat(64),"phase":"Prepared","recordCount":"2","managedByteCount":"0","keptRecordCount":"1","keptByteCount":"0","pendingEffects":"0","createdAtMs":T0.to_string()})).unwrap();
    let mut frame: Value = serde_json::from_str(&Fx::frame(
        "projects.prepare_purge",
        "control-digest-preview",
        json!({"projectId":p.id,"expectedRevision":p.revision}),
    ))
    .unwrap();
    frame["schemaRevision"] = json!(6);
    let authority = fx.authority();
    let gateway = Gateway {
        authority: &authority,
        store: fx.store.as_ref(),
        ids: fx.h.ids.as_ref(),
    };
    let reply = gateway
        .command(
            &fx.user(),
            "projects.prepare_purge",
            frame.to_string().as_bytes(),
            Arc::new(PrepareProbe(manifest)),
        )
        .unwrap();
    assert!(
        matches!(reply,CommandReply::Recorded{receipt,..} if matches!(receipt.decision,DomainDecision::Applied{..}))
    );
    let after = assess();
    assert_ne!(
        before.snapshot.digest, after.snapshot.digest,
        "the gateway really appended its control audit"
    );
    assert_eq!(
        before.digest, after.digest,
        "minimal validated control publication cannot stale its own business review"
    );
    assert_eq!(
        after
            .rows
            .iter()
            .filter(|r| r.reason == ProjectPurgeRowReason::PurgeControlAudit)
            .count(),
        3
    );
    let event=fx.h.raw().query_row("SELECT seq,payload_json FROM events WHERE kind='CommandDecided' AND json_extract(payload_json,'$.operation')='projects.prepare_purge'",[],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?))).unwrap();
    let mut payload: Value = serde_json::from_str(&event.1).unwrap();
    payload["subject"] = json!({"privateBody":"unexpected copied content"});
    fx.h.raw()
        .execute(
            "UPDATE events SET payload_json=?1 WHERE seq=?2",
            rusqlite::params![payload.to_string(), event.0],
        )
        .unwrap();
    let changed = assess();
    assert_ne!(
        before.digest, changed.digest,
        "unknown control content remains business content"
    );
    assert!(
        !changed
            .rows
            .iter()
            .any(|r| r.reason == ProjectPurgeRowReason::PurgeControlAudit)
    );
    assert_eq!(
        fx.get(&fx.user(), &p.id).unwrap().name.0,
        "private control digest project"
    );
}

#[test]
fn purge_manifest_storage_refuses_stale_store_project_and_entry_fences_atomically() {
    let h = Harness::new();
    let store = h.store();
    let prepared = purge_storage_fixture(&store);
    for mode in 0..7 {
        let mut invalid = prepared.clone();
        match mode {
            0 => invalid.restore_epoch += 1,
            1 => invalid.scope_revision += 1,
            2 => invalid.manifest.project_revision = Revision::new(2).unwrap(),
            3 => invalid.entries.push(invalid.entries[0].clone()),
            4 => invalid.policy_digest = "invalid".into(),
            5 => invalid.store_id = devworks_core::protocol::StoreId(id("another-store")),
            _ => {}
        }
        invalid.manifest.digest = project_purge_digest(&invalid).try_into().unwrap();
        if mode == 6 {
            invalid.manifest.digest = "e".repeat(64).try_into().unwrap();
        }
        assert!(
            run_write(&store, WriteOptions::default(), move |tx| tx
                .projects()
                .insert_purge_manifest(&invalid))
            .is_err(),
            "mode {mode}"
        );
        assert!(
            read_snapshot(&store, &ReadOptions::default(), |r| r
                .projects()
                .purge_manifest(&project("purge-a"), &id("purge-manifest")))
            .unwrap()
            .is_none()
        );
    }
    run_write(&store, WriteOptions::default(), move |tx| {
        tx.projects().insert_purge_manifest(&prepared)
    })
    .unwrap();
    let migration = MIGRATIONS.iter().find(|m| m.version == 36).unwrap();
    assert_eq!(
        (migration.minimum_reader, migration.minimum_writer),
        (36, 36)
    );
    assert!(!migration.rebuilds_tables);
}

#[test]
fn purge_migration36_refuses_an_older_reader_before_changing_the_store() {
    let h = Harness::new();
    drop(h.store());
    let before = super::file_digest(&h);
    let boot_count = super::boots(&h);
    match SqliteStore::open_with(h.config(), &through(35)) {
        Err(StorageError::IncompatibleSchema(message)) => {
            assert!(message.contains("reader version 36"))
        }
        _ => panic!("an older reader must not interpret compact purge identities"),
    }
    assert_eq!(super::file_digest(&h), before);
    assert_eq!(super::boots(&h), boot_count);
    let current = h.store();
    assert!(current.integrity_check().unwrap().problems.is_empty());
}

fn managed_reservation(store: &SqliteStore, name: &str) -> ProjectManagedFileReservation {
    let info = store.info();
    ProjectManagedFileReservation {
        id: id(name),
        project_id: project("purge-a"),
        kind: "DiagnosticExport".into(),
        relative_path: format!("{name}.jsonl"),
        root: devworks_core::authority::destructive::FileIdentity { volume: 1, file: 2 },
        store_id: info.store_id,
        restore_epoch: info.restore_epoch,
        boot_id: info.boot_id,
        created_at_ms: T0,
    }
}
fn managed_publication(r: &ProjectManagedFileReservation) -> StoredProjectManagedFile {
    StoredProjectManagedFile {
        id: r.id.clone(),
        project_id: r.project_id.clone(),
        kind: r.kind.clone(),
        relative_path: r.relative_path.clone(),
        created_at_ms: r.created_at_ms,
        witness: devworks_core::ports::project_purge::PurgeFileWitness {
            project_id: r.project_id.clone(),
            store_id: r.store_id.clone(),
            restore_epoch: r.restore_epoch,
            owner: devworks_core::ports::project_purge::PurgeFileOwner::DiagnosticExport {
                file_id: r.id.clone(),
            },
            root: r.root,
            relative_entry: r.relative_path.clone(),
            identity: devworks_core::authority::destructive::FileIdentity { volume: 1, file: 3 },
            byte_count: 3,
            content_digest: "a".repeat(64),
            directory: false,
            link: false,
        },
    }
}
#[test]
fn managed_purge_file_reservations_survive_restart_block_preview_and_publish_only_the_reserved_identity()
 {
    let h = Harness::new();
    let store = h.store();
    let prepared = purge_storage_fixture(&store);
    let reservation = managed_reservation(&store, "managed-export");
    let saved = reservation.clone();
    run_write(&store, WriteOptions::default(), move |tx| {
        tx.projects().reserve_project_managed_file(&saved)
    })
    .unwrap();
    assert!(
        read_snapshot(&store, &ReadOptions::default(), |r| r
            .projects()
            .pending_project_managed_writes(&project("purge-a")))
        .unwrap()
    );
    assert!(matches!(
        run_write(&store, WriteOptions::default(), move |tx| tx
            .projects()
            .insert_purge_manifest(&prepared)),
        Err(StorageError::Busy)
    ));
    drop(store);
    let store = h.store();
    assert!(
        read_snapshot(&store, &ReadOptions::default(), |r| r
            .projects()
            .pending_project_managed_writes(&project("purge-a")))
        .unwrap()
    );
    let old = managed_publication(&reservation);
    assert!(
        matches!(
            run_write(&store, WriteOptions::default(), move |tx| tx
                .projects()
                .register_project_managed_file(&old)),
            Err(StorageError::ScopeDenied)
        ),
        "a restarted host cannot publish a previous boot's unfinished write"
    );
    run_write(&store, WriteOptions::default(), move |tx| {
        tx.projects()
            .finish_project_managed_file(&project("purge-a"), &reservation.id)
    })
    .unwrap();
    let reservation = managed_reservation(&store, "managed-recovered-export");
    let saved = reservation.clone();
    run_write(&store, WriteOptions::default(), move |tx| {
        tx.projects().reserve_project_managed_file(&saved)
    })
    .unwrap();
    let published = managed_publication(&reservation);
    let mut wrong = published.clone();
    wrong.witness.root.file += 1;
    assert!(
        run_write(&store, WriteOptions::default(), move |tx| tx
            .projects()
            .register_project_managed_file(&wrong))
        .is_err()
    );
    let saved = published.clone();
    run_write(&store, WriteOptions::default(), move |tx| {
        tx.projects().register_project_managed_file(&saved)
    })
    .unwrap();
    assert!(
        !read_snapshot(&store, &ReadOptions::default(), |r| r
            .projects()
            .pending_project_managed_writes(&project("purge-a")))
        .unwrap()
    );
    assert!(
        read_snapshot(&store, &ReadOptions::default(), |r| r
            .projects()
            .project_managed_file(
                &project("purge-b"),
                &id("managed-recovered-export")
            ))
        .unwrap()
        .is_none()
    );
    let found = read_snapshot(&store, &ReadOptions::default(), |r| {
        r.projects()
            .project_managed_file(&project("purge-a"), &id("managed-recovered-export"))
    })
    .unwrap()
    .unwrap();
    assert_eq!(found.witness, published.witness);
    run_write(&store, WriteOptions::default(), |tx| {
        tx.projects()
            .finish_project_managed_file(&project("purge-a"), &id("managed-recovered-export"))
    })
    .unwrap();
    let saved = published;
    assert!(
        run_write(&store, WriteOptions::default(), move |tx| tx
            .projects()
            .register_project_managed_file(&saved))
        .is_err(),
        "retired file identities never revive"
    );
    assert!(
        read_snapshot(&store, &ReadOptions::default(), |r| r
            .projects()
            .project_managed_files(&project("purge-a"), None, 10))
        .unwrap()
        .is_empty()
    );
    let raw = h.raw();
    let (path,witness):(String,String)=raw.query_row("SELECT relative_path,identity_json FROM project_managed_files WHERE id='managed-export'",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert!(path.is_empty());
    assert_eq!(witness, "{}");
}

#[test]
fn project_preview_writes_refuse_a_logically_purged_project_and_leave_workspace_cache_untouched() {
    use devworks_core::ports::files::ArtifactFiles;
    let h = Harness::new();
    let store = Arc::new(h.store());
    let prepared = purge_storage_fixture(store.as_ref());
    let files =
        crate::fs::artifacts::NativeArtifactFiles::new(h.root.0.clone(), h.root.0.join("cache"))
            .with_store(store.clone());
    let key = devworks_core::services::artifacts::preview::preview_id("before-purge");
    files
        .write_project_derived(&project("purge-a"), &key, b"private preview")
        .unwrap();
    assert_eq!(
        files.read_derived(&key, 100).unwrap().unwrap(),
        b"private preview"
    );
    assert!(
        !read_snapshot(store.as_ref(), &ReadOptions::default(), |r| r
            .projects()
            .pending_project_managed_writes(&project("purge-a")))
        .unwrap()
    );
    let mut manifest = prepared.manifest.clone();
    run_write(store.as_ref(), WriteOptions::default(), move |tx| {
        tx.projects().insert_purge_manifest(&prepared)?;
        tx.operations().record_intent(NewOperation {
            id: devworks_core::protocol::OperationId(id("purge-preview-fence")),
            root_id: None,
            assignment_id: None,
            capability: "projects.purge".into(),
            schema_version: 6,
            args_digest: "a".repeat(64),
            payload: json!({}),
        })?;
        Ok(())
    })
    .unwrap();
    manifest.phase = devworks_core::protocol::records::PurgePhase::RemovingBytes;
    manifest.revision = Revision::new(2).unwrap();
    manifest.operation_id = Some(devworks_core::protocol::OperationId(id(
        "purge-preview-fence",
    )));
    manifest.pending_effects = Counter::new(1).unwrap();
    h.raw().execute("UPDATE project_purge_manifests SET phase='RemovingBytes',revision=2,operation_id='purge-preview-fence',body_json=?1 WHERE id='purge-manifest'",[serde_json::to_string(&manifest).unwrap()]).unwrap();
    let late = devworks_core::services::artifacts::preview::preview_id("late-purge");
    assert!(
        files
            .write_project_derived(&project("purge-a"), &late, b"late private bytes")
            .is_err()
    );
    assert!(files.read_derived(&late, 100).unwrap().is_none());
    let workspace = devworks_core::services::artifacts::preview::preview_id("workspace-kept");
    files.write_derived(&workspace, b"workspace cache").unwrap();
    assert_eq!(
        files.read_derived(&workspace, 100).unwrap().unwrap(),
        b"workspace cache"
    );
}

#[test]
fn project_preview_recovery_discards_only_the_interrupted_native_key_after_restart() {
    use devworks_core::ports::files::ArtifactFiles;
    let h = Harness::new();
    let store = Arc::new(h.store());
    let _ = purge_storage_fixture(store.as_ref());
    let files =
        crate::fs::artifacts::NativeArtifactFiles::new(h.root.0.clone(), h.root.0.join("cache"))
            .with_store(store.clone());
    let workspace = devworks_core::services::artifacts::preview::preview_id("workspace-kept");
    files.write_derived(&workspace, b"workspace cache").unwrap();
    let key = devworks_core::services::artifacts::preview::preview_id("interrupted-key");
    let dir = h.root.0.join("cache").join("artifact-previews");
    let mut reserved = managed_reservation(store.as_ref(), "interrupted-preview");
    reserved.kind = "Preview".into();
    reserved.relative_path = key.clone();
    reserved.root = crate::fs::identity_at(&dir, &std::fs::metadata(&dir).unwrap()).unwrap();
    run_write(store.as_ref(), WriteOptions::default(), move |tx| {
        tx.projects().reserve_project_managed_file(&reserved)
    })
    .unwrap();
    let temp = dir.join(format!("{key}.123-1.tmp"));
    std::fs::write(&temp, b"interrupted private cache").unwrap();
    drop(files);
    drop(store);
    let store = Arc::new(h.store());
    assert!(
        read_snapshot(store.as_ref(), &ReadOptions::default(), |r| r
            .projects()
            .pending_project_managed_writes(&project("purge-a")))
        .unwrap()
    );
    let files =
        crate::fs::artifacts::NativeArtifactFiles::new(h.root.0.clone(), h.root.0.join("cache"))
            .with_store(store.clone());
    files.recover_project_derived().unwrap();
    assert!(!temp.exists());
    assert!(
        !read_snapshot(store.as_ref(), &ReadOptions::default(), |r| r
            .projects()
            .pending_project_managed_writes(&project("purge-a")))
        .unwrap()
    );
    assert_eq!(
        files.read_derived(&workspace, 100).unwrap().unwrap(),
        b"workspace cache"
    );
}

fn native_managed_file(
    store: &SqliteStore,
    root: &std::path::Path,
    name: &str,
    kind: &str,
    owner: devworks_core::ports::project_purge::PurgeFileOwner,
    binding: &devworks_core::ports::project_purge::PurgeFileSelection,
) {
    std::fs::create_dir_all(root).unwrap();
    let mut reservation = managed_reservation(store, name);
    reservation.kind = kind.into();
    reservation.relative_path = name.into();
    reservation.root = crate::fs::identity_at(root, &std::fs::metadata(root).unwrap()).unwrap();
    let saved = reservation.clone();
    run_write(store, WriteOptions::default(), move |tx| {
        tx.projects().reserve_project_managed_file(&saved)
    })
    .unwrap();
    std::fs::write(root.join(name), b"owned transient bytes").unwrap();
    let witness =
        crate::fs::purge::capture_managed_file(root, binding, name, owner, &mut || false).unwrap();
    let live = StoredProjectManagedFile {
        id: reservation.id,
        project_id: reservation.project_id,
        kind: reservation.kind,
        relative_path: reservation.relative_path,
        created_at_ms: T0,
        witness,
    };
    run_write(store, WriteOptions::default(), move |tx| {
        tx.projects().register_project_managed_file(&live)
    })
    .unwrap();
}

#[test]
fn native_purge_files_cover_all_owned_classes_and_preserve_sources_and_other_generations() {
    use devworks_core::ports::project_purge::{
        ProjectPurgeFiles, PurgeFileOwner as O, PurgeFileSelection,
    };
    let h = Harness::new();
    let store = Arc::new(h.store());
    let _ = purge_storage_fixture(store.as_ref());
    let data = &h.root.0;
    let cache = data.join("cache");
    let paste = data.join("paste");
    let digest = devworks_core::fingerprint::sha256_hex(b"owned blob");
    let version = id("purge-version");
    let key = devworks_core::services::artifacts::preview::preview_id(version.as_str());
    let blob = format!("artifacts/sha256/{}/{}.g1", &digest[..2], digest);
    let independent = format!("artifacts/sha256/{}/{}.g2", &digest[..2], digest);
    let mut kept = vec![
        independent,
        "staging/other.part".into(),
        "quarantine/other.g1.1".into(),
        "browser/sessions/other/profile".into(),
        "source/.git/config".into(),
        "source/main.rs".into(),
    ];
    let owned = [
        blob,
        "staging/purge-op.part".into(),
        format!("quarantine/{digest}.g1.123-1"),
        "browser/sessions/purge-session/nested/profile".into(),
    ];
    for name in owned.iter().chain(kept.iter()) {
        let path = data.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"fixture bytes").unwrap();
    }
    std::fs::create_dir_all(data.join("browser/sessions/purge-session/empty")).unwrap();
    std::fs::create_dir_all(cache.join("artifact-previews")).unwrap();
    for name in [&key, &format!("{key}.123-1.tmp"), "workspace-cache"] {
        std::fs::write(cache.join("artifact-previews").join(name), b"preview bytes").unwrap();
    }
    kept.push("cache/artifact-previews/workspace-cache".into());
    let info = store.info();
    let selection = PurgeFileSelection {
        project_id: project("purge-a"),
        store_id: info.store_id,
        restore_epoch: info.restore_epoch,
        owners: vec![
            O::Blob {
                digest: digest.clone(),
                generation: "g1".into(),
            },
            O::ArtifactStaging {
                operation_id: id("purge-op"),
            },
            O::Quarantine {
                digest,
                generation: "g1".into(),
            },
            O::Preview {
                version_id: version,
            },
            O::BrowserProfile {
                session_id: id("purge-session"),
            },
            O::DiagnosticExport {
                file_id: id("purge-export"),
            },
            O::ClientPaste {
                file_id: id("purge-paste"),
            },
        ],
    };
    native_managed_file(
        store.as_ref(),
        &data.join("diagnostics/exports"),
        "purge-export",
        "DiagnosticExport",
        selection.owners[5].clone(),
        &selection,
    );
    native_managed_file(
        store.as_ref(),
        &paste,
        "purge-paste",
        "ClientPaste",
        selection.owners[6].clone(),
        &selection,
    );
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        data.join("source/main.rs"),
        data.join("browser/sessions/purge-session/source-link"),
    )
    .unwrap();
    #[cfg(windows)]
    {
        let output = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(
                data.join("browser")
                    .join("sessions")
                    .join("purge-session")
                    .join("source-link"),
            )
            .arg(data.join("source"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fixture junction creation: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let files = crate::fs::purge::NativeProjectPurgeFiles::new(data.clone(), cache, store)
        .with_paste_root(paste);
    let mut snapshot = files.inventory(&selection, &mut || false).unwrap();
    for owner in &selection.owners {
        assert!(
            snapshot.files.iter().any(|w| w.owner == *owner),
            "missing class {owner:?}"
        );
    }
    snapshot.files.sort_by_key(|w| {
        (
            std::cmp::Reverse(w.relative_entry.matches('/').count()),
            w.directory,
        )
    });
    for w in &snapshot.files {
        files.remove_reviewed(w, &mut || false).unwrap();
    }
    for w in &snapshot.files {
        files.remove_reviewed(w, &mut || false).unwrap();
    }
    assert!(
        files
            .inventory(&selection, &mut || false)
            .unwrap()
            .files
            .is_empty()
    );
    for path in owned {
        assert!(!data.join(path).exists());
    }
    assert!(!data.join("browser/sessions/purge-session").exists());
    for path in kept {
        assert!(
            data.join(&path).exists(),
            "independent file removed: {path}"
        );
    }
    assert_eq!(
        std::fs::read(data.join("source/main.rs")).unwrap(),
        b"fixture bytes"
    );
    assert_eq!(
        std::fs::read(data.join("source/.git/config")).unwrap(),
        b"fixture bytes"
    );
}

#[test]
fn native_purge_files_refuse_replacements_new_entries_wrong_bindings_and_forged_names() {
    use devworks_core::ports::project_purge::{
        ProjectPurgeFiles, PurgeFileFault, PurgeFileOwner as O, PurgeFileSelection,
    };
    let h = Harness::new();
    let store = Arc::new(h.store());
    let _ = purge_storage_fixture(store.as_ref());
    let root = h.root.0.join("browser/sessions/purge-session");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("profile"), b"original").unwrap();
    let info = store.info();
    let selection = PurgeFileSelection {
        project_id: project("purge-a"),
        store_id: info.store_id,
        restore_epoch: info.restore_epoch,
        owners: vec![O::BrowserProfile {
            session_id: id("purge-session"),
        }],
    };
    let files = crate::fs::purge::NativeProjectPurgeFiles::new(
        h.root.0.clone(),
        h.root.0.join("cache"),
        store,
    );
    let snapshot = files.inventory(&selection, &mut || false).unwrap();
    let profile = snapshot.files.iter().find(|w| !w.directory).unwrap();
    let dir = snapshot.files.iter().find(|w| w.directory).unwrap();
    let mut forged = profile.clone();
    forged.relative_entry = "other-session/profile".into();
    assert_eq!(
        files.remove_reviewed(&forged, &mut || false),
        Err(PurgeFileFault::Unattributed)
    );
    let mut stale = profile.clone();
    stale.restore_epoch += 1;
    assert_eq!(
        files.remove_reviewed(&stale, &mut || false),
        Err(PurgeFileFault::Changed)
    );
    assert_eq!(
        files.remove_reviewed(profile, &mut || true),
        Err(PurgeFileFault::Cancelled)
    );
    std::fs::write(root.join("profile"), b"modified").unwrap();
    assert_eq!(
        files.remove_reviewed(profile, &mut || false),
        Err(PurgeFileFault::Changed)
    );
    std::fs::rename(root.join("profile"), root.join("held-original")).unwrap();
    std::fs::write(root.join("profile"), b"original").unwrap();
    assert_eq!(
        files.remove_reviewed(profile, &mut || false),
        Err(PurgeFileFault::Changed)
    );
    std::fs::remove_file(root.join("profile")).unwrap();
    std::fs::remove_file(root.join("held-original")).unwrap();
    std::fs::write(root.join("unreviewed"), b"new private bytes").unwrap();
    assert_eq!(
        files.remove_reviewed(dir, &mut || false),
        Err(PurgeFileFault::InUse)
    );
    assert_eq!(
        std::fs::read(root.join("unreviewed")).unwrap(),
        b"new private bytes"
    );
    std::fs::remove_file(root.join("unreviewed")).unwrap();
    files.remove_reviewed(dir, &mut || false).unwrap();
    assert!(!root.exists());
}

#[test]
fn native_purge_files_require_the_published_transient_file_identity_and_content() {
    use devworks_core::ports::project_purge::{
        ProjectPurgeFiles, PurgeFileFault, PurgeFileOwner as O, PurgeFileSelection,
    };
    let h = Harness::new();
    let store = Arc::new(h.store());
    let _ = purge_storage_fixture(store.as_ref());
    let root = h.root.0.join("diagnostics/exports");
    let info = store.info();
    let selection = PurgeFileSelection {
        project_id: project("purge-a"),
        store_id: info.store_id,
        restore_epoch: info.restore_epoch,
        owners: vec![O::DiagnosticExport {
            file_id: id("purge-export"),
        }],
    };
    native_managed_file(
        store.as_ref(),
        &root,
        "purge-export",
        "DiagnosticExport",
        selection.owners[0].clone(),
        &selection,
    );
    let files = crate::fs::purge::NativeProjectPurgeFiles::new(
        h.root.0.clone(),
        h.root.0.join("cache"),
        store,
    );
    let snapshot = files.inventory(&selection, &mut || false).unwrap();
    let original = &snapshot.files[0];
    std::fs::write(root.join("purge-export"), b"unreviewed replacement bytes").unwrap();
    assert!(matches!(
        files.inventory(&selection, &mut || false),
        Err(PurgeFileFault::Changed)
    ));
    let mut forged = original.clone();
    forged.content_digest = devworks_core::fingerprint::sha256_hex(b"unreviewed replacement bytes");
    forged.byte_count = b"unreviewed replacement bytes".len() as u64;
    assert_eq!(
        files.remove_reviewed(&forged, &mut || false),
        Err(PurgeFileFault::Changed)
    );
    assert_eq!(
        std::fs::read(root.join("purge-export")).unwrap(),
        b"unreviewed replacement bytes"
    );
}

#[test]
fn purge_preparation_refuses_unreviewed_tables_columns_indexes_and_triggers_without_mutation() {
    for change in [
        "CREATE TABLE future_project_records(project_id TEXT,private_body TEXT)",
        "ALTER TABLE projects ADD COLUMN future_private_body TEXT",
        "CREATE INDEX future_project_owner ON projects(title)",
        "CREATE TRIGGER future_project_delete AFTER DELETE ON project_details BEGIN DELETE FROM project_repositories WHERE project_id=OLD.project_id; END",
    ] {
        let h = Harness::new();
        let store = h.store();
        let prepared = purge_storage_fixture(&store);
        h.raw().execute_batch(change).unwrap();
        assert!(
            matches!(
                run_write(&store, WriteOptions::default(), move |tx| tx
                    .projects()
                    .insert_purge_manifest(&prepared)),
                Err(StorageError::IncompatibleSchema(_))
            ),
            "unreviewed shape accepted: {change}"
        );
        let raw = h.raw();
        assert_eq!(
            raw.query_row("SELECT count(*) FROM project_purge_manifests", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            raw.query_row("SELECT title FROM projects WHERE id='purge-a'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "private project title"
        );
        assert!(store.integrity_check().unwrap().problems.is_empty());
    }
}
