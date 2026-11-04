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
