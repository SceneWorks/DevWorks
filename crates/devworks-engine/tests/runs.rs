//! Run root controls on the real SQLite store through the shared gateway and host authority
//! (DW-009): Start, Pause, Continue, Redirect, Stop, Inspect and Eligibility, the dispatch
//! substrate, restart/shutdown recovery and the dispatcher's root gate. Races are made
//! deterministic with barriers on the single write owner; no inference, network or sleeps used
//! as synchronization. Process trees are a scripted fake host (no OS processes).
mod common;

use common::*;
use devworks_adapters::fs::NativeFilesystem;
use devworks_adapters::fs::artifacts::NativeArtifactFiles;
use devworks_adapters::fs::sources::HostArtifactSources;
use devworks_core::authority::HostAuthority;
use devworks_core::authority::destructive::ProtectedRoots;
use devworks_core::authority::service::{GrantBounds, bind_principal};
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::gateway::Gateway;
use devworks_core::ports::process::{
    NativeIdentity, PowerAssertion, PowerPort, PowerRequest, Probe, ProcessError, ProcessHost,
    ProcessSpec, ProcessTree, TreeStatus,
};
use devworks_core::ports::storage::*;
use devworks_core::protocol::results::{DomainCommandReply, DomainDecision, DomainQueryReply};
use devworks_core::protocol::{
    ActorId, AssignmentId, CommandReply, HoldRef, Id, OperationId, OperationState, ProjectId,
    Revision, RootId,
};
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::process::{LaunchPolicy, LaunchRequest, ProcessSupervisor};
use devworks_core::services::projects::ProjectService;
use devworks_core::services::resources::ResourceService;
use devworks_core::services::runs::inspect::{RunCondition, RunInspection};
use devworks_core::services::runs::recovery::NoLifecycleHooks;
use devworks_core::services::runs::scope::{PlanState, Readiness, ScriptedScope};
use devworks_core::services::runs::substrate::{
    ActivationFailure, Admission, DispatchRequest, Disposition, Fenced, NewRunAssignment,
    ResultAcceptance, ResultClaim,
};
use devworks_core::services::runs::{ControlService, ResumeBinding, Target};
use devworks_core::services::sessions::SessionService;
use devworks_core::settings::catalog::AgentRole;
use devworks_engine::{
    ActivationLauncher, Composition, Launch, LaunchEnd, LaunchFuture, RunComposition, RunGate,
    RunSupervisor, RunTick, RuntimeConfig, ShutdownStage, boot,
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------------------------------------
// Fixture

const PROTOCOL: &str = "1.0-draft.1";

fn seed(h: &Harness) {
    let c = rusqlite::Connection::open(h.root.join("workspace.sqlite")).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    let work = |id: &str, project: &str, kind: &str, parent: Option<&str>| {
        format!(
            "INSERT INTO work_items VALUES('{id}','{project}','{kind}',{},NULL,NULL,'{id}',1,0,1,1,1,'{{}}');",
            parent.map_or("NULL".to_owned(), |p| format!("'{p}'"))
        )
    };
    let mut sql = String::from(
        "INSERT INTO projects(id,title,lifecycle,revision,scope_revision,created_at) VALUES('project-a','A','Active',1,1,0),('project-b','B','Active',1,1,0);",
    );
    for (id, project, kind, parent) in [
        ("root-f", "project-a", "feature", None),
        ("task-f1", "project-a", "task", Some("root-f")),
        ("task-f2", "project-a", "task", Some("root-f")),
        ("task-f3", "project-a", "task", Some("root-f")),
        ("root-i", "project-a", "issue", None),
        ("root-g", "project-a", "feature", None),
        ("root-b", "project-b", "issue", None),
    ] {
        sql.push_str(&work(id, project, kind, parent));
    }
    c.execute_batch(&sql).unwrap();
}

/// The one store, control service and gateway the tests drive.
struct Fx {
    h: Harness,
    store: Arc<dyn ApplicationStore>,
    scope: Arc<ScriptedScope>,
    service: Arc<ControlService>,
    use_cases: UseCases,
    user: InvocationContext,
    agent: InvocationContext,
    sched: InvocationContext,
    keys: AtomicUsize,
}
impl Fx {
    fn new() -> Self {
        let h = Harness::new();
        let store = h.open();
        seed(&h);
        Self::over(h, store)
    }
    fn over(h: Harness, store: Arc<dyn ApplicationStore>) -> Self {
        let scope = Arc::new(ScriptedScope::new());
        let service = Arc::new(ControlService::with_scope(h.ids.clone(), scope.clone()));
        let use_cases = UseCases::new()
            .with_runs(service.clone())
            .with_projects(Arc::new(ProjectService::new(
                h.ids.clone(),
                service.clone(),
            )))
            .with_sessions(Arc::new(SessionService::new(h.ids.clone(), store.clone())));
        let user = principal(&*store, "user-a", PrincipalKind::Human);
        let agent = principal(&*store, "agent-a", PrincipalKind::Agent);
        let sched = principal(&*store, "sched-a", PrincipalKind::Schedule);
        for (who, kind, role) in [
            ("agent-a", PrincipalKind::Agent, Some(AgentRole::Worker)),
            ("sched-a", PrincipalKind::Schedule, None),
        ] {
            let (u, b) = (
                user.clone(),
                NewPrincipalBinding {
                    actor: ActorId(id(who)),
                    kind,
                    role,
                    project_id: None,
                    tool_families: None,
                    bound_by: ActorId(id("user-a")),
                },
            );
            // A reopened store already has the binding.
            let _ = write(&*store, move |tx| {
                bind_principal(tx, &u, b, None)
                    .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
            });
        }
        Self {
            h,
            store,
            scope,
            service,
            use_cases,
            user,
            agent,
            sched,
            keys: AtomicUsize::new(0),
        }
    }
    /// The harness and store, with every other handle on the store dropped (a restart).
    fn into_parts(self) -> (Harness, Arc<dyn ApplicationStore>) {
        (self.h, self.store)
    }
    /// A request key never used before (also across reopened stores of one data root).
    fn key(&self) -> String {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        self.keys.fetch_add(1, Ordering::SeqCst);
        format!("key-{}", NEXT.fetch_add(1, Ordering::SeqCst))
    }
    fn authority(&self) -> HostAuthority<'_> {
        HostAuthority {
            store: &*self.store,
            clock: &*self.h.clock,
            protected_branches: &[],
        }
    }
    fn command(&self, ctx: &InvocationContext, op: &str, frame: Value) -> DomainCommandReply {
        let authority = self.authority();
        let gateway = Gateway {
            authority: &authority,
            store: &*self.store,
            ids: &*self.h.ids,
        };
        self.use_cases
            .command(&gateway, ctx, op, frame.to_string().as_bytes())
            .expect("addressable reply")
    }
    fn query(&self, ctx: &InvocationContext, op: &str, frame: Value) -> DomainQueryReply {
        let authority = self.authority();
        let gateway = Gateway {
            authority: &authority,
            store: &*self.store,
            ids: &*self.h.ids,
        };
        self.use_cases
            .query(&gateway, ctx, op, frame.to_string().as_bytes())
            .expect("addressable reply")
    }
    fn control(&self, root: &str) -> StoredRootControl {
        let r = RootId(id(root));
        read(&*self.store, |s| {
            Ok(s.runs()
                .root_control(&r)?
                .unwrap_or_else(|| StoredRootControl::initial(r.clone())))
        })
    }
    fn live(&self, root: &str) -> Option<StoredActivation> {
        let r = RootId(id(root));
        read(&*self.store, |s| s.runs().live_activation(&r))
    }
    fn activation(&self, a: &Id) -> StoredActivation {
        read(&*self.store, |s| s.runs().activation(a)).expect("activation")
    }
    fn holds(&self, root: &str) -> Vec<StoredBlocker> {
        let r = RootId(id(root));
        read(&*self.store, |s| {
            devworks_core::services::runs::active_blockers(s.runs(), &BlockerFilter::RootLevel(r))
        })
    }
    fn inspection(&self, root: &str) -> RunInspection {
        let (svc, ctx, r, now) = (
            self.service.clone(),
            self.user.clone(),
            rid(root),
            self.h.now(),
        );
        read(&*self.store, move |s| {
            svc.inspection(s, &ctx, &r, now)
                .map_err(|f| StorageError::InvalidRecord(format!("{f:?}")))
        })
    }
    fn root(&self, root: &str) -> WorkRoot {
        let r = RootId(id(root));
        read(&*self.store, |s| s.sessions().work_root(&r)).expect("root")
    }
    fn tx<T: Send + 'static>(
        &self,
        f: impl FnOnce(&ControlService, &mut dyn WriteTransaction) -> Result<T, StorageError>
        + Send
        + 'static,
    ) -> T {
        let service = self.service.clone();
        write(&*self.store, move |tx| f(&service, tx)).expect("write")
    }
    fn events(&self, root: &str) -> Vec<String> {
        let r = RootId(id(root));
        read(&*self.store, |s| {
            Ok(s.history()
                .page(&EventFilter::Root(r), 0, 500)?
                .events
                .into_iter()
                .map(|e| e.kind)
                .collect())
        })
    }

    // -- run commands -------------------------------------------------------------------------
    fn start(
        &self,
        ctx: &InvocationContext,
        root: &str,
        work: i64,
        control: i64,
    ) -> DomainCommandReply {
        self.start_key(ctx, &self.key(), root, work, control)
    }
    fn start_key(
        &self,
        ctx: &InvocationContext,
        key: &str,
        root: &str,
        work: i64,
        control: i64,
    ) -> DomainCommandReply {
        self.command(
            ctx,
            "runs.start",
            json!({"protocol": PROTOCOL, "requestKey": key, "command": {"kind": "runs.start",
                "rootId": root, "expectedWorkRevision": work.to_string(),
                "expectedControlRevision": control.to_string()}}),
        )
    }
    fn pause(&self, ctx: &InvocationContext, root: &str) -> DomainCommandReply {
        let control = self.control(root).revision;
        self.command(
            ctx,
            "runs.pause",
            json!({"protocol": PROTOCOL, "requestKey": self.key(), "command": {"kind": "runs.pause",
                "rootId": root, "expectedControlRevision": control.to_string()}}),
        )
    }
    fn resume_key(
        &self,
        ctx: &InvocationContext,
        key: &str,
        root: &str,
        control: i64,
        holds: &[(&Id, i64)],
    ) -> DomainCommandReply {
        let covered: Vec<Value> = holds
            .iter()
            .map(|(h, r)| json!({"holdId": h.as_str(), "revision": r.to_string()}))
            .collect();
        self.command(
            ctx,
            "runs.continue",
            json!({"protocol": PROTOCOL, "requestKey": key, "command": {"kind": "runs.continue",
                "rootId": root, "expectedControlRevision": control.to_string(),
                "coveredHolds": covered}}),
        )
    }
    /// Continue covering every current manual hold of the root.
    fn resume(&self, ctx: &InvocationContext, root: &str) -> DomainCommandReply {
        let holds = self.holds(root);
        let covered: Vec<(&Id, i64)> = holds
            .iter()
            .filter(|b| b.cause.manual_resume())
            .map(|b| (&b.id, b.revision))
            .collect();
        self.resume_key(
            ctx,
            &self.key(),
            root,
            self.control(root).revision,
            &covered,
        )
    }
    fn redirect(&self, ctx: &InvocationContext, root: &str, message: &str) -> DomainCommandReply {
        let control = self.control(root).revision;
        self.command(
            ctx,
            "runs.redirect",
            json!({"mode": "Command", "protocol": PROTOCOL, "requestKey": self.key(), "body": {
                "kind": "runs.redirect", "rootId": root,
                "expectedControlRevision": control.to_string(), "message": message,
                "attachments": []}}),
        )
    }
    fn stop(&self, ctx: &InvocationContext, root: &str) -> DomainCommandReply {
        let control = self.control(root).revision;
        self.command(
            ctx,
            "runs.stop",
            json!({"mode": "Command", "protocol": PROTOCOL, "requestKey": self.key(), "body": {
                "kind": "runs.stop", "rootId": root,
                "expectedControlRevision": control.to_string()}}),
        )
    }
    fn inspect(&self, ctx: &InvocationContext, root: &str) -> Value {
        result(&self.query(
            ctx,
            "runs.inspect",
            json!({"protocol": PROTOCOL, "queryId": "q-1", "query": {"kind": "runs.inspect", "rootId": root}}),
        ))
    }
    fn eligibility(
        &self,
        ctx: &InvocationContext,
        kind: &str,
        target: &str,
        purpose: &str,
    ) -> Value {
        result(&self.query(
            ctx,
            "runs.eligibility",
            json!({"protocol": PROTOCOL, "queryId": "q-2", "query": {"kind": "runs.eligibility",
                "target": {"kind": kind, "id": target}, "purpose": purpose}}),
        ))
    }
    /// A session of `root` started by the user through `sessions.start`.
    fn session(&self, root: &str) -> Id {
        let reply = self.command(
            &self.user,
            "sessions.start",
            json!({"mode": "Command", "protocol": PROTOCOL, "requestKey": self.key(), "body": {
                "kind": "sessions.start", "scope": {"kind": "Project", "projectId": "project-a"},
                "rootId": root, "title": "Work"}}),
        );
        let v = serde_json::to_value(&reply).unwrap();
        let sid = v["receipt"]["decision"]["result"]["value"]["sessionId"]
            .as_str()
            .or_else(|| v["receipt"]["decision"]["result"]["value"]["id"].as_str())
            .unwrap_or_else(|| panic!("session start: {v}"))
            .to_owned();
        id(&sid)
    }
    /// Claims (as `owner`) and starts the root's pending activation through the substrate.
    fn launch(&self, owner: &str) -> StoredActivation {
        let owner = id(owner);
        self.tx(move |svc, tx| {
            let a = svc
                .claim_activation(tx, &owner)?
                .expect("a claimable activation");
            Ok(svc
                .start_activation(tx, &a.id, a.owner_epoch)?
                .expect("started"))
        })
    }
    fn assign(
        &self,
        root: &str,
        work: &str,
        parent: Option<&AssignmentId>,
        key: Option<&str>,
    ) -> StoredAssignment {
        let new = NewRunAssignment {
            root: RootId(id(root)),
            work: RootId(id(work)),
            parent: parent.cloned(),
            session: self.session(root),
            scope_key: key.map(str::to_owned),
        };
        self.tx(move |svc, tx| {
            let host = tx.store().host_actor.clone();
            Ok(svc
                .register_assignment(tx, &host, new)?
                .expect("registered"))
        })
    }
    fn dispatch(
        &self,
        a: &StoredActivation,
        target: Target,
        purpose: ActivationPurpose,
    ) -> Result<Admission, Fenced> {
        let request = DispatchRequest {
            root: a.root_id.clone(),
            execution_id: a.id.clone(),
            owner_epoch: a.owner_epoch,
            target,
            purpose,
        };
        self.tx(move |svc, tx| svc.begin_dispatch(tx, &request))
    }
    fn hold(
        &self,
        root: &str,
        subject: devworks_core::services::runs::HoldSubject,
        cause: HoldCause,
        origin: &str,
    ) -> StoredBlocker {
        let (root, origin) = (self.root(root), origin.to_owned());
        self.tx(move |_, tx| {
            let host = tx.store().host_actor.clone();
            devworks_core::services::runs::raise_blocker(
                tx,
                &host,
                &root,
                subject,
                cause,
                &origin,
                1,
                "fixture",
                json!({}),
            )
        })
    }
}

fn decision(r: &DomainCommandReply) -> &DomainDecision {
    match r {
        CommandReply::Recorded { receipt, .. } => &receipt.decision,
        other => panic!("not recorded: {other:?}"),
    }
}
fn replayed(r: &DomainCommandReply) -> bool {
    matches!(r, CommandReply::Recorded { replayed: true, .. })
}
fn applied(r: &DomainCommandReply) -> Value {
    match decision(r) {
        DomainDecision::Applied { result } => {
            serde_json::to_value(result).unwrap()["value"].clone()
        }
        other => panic!("not applied: {other:?}"),
    }
}
fn unchanged(r: &DomainCommandReply) -> Value {
    match decision(r) {
        DomainDecision::NoChange { result } => {
            serde_json::to_value(result).unwrap()["value"].clone()
        }
        other => panic!("not unchanged: {other:?}"),
    }
}
/// The refusal code and its blocker IDs.
fn refused(r: &DomainCommandReply) -> (String, Vec<String>) {
    match decision(r) {
        DomainDecision::Refused { fault } => {
            let v = serde_json::to_value(fault).unwrap();
            (
                v["code"].as_str().unwrap().to_owned(),
                v["blockerIds"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|b| b.as_str().unwrap().to_owned())
                    .collect(),
            )
        }
        other => panic!("not refused: {other:?}"),
    }
}
fn result(r: &DomainQueryReply) -> Value {
    match r {
        DomainQueryReply::Result { result, .. } => {
            serde_json::to_value(result).unwrap()["value"].clone()
        }
        other => panic!("query rejected: {other:?}"),
    }
}
fn blocked(r: Result<Admission, Fenced>) -> Vec<(HoldCause, String)> {
    match r {
        Err(Fenced::Blocked(e)) => e
            .conditions
            .iter()
            .map(|c| (c.cause, c.reason.clone()))
            .collect(),
        other => panic!("not blocked: {other:?}"),
    }
}
fn rid(s: &str) -> RootId {
    RootId(id(s))
}

// ---------------------------------------------------------------------------------------------
// A1: one coordinator owner per root

#[test]
fn racing_starts_converge_on_one_activation_and_a_reused_key_replays() {
    let fx = Arc::new(Fx::new());
    const N: usize = 6;
    let barrier = Arc::new(Barrier::new(N));
    let replies: Vec<DomainCommandReply> = (0..N)
        .map(|i| {
            let (fx, barrier) = (fx.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                fx.start_key(&fx.user, &format!("start-{i}"), "root-f", 1, 1)
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect();
    let applied_n = replies
        .iter()
        .filter(|r| matches!(decision(r), DomainDecision::Applied { .. }))
        .count();
    let unchanged_n = replies
        .iter()
        .filter(|r| matches!(decision(r), DomainDecision::NoChange { .. }))
        .count();
    assert_eq!((applied_n, unchanged_n), (1, N - 1), "{replies:?}");
    // Every reply names the same activation.
    let first = fx.live("root-f").expect("one live activation");
    for r in &replies {
        let v = serde_json::to_value(decision(r)).unwrap();
        assert_eq!(
            v["result"]["value"]["activation"]["executionId"],
            first.id.as_str()
        );
    }
    assert_eq!(first.phase, ActivationPhase::Pending);
    assert_eq!(first.generation, 1);
    // A feature without an approved plan is accepted for planning only (Start permits planning).
    assert_eq!(first.purpose, ActivationPurpose::Planning);
    // The same request key replays its receipt; it never creates a second activation.
    let again = fx.start_key(&fx.user, "start-0", "root-f", 1, 1);
    assert!(replayed(&again));
    let count = read(&*fx.store, |s| {
        s.runs()
            .activations(&ActivationFilter::Root(rid("root-f")), None, 50)
    });
    assert_eq!(count.len(), 1);
    // The store backstop: a second live activation of one root is refused outright.
    let mut dup = first.clone();
    dup.id = id("duplicate");
    dup.generation = 9;
    let err = write(&*fx.store, move |tx| tx.runs().insert_activation(&dup)).unwrap_err();
    assert!(matches!(err, StorageError::Duplicate { .. }), "{err:?}");
    // A feature-linked task has no manual control.
    let task = fx.start(&fx.user, "task-f1", 1, 1);
    assert_eq!(refused(&task).0, "UnsupportedTarget");
    // Inspect is the same snapshot.
    let snap = fx.inspect(&fx.user, "root-f");
    assert_eq!(snap["root"]["desiredState"], "Enabled");
    assert_eq!(snap["activation"]["phase"], "Pending");
}

#[test]
fn one_live_coordinator_per_root_and_assignment_identity_is_enforced() {
    let fx = Fx::new();
    applied(&fx.start(&fx.user, "root-f", 1, 1));
    let coord = fx.assign("root-f", "root-f", None, None);
    // A second live coordinator is refused; so is work outside the root's hierarchy or a
    // session of another root.
    let session = fx.session("root-f");
    let cases = [
        NewRunAssignment {
            root: rid("root-f"),
            work: rid("task-f1"),
            parent: None,
            session: session.clone(),
            scope_key: None,
        },
        NewRunAssignment {
            root: rid("root-f"),
            work: rid("root-g"),
            parent: Some(coord.id.clone()),
            session: session.clone(),
            scope_key: None,
        },
        NewRunAssignment {
            root: rid("root-f"),
            work: rid("task-f1"),
            parent: Some(coord.id.clone()),
            session: fx.session("root-g"),
            scope_key: None,
        },
    ];
    for new in cases {
        let r = fx.tx(move |svc, tx| {
            let host = tx.store().host_actor.clone();
            svc.register_assignment(tx, &host, new)
        });
        assert!(matches!(r, Err(Fenced::Invalid(_))), "{r:?}");
    }
    let child = fx.assign("root-f", "task-f1", Some(&coord.id), None);
    assert_eq!(child.root_id, rid("root-f"));
    assert_eq!(child.parent_id, Some(coord.id.clone()));
}

// ---------------------------------------------------------------------------------------------
// CONTROL-03 / CONTROL-12: Pause, requested/acknowledged/stopped, Start leaves pauses

#[test]
fn start_leaves_paused_work_paused_and_pause_stages_are_distinct() {
    let fx = Fx::new();
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let running = fx.launch("supervisor-1");
    assert_eq!(running.phase, ActivationPhase::Running);
    let paused = applied(&fx.pause(&fx.user, "root-i"));
    assert_eq!(paused["root"]["desiredState"], "Paused");
    // Requested: the launched activation is Stopping, not yet stopped.
    let a = fx.activation(&running.id);
    assert_eq!(a.phase, ActivationPhase::Stopping);
    assert!(a.stop_requested_at_ms.is_some() && a.stop_acknowledged_at_ms.is_none());
    let hold = fx.holds("root-i").pop().expect("a human pause");
    assert_eq!(hold.cause, HoldCause::HumanPause);
    let seen = fx.inspection("root-i");
    assert_eq!(seen.condition, RunCondition::Paused);
    assert_eq!(seen.activations[0].phase, ActivationPhase::Stopping);
    // Start through the UI, an agent or a schedule leaves the pause effective.
    for ctx in [&fx.user, &fx.agent] {
        let r = fx.start(ctx, "root-i", 1, fx.control("root-i").revision);
        let (code, holds) = refused(&r);
        assert_eq!(
            (code.as_str(), holds),
            ("ContinueRequired", vec![hold.id.as_str().to_owned()])
        );
    }
    // A repeated pause is unchanged.
    unchanged(&fx.pause(&fx.user, "root-i"));
    // Acknowledged at a safe boundary, then stopped.
    let (x, e) = (running.id.clone(), running.owner_epoch);
    let acked = fx
        .tx(move |svc, tx| svc.acknowledge_stop(tx, &x, e))
        .unwrap();
    assert!(acked.stop_acknowledged_at_ms.is_some());
    assert_eq!(acked.phase, ActivationPhase::Stopping);
    let x = running.id.clone();
    let stopped = fx
        .tx(move |svc, tx| svc.finish_activation(tx, &x, e, ActivationOutcome::Cancelled, None))
        .unwrap();
    assert_eq!(stopped.phase, ActivationPhase::Stopped);
    assert_eq!(stopped.stop_cause, Some(StopCause::Pause));
    let kinds = fx.events("root-i");
    for k in [
        "RootPaused",
        "ActivationStopRequested",
        "ActivationStopAcknowledged",
        "ActivationStopped",
    ] {
        assert!(kinds.iter().any(|x| x == k), "{k} missing from {kinds:?}");
    }
    // A pending (never launched) activation is prevented outright by a pause.
    applied(&fx.start(&fx.user, "root-g", 1, 1));
    let pending = fx.live("root-g").unwrap();
    applied(&fx.pause(&fx.user, "root-g"));
    let a = fx.activation(&pending.id);
    assert_eq!(
        (a.phase, a.outcome),
        (ActivationPhase::Stopped, Some(ActivationOutcome::Prevented))
    );
    assert!(
        fx.tx(|svc, tx| svc.claim_activation(tx, &id("supervisor-1")))
            .is_none()
    );
}

#[test]
fn pause_and_dispatch_admission_are_ordered_on_the_write_owner() {
    // Both orders explicitly, then a barrier race asserting the invariant.
    let fx = Fx::new();
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let a = fx.launch("supervisor-1");
    // Admission wins: the effect is in flight; the pause after it requests a stop and later
    // admissions fail.
    let admitted = fx
        .dispatch(&a, Target::Root, ActivationPurpose::Implementation)
        .unwrap();
    applied(&fx.pause(&fx.user, "root-i"));
    assert!(admitted.control_revision < fx.control("root-i").revision);
    assert!(matches!(
        fx.dispatch(&a, Target::Root, ActivationPurpose::Implementation),
        Err(Fenced::StopRequested)
    ));
    // Pause wins: nothing is admitted under the newer revision.
    applied(&fx.start(&fx.user, "root-b", 1, 1));
    let b = fx.launch("supervisor-1");
    applied(&fx.pause(&fx.user, "root-b"));
    assert!(
        fx.dispatch(&b, Target::Root, ActivationPurpose::Implementation)
            .is_err()
    );

    // The race: whichever commits first on the write owner wins; never an admission after the
    // pause committed.
    let fx = Arc::new(Fx::new());
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let a = fx.launch("supervisor-1");
    let barrier = Arc::new(Barrier::new(2));
    let (f1, b1, a1) = (fx.clone(), barrier.clone(), a.clone());
    let dispatcher = std::thread::spawn(move || {
        b1.wait();
        f1.dispatch(&a1, Target::Root, ActivationPurpose::Implementation)
    });
    let (f2, b2) = (fx.clone(), barrier.clone());
    let pauser = std::thread::spawn(move || {
        b2.wait();
        f2.pause(&f2.user, "root-i")
    });
    let admission = dispatcher.join().unwrap();
    let paused = applied(&pauser.join().unwrap());
    let pause_revision: i64 = paused["root"]["revision"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    match admission {
        Ok(adm) => assert!(adm.control_revision < pause_revision),
        Err(Fenced::StopRequested) => {}
        Err(other) => panic!("unexpected refusal {other:?}"),
    }
    assert!(
        fx.dispatch(&a, Target::Root, ActivationPurpose::Implementation)
            .is_err()
    );
}

// ---------------------------------------------------------------------------------------------
// CONTROL-12: explicit Continue; renewed holds; resume authority

#[test]
fn continue_clears_only_covered_holds_and_a_reused_command_cannot_clear_a_later_pause() {
    let fx = Fx::new();
    applied(&fx.pause(&fx.user, "root-i"));
    let h1 = fx.holds("root-i").pop().unwrap();
    let rev = fx.control("root-i").revision;
    let first = fx.resume_key(
        &fx.user,
        "continue-1",
        "root-i",
        rev,
        &[(&h1.id, h1.revision)],
    );
    let snap = applied(&first);
    assert_eq!(snap["root"]["desiredState"], "Enabled");
    // Continue recorded a new activation (lineage preserved by the root, new attempt).
    assert_eq!(snap["activation"]["phase"], "Pending");
    // A renewed pause is a new hold; the old command cannot cover it.
    applied(&fx.pause(&fx.user, "root-i"));
    let h2 = fx.holds("root-i").pop().unwrap();
    assert_ne!(h1.id, h2.id);
    let replay = fx.resume_key(
        &fx.user,
        "continue-1",
        "root-i",
        rev,
        &[(&h1.id, h1.revision)],
    );
    assert!(replayed(&replay));
    assert_eq!(
        fx.holds("root-i").len(),
        1,
        "the later pause survives a replay"
    );
    // A new command still covering only the old hold names the uncovered one.
    let rev = fx.control("root-i").revision;
    let partial = fx.resume_key(&fx.user, &fx.key(), "root-i", rev, &[(&h1.id, h1.revision)]);
    let (code, holds) = refused(&partial);
    assert_eq!(
        (code.as_str(), holds),
        ("ContinueRequired", vec![h2.id.as_str().to_owned()])
    );
    // A stale expected revision is a revision conflict.
    let conflict = fx.resume_key(
        &fx.user,
        &fx.key(),
        "root-i",
        rev - 1,
        &[(&h2.id, h2.revision)],
    );
    assert_eq!(refused(&conflict).0, "RevisionConflict");
    // Continue cannot delete another cause's blocker.
    let dep = fx.hold(
        "root-i",
        Default::default(),
        HoldCause::Dependency,
        "task-x",
    );
    let wrong = fx.resume_key(
        &fx.user,
        &fx.key(),
        "root-i",
        rev,
        &[(&h2.id, h2.revision), (&dep.id, dep.revision)],
    );
    assert_eq!(refused(&wrong).0, "InvalidArgument");
    applied(&fx.resume_key(&fx.user, &fx.key(), "root-i", rev, &[(&h2.id, h2.revision)]));
    // CONTROL-07: the condition that still prevents progress stays visible.
    let snap = fx.inspect(&fx.user, "root-i");
    let causes: Vec<&str> = snap["blockers"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["cause"].as_str().unwrap())
        .collect();
    assert_eq!(causes, vec!["Dependency"]);
    let e = fx.eligibility(&fx.user, "root", "root-i", "Resume");
    assert_eq!(e["eligible"], false);
}

#[test]
fn agents_and_schedules_continue_a_human_pause_only_with_a_resume_grant() {
    let fx = Fx::new();
    applied(&fx.pause(&fx.user, "root-i"));
    let hold = fx.holds("root-i").pop().unwrap();
    // Broad tool access is not resume authority.
    let r = fx.resume(&fx.agent, "root-i");
    let (code, holds) = refused(&r);
    assert_eq!(
        (code.as_str(), holds),
        ("PermissionDenied", vec![hold.id.as_str().to_owned()])
    );
    // The schedule has no grant at all: refused by authority before admission.
    let r = fx.resume(&fx.sched, "root-i");
    assert!(matches!(r, CommandReply::Rejected { .. }), "{r:?}");
    // The user authorizes exactly the current pause for the agent.
    let mint = |holder: &str, binding: ResumeBinding, uses: i64| {
        let (u, holder) = (fx.user.clone(), ActorId(id(holder)));
        let now = fx.h.now();
        write(&*fx.store, move |tx| {
            devworks_core::authority::mint_resume_grant(
                tx,
                &u,
                holder,
                rid("root-i"),
                binding,
                GrantBounds {
                    expires_at_ms: now + 60_000,
                    uses,
                },
            )
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
        })
    };
    let grant = mint(
        "agent-a",
        ResumeBinding {
            holds: vec![HoldRef {
                hold_id: devworks_core::protocol::HoldId(hold.id.clone()),
                revision: Revision::new(hold.revision).unwrap(),
            }],
            causes: vec![],
        },
        1,
    )
    .unwrap();
    // Standing grants never cover restart or budget holds.
    assert!(
        mint(
            "sched-a",
            ResumeBinding {
                holds: vec![],
                causes: vec!["RestartContinue".into()]
            },
            5
        )
        .is_err()
    );
    let agent = fx.agent.clone().with_authority_source(grant.id.clone());
    let ok = fx.resume(&agent, "root-i");
    assert_eq!(applied(&ok)["root"]["desiredState"], "Enabled");
    let spent = read(&*fx.store, |s| s.grants().grant(&grant.id)).unwrap();
    assert!(
        !spent.active && spent.uses_remaining == Some(0),
        "{spent:?}"
    );
    // A later human pause is not covered by the spent, pause-specific authorization.
    applied(&fx.pause(&fx.user, "root-i"));
    assert!(!matches!(
        decision(&fx.resume(&agent, "root-i")),
        DomainDecision::Applied { .. }
    ));
    // A standing HumanPause grant lets the schedule Continue; revoking it ends that.
    let standing = mint(
        "sched-a",
        ResumeBinding {
            holds: vec![],
            causes: vec!["HumanPause".into()],
        },
        5,
    )
    .unwrap();
    let sched = fx.sched.clone().with_authority_source(standing.id.clone());
    applied(&fx.resume(&sched, "root-i"));
    applied(&fx.pause(&fx.user, "root-i"));
    let (u, gid, gr) = (fx.user.clone(), standing.id.clone(), standing.revision + 1);
    write(&*fx.store, move |tx| {
        devworks_core::authority::revoke_resume_grant(tx, &u, &gid, gr)
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    })
    .unwrap();
    assert!(!matches!(
        decision_or_rejected(&fx.resume(&sched, "root-i")),
        Some(DomainDecision::Applied { .. })
    ));
    assert_eq!(fx.control("root-i").desired, DesiredExecution::Paused);
    // A Continue with nothing left to clear never spends a grant use.
    let hold = fx.holds("root-i").pop().unwrap();
    applied(&fx.resume(&fx.user, "root-i"));
    let agents = mint(
        "agent-a",
        ResumeBinding {
            holds: vec![],
            causes: vec!["HumanPause".into()],
        },
        3,
    );
    assert!(agents.is_ok());
    let agents = agents.unwrap();
    let agent = fx.agent.clone().with_authority_source(agents.id.clone());
    let rev = fx.control("root-i").revision;
    let noop = fx.resume_key(
        &agent,
        &fx.key(),
        "root-i",
        rev,
        &[(&hold.id, hold.revision)],
    );
    unchanged(&noop);
    let kept = read(&*fx.store, |s| s.grants().grant(&agents.id)).unwrap();
    assert_eq!(kept.uses_remaining, Some(3));
}
fn decision_or_rejected(r: &DomainCommandReply) -> Option<&DomainDecision> {
    match r {
        CommandReply::Recorded { receipt, .. } => Some(&receipt.decision),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// CONTROL-13: eligible descendants; children keep their blockers

#[test]
fn continue_resumes_eligible_descendants_and_children_keep_their_blockers() {
    let fx = Fx::new();
    applied(&fx.start(&fx.user, "root-f", 1, 1));
    let coord = fx.assign("root-f", "root-f", None, None);
    let b = fx.assign("root-f", "task-f1", Some(&coord.id), None);
    let c = fx.assign("root-f", "task-f2", Some(&coord.id), None);
    let d = fx.assign("root-f", "task-f3", Some(&c.id), None);
    fx.hold(
        "root-f",
        devworks_core::services::runs::HoldSubject {
            assignment: Some(b.id.clone()),
            operation: None,
        },
        HoldCause::Approval,
        "proposal-1",
    );
    fx.hold(
        "root-f",
        devworks_core::services::runs::HoldSubject {
            assignment: Some(c.id.clone()),
            operation: None,
        },
        HoldCause::Dependency,
        "task-f3",
    );
    applied(&fx.pause(&fx.user, "root-f"));
    let root = fx.root("root-f");
    let runnable = |fx: &Fx| -> Vec<AssignmentId> {
        let (svc, root) = (fx.service.clone(), root.clone());
        read(&*fx.store, move |s| {
            svc.runnable_assignments(s.runs(), &root, ActivationPurpose::Planning)
        })
        .into_iter()
        .map(|a| a.id)
        .collect()
    };
    // A paused parent keeps every child stopped.
    assert!(runnable(&fx).is_empty());
    let snap = applied(&fx.resume(&fx.user, "root-f"));
    let causes: Vec<String> = snap["blockers"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["cause"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(causes.len(), 2);
    assert!(causes.contains(&"Approval".to_owned()) && causes.contains(&"Dependency".to_owned()));
    let mut got = runnable(&fx);
    got.sort_by(|x, y| x.0.as_str().cmp(y.0.as_str()));
    let mut want = vec![coord.id.clone(), d.id.clone()];
    want.sort_by(|x, y| x.0.as_str().cmp(y.0.as_str()));
    assert_eq!(got, want);
    let e = fx.eligibility(&fx.user, "assignment", b.id.0.as_str(), "Planning");
    assert_eq!(e["eligible"], false);
    assert_eq!(e["blockers"]["items"][0]["cause"], "Approval");
}

// ---------------------------------------------------------------------------------------------
// Parallel tool isolation and one continuation per session

#[test]
fn a_held_tool_call_blocks_only_itself_and_its_dependents() {
    let fx = Fx::new();
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let coord = fx.assign("root-i", "root-i", None, None);
    let a = fx.launch("supervisor-1");
    let (op_a, op_b, op_c) = (
        OperationId(id("op-a")),
        OperationId(id("op-b")),
        OperationId(id("op-c")),
    );
    for op in [&op_a, &op_b, &op_c] {
        let op = op.clone();
        write(&*fx.store, move |tx| {
            tx.operations().record_intent(NewOperation {
                id: op,
                root_id: Some(rid("root-i")),
                assignment_id: None,
                capability: "fixture.tool".into(),
                schema_version: 1,
                args_digest: "d".into(),
                payload: json!({}),
            })
        })
        .unwrap();
    }
    fx.hold(
        "root-i",
        devworks_core::services::runs::HoldSubject {
            assignment: None,
            operation: Some(op_a.clone()),
        },
        HoldCause::Approval,
        "proposal-a",
    );
    let call = |op: &OperationId, deps: Vec<OperationId>| Target::Operation {
        assignment: Some(coord.id.clone()),
        operation: op.clone(),
        depends_on: deps,
    };
    assert_eq!(
        blocked(fx.dispatch(&a, call(&op_a, vec![]), ActivationPurpose::Implementation))[0].0,
        HoldCause::Approval
    );
    assert!(
        fx.dispatch(&a, call(&op_b, vec![]), ActivationPurpose::Implementation)
            .is_ok()
    );
    assert_eq!(
        blocked(fx.dispatch(
            &a,
            call(&op_c, vec![op_a.clone()]),
            ActivationPurpose::Implementation
        ))[0]
            .0,
        HoldCause::Dependency
    );
    // A root pause invalidates every call, independent or not.
    applied(&fx.pause(&fx.user, "root-i"));
    assert!(
        fx.dispatch(&a, call(&op_b, vec![]), ActivationPurpose::Implementation)
            .is_err()
    );
}

#[test]
fn a_session_has_one_continuation_in_flight() {
    let fx = Arc::new(Fx::new());
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let coord = fx.assign("root-i", "root-i", None, None);
    let a = fx.launch("supervisor-1");
    let coord = read(&*fx.store, |s| s.runs().assignment(&coord.id)).unwrap();
    let barrier = Arc::new(Barrier::new(4));
    let results: Vec<_> = (0..4)
        .map(|_| {
            let (fx, barrier, coord, a) = (fx.clone(), barrier.clone(), coord.clone(), a.clone());
            std::thread::spawn(move || {
                barrier.wait();
                fx.tx(move |svc, tx| {
                    svc.begin_turn(tx, &coord.session_id, &coord.id, &a.id, a.owner_epoch)
                })
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect();
    let won: Vec<&StoredTurn> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    assert_eq!(won.len(), 1, "{results:?}");
    let turn = won[0].clone();
    let (s, t) = (coord.session_id.clone(), turn.turn_id.clone());
    fx.tx(move |svc, tx| svc.end_turn(tx, &s, &t)).unwrap();
    let (c2, a2) = (coord.clone(), a.clone());
    assert!(
        fx.tx(move |svc, tx| svc.begin_turn(tx, &c2.session_id, &c2.id, &a2.id, a2.owner_epoch))
            .is_ok()
    );
}

// ---------------------------------------------------------------------------------------------
// CONTROL-02 / CONTROL-04: redirect stages, fencing only incompatible work, late results

#[test]
fn redirect_is_received_acknowledged_and_incorporated_and_fences_only_incompatible_work() {
    let fx = Fx::new();
    applied(&fx.start(&fx.user, "root-f", 1, 1));
    let coord = fx.assign("root-f", "root-f", None, None);
    let w1 = fx.assign("root-f", "task-f1", Some(&coord.id), None);
    let w2 = fx.assign("root-f", "task-f2", Some(&coord.id), None);
    let a = fx.launch("supervisor-1");
    // An admission before the redirect is in flight.
    let before = fx
        .dispatch(
            &a,
            Target::Assignment(coord.id.clone()),
            ActivationPurpose::Planning,
        )
        .unwrap();
    let summary = applied(&fx.redirect(&fx.user, "root-f", "Use the new storage layout"));
    assert_eq!(summary["changed"], true);
    let instruction = id(summary["id"].as_str().unwrap());
    let seen = fx.inspection("root-f");
    assert_eq!(seen.instructions.len(), 1);
    assert!(seen.sessions().contains(&coord.session_id));
    let stored = read(&*fx.store, |s| s.runs().instruction(&instruction)).unwrap();
    assert_eq!(stored.stage, InstructionStage::Received);
    assert_eq!(
        stored.session_id.as_ref(),
        Some(&coord.session_id),
        "linked to the coordinator session"
    );
    assert!(
        stored.transcript_entry_id.is_some(),
        "appended to the coordinator transcript"
    );
    assert!(before.instruction_revision < stored.instruction_revision);
    // The coordinator's next dispatch is fenced until it acknowledges; workers keep running.
    let fenced = blocked(fx.dispatch(
        &a,
        Target::Assignment(coord.id.clone()),
        ActivationPurpose::Planning,
    ));
    assert!(
        fenced.iter().any(|(_, why)| why.contains("redirect")),
        "{fenced:?}"
    );
    assert!(
        fx.dispatch(
            &a,
            Target::Assignment(w1.id.clone()),
            ActivationPurpose::Planning
        )
        .is_ok()
    );
    // Acknowledged at a safe boundary.
    let (x, e, seq) = (a.id.clone(), a.owner_epoch, stored.seq);
    let acked = fx
        .tx(move |svc, tx| svc.acknowledge_instructions(tx, &x, e, seq))
        .unwrap();
    assert_eq!(acked.len(), 1);
    assert!(
        fx.dispatch(
            &a,
            Target::Assignment(coord.id.clone()),
            ActivationPurpose::Planning
        )
        .is_ok()
    );
    // Incorporated: only the incompatible worker is superseded.
    let (x, i, w) = (a.id.clone(), instruction.clone(), w1.id.clone());
    let done = fx
        .tx(move |svc, tx| {
            svc.incorporate_instruction(
                tx,
                &x,
                e,
                &i,
                Disposition {
                    summary: "task 1 is obsolete".into(),
                    superseded: vec![w],
                    pending_plan_revision: None,
                },
            )
        })
        .unwrap();
    assert_eq!(done.stage, InstructionStage::Incorporated);
    assert!(
        fx.dispatch(
            &a,
            Target::Assignment(w1.id.clone()),
            ActivationPurpose::Planning
        )
        .is_err()
    );
    assert!(
        fx.dispatch(
            &a,
            Target::Assignment(w2.id.clone()),
            ActivationPurpose::Planning
        )
        .is_ok()
    );
    // A late result of the superseded worker is retained, never current.
    let claim = ResultClaim {
        assignment: w1.id.clone(),
        execution_id: a.id.clone(),
        owner_epoch: a.owner_epoch,
        plan_revision: None,
    };
    let verdict = fx.tx(move |svc, tx| {
        let host = tx.store().host_actor.clone();
        svc.accept_result(tx, &host, &claim)
    });
    assert!(matches!(verdict, ResultAcceptance::Stale(_)));
    assert!(
        fx.events("root-f")
            .iter()
            .any(|k| k == "LateResultRetained")
    );
    // A redirect never clears a pause.
    applied(&fx.pause(&fx.user, "root-f"));
    applied(&fx.redirect(&fx.user, "root-f", "and keep the old API"));
    assert_eq!(fx.control("root-f").desired, DesiredExecution::Paused);
    assert_eq!(fx.holds("root-f").len(), 1);
}

#[test]
fn a_result_from_an_earlier_activation_or_older_plan_is_not_current() {
    let fx = Fx::new();
    fx.scope.approve(&rid("root-f"), 1, &[("x", "dx")]);
    applied(&fx.start(&fx.user, "root-f", 1, 1));
    let coord = fx.assign("root-f", "root-f", None, None);
    let w = fx.assign("root-f", "task-f1", Some(&coord.id), Some("x"));
    let old = fx.launch("supervisor-1");
    let claim = |a: &StoredActivation, plan: Option<i64>| ResultClaim {
        assignment: w.id.clone(),
        execution_id: a.id.clone(),
        owner_epoch: a.owner_epoch,
        plan_revision: plan,
    };
    let accept = |c: ResultClaim| {
        fx.tx(move |svc, tx| {
            let host = tx.store().host_actor.clone();
            svc.accept_result(tx, &host, &c)
        })
    };
    assert_eq!(accept(claim(&old, Some(1))), ResultAcceptance::Current);
    applied(&fx.pause(&fx.user, "root-f"));
    let (x, e) = (old.id.clone(), old.owner_epoch);
    fx.tx(move |svc, tx| svc.finish_activation(tx, &x, e, ActivationOutcome::Cancelled, None))
        .unwrap();
    applied(&fx.resume(&fx.user, "root-f"));
    let new = fx.launch("supervisor-1");
    assert!(matches!(
        accept(claim(&old, Some(1))),
        ResultAcceptance::Stale(_)
    ));
    assert_eq!(accept(claim(&new, Some(1))), ResultAcceptance::Current);
    // A compatible replacement plan rebinds the work; a result under the older plan is stale.
    fx.scope.approve(&rid("root-f"), 2, &[("x", "dx")]);
    assert!(
        fx.dispatch(
            &new,
            Target::Assignment(w.id.clone()),
            ActivationPurpose::Implementation
        )
        .is_ok()
    );
    assert!(matches!(
        accept(claim(&new, Some(1))),
        ResultAcceptance::Stale(_)
    ));
    assert_eq!(accept(claim(&new, Some(2))), ResultAcceptance::Current);
}

// ---------------------------------------------------------------------------------------------
// A1/AR-02: active plan while a revision is pending; adoption fences only incompatible work

#[test]
fn eligibility_uses_the_active_plan_and_adoption_fences_only_incompatible_work() {
    let fx = Fx::new();
    let f = rid("root-f");
    fx.scope.approve(&f, 1, &[("x", "dx"), ("y", "dy")]);
    fx.scope.set_pending(&f, Some(2));
    applied(&fx.start(&fx.user, "root-f", 1, 1));
    assert_eq!(
        fx.live("root-f").unwrap().purpose,
        ActivationPurpose::Implementation
    );
    let coord = fx.assign("root-f", "root-f", None, None);
    let wx = fx.assign("root-f", "task-f1", Some(&coord.id), Some("x"));
    let wy = fx.assign("root-f", "task-f2", Some(&coord.id), Some("y"));
    // A unit only in the pending revision cannot be dispatched (or even bound).
    let pending_only = NewRunAssignment {
        root: f.clone(),
        work: rid("task-f3"),
        parent: Some(coord.id.clone()),
        session: fx.session("root-f"),
        scope_key: Some("z".into()),
    };
    let r = fx.tx(move |svc, tx| {
        let host = tx.store().host_actor.clone();
        svc.register_assignment(tx, &host, pending_only)
    });
    assert!(matches!(r, Err(Fenced::Invalid(_))));
    let a = fx.launch("supervisor-1");
    assert!(
        fx.dispatch(
            &a,
            Target::Assignment(wx.id.clone()),
            ActivationPurpose::Implementation
        )
        .is_ok()
    );
    assert!(
        fx.dispatch(
            &a,
            Target::Assignment(wy.id.clone()),
            ActivationPurpose::Implementation
        )
        .is_ok()
    );
    // The approved replacement changes y and keeps x: adopted at the next safe boundary.
    fx.scope.approve(&f, 3, &[("x", "dx"), ("y", "dy2")]);
    assert!(
        fx.dispatch(
            &a,
            Target::Assignment(wx.id.clone()),
            ActivationPurpose::Implementation
        )
        .is_ok()
    );
    let y = read(&*fx.store, |s| s.runs().assignment(&wy.id)).unwrap();
    assert_eq!(y.state, AssignmentState::Superseded);
    let x = read(&*fx.store, |s| s.runs().assignment(&wx.id)).unwrap();
    assert_eq!(
        (x.state, x.plan_revision),
        (AssignmentState::Ready, Some(3))
    );
    assert!(
        fx.dispatch(
            &a,
            Target::Assignment(wy.id.clone()),
            ActivationPurpose::Implementation
        )
        .is_err()
    );
    assert!(fx.events("root-f").iter().any(|k| k == "PlanAdopted"));
    // An unapproved feature (only a pending plan) cannot implement at all.
    fx.scope.set(
        &rid("root-g"),
        PlanState::Unapproved {
            pending_revision: Some(1),
        },
    );
    let e = fx.eligibility(&fx.user, "root", "root-g", "Implementation");
    assert_eq!(
        (
            e["eligible"].clone(),
            e["blockers"]["items"][0]["cause"].clone()
        ),
        (json!(false), json!("Approval"))
    );
}

// ---------------------------------------------------------------------------------------------
// A3 / AD79: readiness gate

#[test]
fn implementation_dispatch_requires_current_readiness_independently_of_approval() {
    let fx = Fx::new();
    let f = rid("root-f");
    fx.scope.approve(&f, 1, &[("x", "dx"), ("y", "dy")]);
    fx.scope.set_readiness(&f, None, Readiness::Missing);
    fx.scope.set_readiness(&f, Some("x"), Readiness::Missing);
    // Start permits planning/assessment of an approved but unassessed feature.
    applied(&fx.start(&fx.user, "root-f", 1, 1));
    assert_eq!(
        fx.live("root-f").unwrap().purpose,
        ActivationPurpose::Planning
    );
    let coord = fx.assign("root-f", "root-f", None, None);
    let wx = fx.assign("root-f", "task-f1", Some(&coord.id), Some("x"));
    let wy = fx.assign("root-f", "task-f2", Some(&coord.id), Some("y"));
    let a = fx.launch("supervisor-1");
    assert!(
        fx.dispatch(
            &a,
            Target::Assignment(coord.id.clone()),
            ActivationPurpose::Planning
        )
        .is_ok()
    );
    let gate = blocked(fx.dispatch(
        &a,
        Target::Assignment(wx.id.clone()),
        ActivationPurpose::Implementation,
    ));
    assert!(
        gate.iter().any(|(_, why)| why.contains("readiness")),
        "{gate:?}"
    );
    let e = fx.eligibility(&fx.user, "assignment", wx.id.0.as_str(), "Implementation");
    assert_eq!(e["eligible"], false);
    // The assessment becomes current: implementation is admitted.
    fx.scope.set_readiness(
        &f,
        Some("x"),
        Readiness::Current {
            assessment_revision: 2,
        },
    );
    let adm = fx
        .dispatch(
            &a,
            Target::Assignment(wx.id.clone()),
            ActivationPurpose::Implementation,
        )
        .unwrap();
    assert_eq!(adm.readiness_revision, Some(2));
    // Continue reuses the current assessment: no new assessment is needed after a pause.
    applied(&fx.pause(&fx.user, "root-f"));
    let (x, ep) = (a.id.clone(), a.owner_epoch);
    fx.tx(move |svc, tx| svc.finish_activation(tx, &x, ep, ActivationOutcome::Cancelled, None))
        .unwrap();
    applied(&fx.resume(&fx.user, "root-f"));
    let b = fx.launch("supervisor-1");
    assert!(
        fx.dispatch(
            &b,
            Target::Assignment(wx.id.clone()),
            ActivationPurpose::Implementation
        )
        .is_ok()
    );
    // Changed evidence fences only the affected scope and clears no pause.
    fx.scope.set_readiness(
        &f,
        Some("y"),
        Readiness::Stale {
            assessment_revision: 1,
        },
    );
    applied(&fx.pause(&fx.user, "root-g"));
    assert!(
        fx.dispatch(
            &b,
            Target::Assignment(wy.id.clone()),
            ActivationPurpose::Implementation
        )
        .is_err()
    );
    assert!(
        fx.dispatch(
            &b,
            Target::Assignment(wx.id.clone()),
            ActivationPurpose::Implementation
        )
        .is_ok()
    );
    assert_eq!(fx.control("root-g").desired, DesiredExecution::Paused);
    // A standalone issue needs no feature plan or assessment.
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let i = fx.launch("supervisor-1");
    assert!(
        fx.dispatch(&i, Target::Root, ActivationPurpose::Implementation)
            .is_ok()
    );
}

#[test]
fn the_production_scope_fails_closed_without_recorded_readiness() {
    let h = Harness::new();
    let store = h.open();
    seed(&h);
    let c = rusqlite::Connection::open(h.root.join("workspace.sqlite")).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c.execute_batch(
        "INSERT INTO actors VALUES('planner','human','Planner');
         INSERT INTO feature_plans(feature_id,revision,approved_plan_json,approved_by,approved_at) VALUES('root-f',4,'{}','planner',1);",
    )
    .unwrap();
    drop(c);
    let service = ControlService::new(h.ids.clone());
    let root = read(&*store, |s| s.sessions().work_root(&rid("root-f"))).unwrap();
    let plan = read(&*store, |s| service.scope().plan(s.runs(), &root));
    assert_eq!(plan.approved_revision(), Some(4));
    let e = read(&*store, |s| {
        service.evaluate(
            s.runs(),
            &root,
            &Target::Root,
            ActivationPurpose::Implementation,
        )
    });
    assert!(!e.eligible && e.conditions.iter().any(|c| c.reason.contains("readiness")));
    let p = read(&*store, |s| {
        service.evaluate(s.runs(), &root, &Target::Root, ActivationPurpose::Planning)
    });
    assert!(p.eligible);
}

// ---------------------------------------------------------------------------------------------
// CONTROL-03: Stop propagates to owned processes; retained CancelRun behaviour

#[derive(Default)]
struct FakeHost {
    /// Trees that ignore stop and kill (never verifiably gone).
    stubborn: Arc<AtomicBool>,
    stopped: Arc<AtomicUsize>,
    /// Runs inside the next spawn: after the launch intent committed, before the tree exists.
    on_spawn: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
struct FakeTree {
    identity: NativeIdentity,
    gone: bool,
    stubborn: Arc<AtomicBool>,
    stopped: Arc<AtomicUsize>,
}
impl ProcessTree for FakeTree {
    fn identity(&self) -> &NativeIdentity {
        &self.identity
    }
    fn request_stop(&mut self) -> Result<bool, ProcessError> {
        if !self.stubborn.load(Ordering::SeqCst) {
            self.gone = true;
            self.stopped.fetch_add(1, Ordering::SeqCst);
        }
        Ok(true)
    }
    fn kill(&mut self) -> Result<(), ProcessError> {
        if !self.stubborn.load(Ordering::SeqCst) {
            self.gone = true;
        }
        Ok(())
    }
    fn poll(&mut self) -> Result<TreeStatus, ProcessError> {
        Ok(TreeStatus {
            empty: self.gone,
            exit_code: self.gone.then_some(0),
        })
    }
    fn take_output(
        &mut self,
    ) -> Option<(Box<dyn std::io::Read + Send>, Box<dyn std::io::Read + Send>)> {
        None
    }
}
impl ProcessHost for FakeHost {
    fn platform(&self) -> &'static str {
        "unix"
    }
    fn environment(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn spawn(&self, _: &ProcessSpec) -> Result<Box<dyn ProcessTree>, ProcessError> {
        if let Some(f) = self.on_spawn.lock().unwrap().take() {
            f();
        }
        Ok(Box::new(FakeTree {
            identity: NativeIdentity {
                pid: 4242,
                birth: "b".into(),
                group: Some(4242),
            },
            gone: false,
            stubborn: self.stubborn.clone(),
            stopped: self.stopped.clone(),
        }))
    }
    fn probe(&self, _: &NativeIdentity) -> Probe {
        Probe::Gone
    }
    fn terminate(&self, _: &NativeIdentity) -> Probe {
        Probe::Gone
    }
}

fn supervisor(
    fx: &Fx,
    host: Arc<FakeHost>,
    launcher: Option<Arc<dyn ActivationLauncher>>,
) -> (Arc<RunSupervisor>, Arc<ProcessSupervisor>) {
    let processes = Arc::new(ProcessSupervisor::new(
        fx.store.clone(),
        host,
        Arc::new(ResourceService::new(fx.h.ids.clone(), fx.h.clock())),
    ));
    let sup = RunSupervisor::new(
        fx.store.clone(),
        fx.h.clock(),
        fx.service.clone(),
        processes.clone(),
        launcher,
        Arc::new(NoLifecycleHooks),
        id("supervisor-1"),
    );
    (sup, processes)
}
fn spawn_tree(fx: &Fx, processes: &ProcessSupervisor, root: &str, project: &str) {
    spawn_owned(fx, processes, Some(root), project);
}
fn spawn_owned(fx: &Fx, processes: &ProcessSupervisor, root: Option<&str>, project: &str) {
    let ctx = fx
        .agent
        .clone()
        .with_lineage(Some(ProjectId(id(project))), root.map(rid), None);
    let fs = NativeFilesystem::default();
    let roots = ProtectedRoots::new(vec![], vec![], None);
    let policy = LaunchPolicy {
        fs: &fs,
        roots: &roots,
        protected_branches: &[],
    };
    processes
        .launch(
            &ctx,
            LaunchRequest {
                spec: ProcessSpec::new(&["fake-tool".to_owned()]).unwrap(),
                operation_id: None,
                lease: None,
                approved_hold: false,
            },
            &policy,
        )
        .expect("launched");
}

#[test]
fn stop_cancels_root_work_stops_owned_trees_and_answers_a_finished_stop_unchanged() {
    let fx = Fx::new();
    seed_delivery(&fx, "root-i");
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let host = Arc::new(FakeHost::default());
    let (sup, processes) = supervisor(&fx, host.clone(), None);
    spawn_tree(&fx, &processes, "root-i", "project-a");
    spawn_tree(&fx, &processes, "root-b", "project-b");
    let stop = fx.stop(&fx.user, "root-i");
    let op = match decision(&stop) {
        DomainDecision::Accepted { operation_id } => operation_id.clone(),
        other => panic!("stop not accepted: {other:?}"),
    };
    // The never-launched activation stopped at once; the root is paused under a new epoch and
    // its queued delivery was cancelled.
    let ctl = fx.control("root-i");
    assert_eq!((ctl.desired, ctl.stop_epoch), (DesiredExecution::Paused, 1));
    assert!(fx.live("root-i").is_none());
    let deliveries = read(&*fx.store, |s| {
        s.runs().root_deliveries(&rid("root-i"), None, 10)
    });
    assert!(deliveries.is_empty(), "{deliveries:?}");
    // The supervisor executes the stop: the root's owned tree is stopped, the other root's is not.
    let rt = runtime();
    let tick = rt.block_on(sup.tick()).unwrap();
    assert_eq!(
        tick.stops,
        vec![(rid("root-i"), OperationState::ObservedComplete)]
    );
    assert_eq!(host.stopped.load(Ordering::SeqCst), 1);
    assert_eq!(
        processes.owned().len(),
        1,
        "the other project's tree keeps running"
    );
    let state = read(&*fx.store, |s| s.operations().operation(&op))
        .unwrap()
        .state;
    assert_eq!(state, OperationState::ObservedComplete);
    let seen = fx.inspection("root-i");
    assert_eq!(
        seen.stop.map(|o| o.state),
        Some(OperationState::ObservedComplete)
    );
    assert!(seen.processes.is_empty());
    // CodeGraph CancelRun: a finished stop answers itself unchanged.
    let again = fx.stop(&fx.user, "root-i");
    let v = unchanged(&again);
    assert_eq!(
        (v["operationId"].as_str(), v["state"].as_str()),
        (Some(op.0.as_str()), Some("ObservedComplete"))
    );
    // An invisible root is NotFound.
    let other = fx
        .user
        .clone()
        .with_lineage(Some(ProjectId(id("project-b"))), None, None);
    assert_eq!(refused(&fx.stop(&other, "root-i")).0, "NotFound");
    // Nothing live: the stop is observed complete in the command itself.
    let quiet = fx.stop(&fx.user, "root-g");
    assert_eq!(applied(&quiet)["state"], "ObservedComplete");
}

#[test]
fn a_tree_that_cannot_be_verified_stopped_holds_the_root_for_reconciliation() {
    let fx = Fx::new();
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let host = Arc::new(FakeHost::default());
    host.stubborn.store(true, Ordering::SeqCst);
    let (sup, processes) = supervisor(&fx, host, None);
    let mut sup = Arc::try_unwrap(sup).ok().expect("the only handle");
    sup.stop_grace = Duration::from_millis(20);
    sup.stop_bound = Duration::from_millis(100);
    let sup = Arc::new(sup);
    spawn_tree(&fx, &processes, "root-i", "project-a");
    let _ = fx.stop(&fx.user, "root-i");
    let tick = runtime().block_on(sup.tick()).unwrap();
    assert_eq!(
        tick.stops,
        vec![(rid("root-i"), OperationState::OutcomeUnknown)]
    );
    let holds = fx.holds("root-i");
    assert!(
        holds.iter().any(|b| b.cause == HoldCause::Reconciliation),
        "{holds:?}"
    );
    // Continue cannot clear the reconciliation hold; the condition stays inspectable.
    applied(&fx.resume(&fx.user, "root-i"));
    assert!(
        fx.holds("root-i")
            .iter()
            .any(|b| b.cause == HoldCause::Reconciliation)
    );
}

fn seed_delivery(fx: &Fx, root: &str) {
    let r = rid(root);
    write(&*fx.store, move |tx| {
        let e = event(tx, "Changed")?;
        let entry = tx.new_id(devworks_core::ports::IdKind::QueueEntry);
        tx.queue().enqueue(NewQueueEntry {
            id: entry,
            consumer: "root-work".into(),
            event_id: e,
            root_id: Some(r),
            operation_id: None,
            partition: None,
            priority: 0,
            ready_at_ms: 0,
            payload: json!({}),
        })
    })
    .unwrap();
}

fn archive_frame(fx: &Fx, project: &str, revision: &str) -> Value {
    json!({"mode": "Command", "protocol": PROTOCOL, "requestKey": fx.key(),
        "body": {"kind": "projects.archive", "projectId": project,
            "expectedRevision": revision}})
}

#[test]
fn a_committed_archive_kills_the_projects_trees_at_once_through_the_runtime() {
    let fx = Fx::new();
    applied(&fx.start(&fx.user, "root-f", 1, 1));
    applied(&fx.start(&fx.user, "root-b", 1, 1));
    let rt = runtime();
    let host = Arc::new(FakeHost::default());
    let app = rt
        .block_on(boot(composition_with(
            &fx,
            None,
            Arc::new(FakePower::default()),
            Arc::new(Authority),
            host.clone(),
        )))
        .unwrap_or_else(|e| panic!("boot: {e:?}"));
    spawn_tree(&fx, app.processes(), "root-f", "project-a");
    // A tree the project owns without a root is stopped too.
    spawn_owned(&fx, app.processes(), None, "project-a");
    spawn_tree(&fx, app.processes(), "root-b", "project-b");
    assert_eq!(app.processes().owned().len(), 3);
    let frame = archive_frame(&fx, "project-a", "1");
    let reply = app
        .command(&fx.user, "projects.archive", frame.to_string().as_bytes())
        .expect("addressable");
    let CommandReply::Recorded { receipt, .. } = &reply else {
        panic!("{reply:?}")
    };
    assert!(
        matches!(receipt.decision, DomainDecision::Applied { .. }),
        "{reply:?}"
    );
    // The fence and the root stops are already committed when the reply returns.
    for r in ["root-f", "root-g", "root-i"] {
        assert_eq!(fx.control(r).desired, DesiredExecution::Paused, "{r}");
        assert!(fx.control(r).stop_operation_id.is_some(), "{r}");
    }
    // Then the project's owned trees are killed without a cooperative grace, and only its own.
    rt.block_on(within("the archived project's trees stop", async {
        while app.processes().owned().len() != 1 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }));
    assert!(fx.live("root-b").is_some(), "other projects continue");
    assert_eq!(fx.control("root-b").desired, DesiredExecution::Enabled);
    // No tree is launched for the archived project any more, by any path; others still are.
    let ctx =
        fx.agent
            .clone()
            .with_lineage(Some(ProjectId(id("project-a"))), Some(rid("root-f")), None);
    let fs = NativeFilesystem::default();
    let roots = ProtectedRoots::new(vec![], vec![], None);
    let refused_launch = app.processes().launch(
        &ctx,
        LaunchRequest {
            spec: ProcessSpec::new(&["fake-tool".to_owned()]).unwrap(),
            operation_id: None,
            lease: None,
            approved_hold: false,
        },
        &LaunchPolicy {
            fs: &fs,
            roots: &roots,
            protected_branches: &[],
        },
    );
    assert!(
        matches!(
            refused_launch,
            Err(devworks_core::services::process::LaunchRefusal::ProjectClosed)
        ),
        "{refused_launch:?}"
    );
    spawn_tree(&fx, app.processes(), "root-b", "project-b");
    assert_eq!(app.processes().owned().len(), 2);
    // The stop of the root whose tree was killed is executed and observed.
    let stop = fx.control("root-f").stop_operation_id.unwrap();
    rt.block_on(within("the root's stop is observed", async {
        loop {
            let state = read(&*fx.store, |s| s.operations().operation(&stop)).map(|o| o.state);
            if state == Some(OperationState::ObservedComplete) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }));
    rt.block_on(app.shutdown());
}

/// A launch whose intent committed before an archival, but whose tree registers only after the
/// archival snapshotted the live trees, is killed when it registers: while the project is
/// archived, and after a restore too (its archival epoch is over).
#[test]
fn a_launch_overlapping_an_archival_is_killed_when_it_registers() {
    let fx = Fx::new();
    let host = Arc::new(FakeHost::default());
    let (_sup, processes) = supervisor(&fx, host.clone(), None);
    // What the engine records once the archival's project-wide stop ran (no tree was live yet).
    let store = fx.store.clone();
    let stop_ran = move || {
        let _ = write(&*store, |tx| {
            let p = ProjectId(id("project-a"));
            let epoch = tx.projects().project(&p)?.expect("project").archive_epoch;
            tx.projects().finish_archive_stop(&p, epoch)
        });
    };
    let gateway_parts = (
        fx.store.clone(),
        fx.h.clock.clone(),
        fx.h.ids.clone(),
        fx.use_cases.clone(),
        fx.user.clone(),
    );
    let lifecycle = move |op: &str, key: &str, revision: &str| {
        let (store, clock, ids, cases, user) = gateway_parts.clone();
        let authority = HostAuthority {
            store: &*store,
            clock: &*clock,
            protected_branches: &[],
        };
        let gateway = Gateway {
            authority: &authority,
            store: &*store,
            ids: &*ids,
        };
        let frame = json!({"mode": "Command", "protocol": PROTOCOL, "requestKey": key,
            "body": {"kind": op, "projectId": "project-a", "expectedRevision": revision}});
        cases
            .command(&gateway, &user, op, frame.to_string().as_bytes())
            .expect("addressable")
    };
    let launch = |root: Option<&str>| {
        let ctx =
            fx.agent
                .clone()
                .with_lineage(Some(ProjectId(id("project-a"))), root.map(rid), None);
        let fs = NativeFilesystem::default();
        let roots = ProtectedRoots::new(vec![], vec![], None);
        processes.launch(
            &ctx,
            LaunchRequest {
                spec: ProcessSpec::new(&["fake-tool".to_owned()]).unwrap(),
                operation_id: None,
                lease: None,
                approved_hold: false,
            },
            &LaunchPolicy {
                fs: &fs,
                roots: &roots,
                protected_branches: &[],
            },
        )
    };
    // 1. The archival commits (and its project-wide stop runs) between the intent and the spawn.
    let (archive, ran) = (lifecycle.clone(), stop_ran.clone());
    *host.on_spawn.lock().unwrap() = Some(Box::new(move || {
        let _ = archive("projects.archive", "overlap-archive", "1");
        ran();
    }));
    let late = launch(None);
    assert!(
        matches!(
            late,
            Err(devworks_core::services::process::LaunchRefusal::ProjectClosed)
        ),
        "{late:?}"
    );
    assert!(processes.owned().is_empty(), "the late tree was killed");
    // 2. Restored meanwhile: the tree belongs to the archived epoch and is killed as well.
    let reply = lifecycle("projects.restore", "restore-1", "2");
    assert!(
        matches!(&reply, CommandReply::Recorded { receipt, .. }
        if matches!(receipt.decision, DomainDecision::Applied { .. })),
        "{reply:?}"
    );
    let (again, ran) = (lifecycle.clone(), stop_ran.clone());
    *host.on_spawn.lock().unwrap() = Some(Box::new(move || {
        let _ = again("projects.archive", "overlap-archive-2", "3");
        ran();
        let _ = again("projects.restore", "overlap-restore-2", "4");
    }));
    let late = launch(None);
    assert!(
        matches!(
            late,
            Err(devworks_core::services::process::LaunchRefusal::ProjectClosed)
        ),
        "{late:?}"
    );
    assert!(processes.owned().is_empty());
    // 3. Without an archival in between, a launch of the (restored, active) project runs.
    assert!(launch(None).is_ok());
    assert_eq!(processes.owned().len(), 1);
}

/// Once shutdown began, a pending archival stop starts no project-wide kill (its blocking stop of
/// a tree that will not stop would hold the trees shutdown stops within its one bound): the
/// supervision pass and the post-commit stop return at once and leave the stop pending for the
/// next boot.
#[test]
fn a_pending_archive_stop_starts_no_kill_once_shutdown_began() {
    let fx = Fx::new();
    let host = Arc::new(FakeHost::default());
    host.stubborn.store(true, Ordering::SeqCst);
    let (sup, processes) = supervisor(&fx, host, None);
    let mut sup = sup;
    if let Some(s) = Arc::get_mut(&mut sup) {
        s.stop_bound = Duration::from_secs(5);
    }
    spawn_owned(&fx, &processes, None, "project-a");
    let reply = fx.command(
        &fx.user,
        "projects.archive",
        archive_frame(&fx, "project-a", "1"),
    );
    assert!(
        matches!(&reply, CommandReply::Recorded { receipt, .. }
        if matches!(receipt.decision, DomainDecision::Applied { .. })),
        "{reply:?}"
    );
    let pending = || read(&*fx.store, |s| s.projects().pending_archive_stops(10));
    assert_eq!(pending().len(), 1);
    // Shutdown begins (the supervisor stops admitting work).
    sup.stop();
    let rt = runtime();
    let started = std::time::Instant::now();
    let tick = rt.block_on(sup.tick()).unwrap();
    let direct = rt
        .block_on(sup.stop_archived_project(&ProjectId(id("project-a"))))
        .unwrap();
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(2),
        "no stop of the stubborn tree was started: {took:?}"
    );
    assert!(tick.archived.is_empty() && direct.is_empty());
    assert_eq!(pending().len(), 1, "left for the next boot");
    assert_eq!(
        processes.owned().len(),
        1,
        "shutdown's own stop owns the tree"
    );
}

/// Two stop paths of one archival interleaved with a restore and a new launch: the path that
/// read the archival first finishes the stop; the project is restored and a tree launched; the
/// other path, holding the epoch it read before the restore, then runs. It must not kill the new
/// tree, now or after a second archival, and only the second archival's own stop reaches it. The
/// kill of an archival stop is bound to the trees of earlier epochs.
#[test]
fn a_stale_archive_stop_never_kills_work_of_a_later_epoch() {
    let fx = Fx::new();
    let host = Arc::new(FakeHost::default());
    let (sup, processes) = supervisor(&fx, host, None);
    let rt = runtime();
    let p = ProjectId(id("project-a"));
    let lifecycle = |op: &str, revision: &str| {
        let frame = json!({"mode": "Command", "protocol": PROTOCOL, "requestKey": fx.key(),
            "body": {"kind": op, "projectId": "project-a", "expectedRevision": revision}});
        let reply = fx.command(&fx.user, op, frame);
        assert!(
            matches!(&reply, CommandReply::Recorded { receipt, .. }
            if matches!(receipt.decision, DomainDecision::Applied { .. })),
            "{op}: {reply:?}"
        );
    };
    let epoch = || {
        read(&*fx.store, |s| {
            s.projects().project(&ProjectId(id("project-a")))
        })
        .unwrap()
        .archive_epoch
    };
    spawn_owned(&fx, &processes, None, "project-a");
    let old = processes.owned();
    lifecycle("projects.archive", "1");
    // Both paths (the post-commit task and a supervision pass) read archival epoch 1.
    let stale = epoch();
    assert_eq!(stale, 1);
    // The first finishes: the old tree is killed and the stop is no longer pending.
    rt.block_on(sup.stop_archived_epoch(&p, stale)).unwrap();
    assert!(processes.owned().is_empty(), "{old:?} killed");
    // Restored, and new work launched under epoch 1.
    lifecycle("projects.restore", "2");
    spawn_owned(&fx, &processes, None, "project-a");
    let new = processes.owned();
    assert_eq!(new.len(), 1);
    // The second path now runs with what it read: nothing is killed.
    let late = rt.block_on(sup.stop_archived_epoch(&p, stale)).unwrap();
    assert!(late.is_empty());
    assert_eq!(
        processes.owned(),
        new,
        "work started after the restore survives"
    );
    // Archived again (epoch 2): the stale stop of epoch 1 still kills nothing ...
    lifecycle("projects.archive", "3");
    assert_eq!(epoch(), 2);
    rt.block_on(sup.stop_archived_epoch(&p, stale)).unwrap();
    assert_eq!(processes.owned(), new);
    assert_eq!(
        read(&*fx.store, |s| s.projects().pending_archive_stops(10)),
        std::slice::from_ref(&p),
        "nor ends the second archival's pending stop"
    );
    // ... and the second archival's own stop does.
    rt.block_on(sup.stop_archived_project(&p)).unwrap();
    assert!(processes.owned().is_empty());
    assert!(read(&*fx.store, |s| s.projects().pending_archive_stops(10)).is_empty());
}

/// The project-wide kill of an archival of epoch E reaches exactly the project's trees launched
/// under an earlier epoch.
#[test]
fn an_epoch_bound_kill_reaches_only_earlier_epochs() {
    let fx = Fx::new();
    let host = Arc::new(FakeHost::default());
    let (_sup, processes) = supervisor(&fx, host, None);
    spawn_owned(&fx, &processes, None, "project-a");
    let earlier = processes.owned();
    write(&*fx.store, |tx| {
        let p = ProjectId(id("project-a"));
        let mut stored = tx.projects().project(&p)?.expect("project");
        let revision = stored.revision;
        stored.revision += 1;
        stored.archive_epoch = 1;
        tx.projects().update_project(&stored, revision)
    })
    .unwrap();
    spawn_owned(&fx, &processes, None, "project-a");
    spawn_owned(&fx, &processes, None, "project-b");
    let all = processes.owned();
    assert_eq!(all.len(), 3);
    let stopped = processes.stop(
        &devworks_core::services::process::StopScope::ProjectBefore(ProjectId(id("project-a")), 1),
        Duration::ZERO,
        Duration::from_secs(5),
    );
    let stopped: Vec<Id> = stopped.into_iter().map(|(id, _)| id).collect();
    assert_eq!(stopped, earlier);
    assert_eq!(processes.owned().len(), 2);
}

#[test]
fn an_archive_whose_kill_never_ran_stays_fenced_after_a_restart() {
    let fx = Fx::new();
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let running = fx.launch("supervisor-1");
    // Committed through the use case, then the process dies before any post-commit kill.
    let reply = fx.command(
        &fx.user,
        "projects.archive",
        archive_frame(&fx, "project-a", "1"),
    );
    let CommandReply::Recorded { receipt, .. } = &reply else {
        panic!("{reply:?}")
    };
    assert!(matches!(receipt.decision, DomainDecision::Applied { .. }));
    let (h, store) = fx.into_parts();
    drop(store);
    let store = h.open();
    let fx = Fx::over(h, store);
    let rt = runtime();
    let app = rt
        .block_on(boot(composition(&fx, None, Arc::new(FakePower::default()))))
        .unwrap_or_else(|e| panic!("boot: {e:?}"));
    // The earlier boot's activation is interrupted; the archived project's roots stay paused
    // and closed to work: no Start, no eligibility.
    let a = fx.activation(&running.id);
    assert_eq!(a.phase, ActivationPhase::Stopped);
    for r in ["root-f", "root-g", "root-i"] {
        assert_eq!(fx.control(r).desired, DesiredExecution::Paused, "{r}");
        let root = fx.root(r);
        let e = read(&*fx.store, |s| {
            fx.service
                .evaluate(s.runs(), &root, &Target::Root, ActivationPurpose::Resume)
        });
        assert!(!e.eligible, "{r}");
    }
    let (code, _) = refused(&fx.start(&fx.user, "root-g", 1, fx.control("root-g").revision));
    assert_eq!(code, "Conflict");
    // Run supervision runs the archival stop the crash interrupted; only then may the project
    // be restored.
    rt.block_on(within("the interrupted archival stop runs", async {
        while !read(&*fx.store, |s| s.projects().pending_archive_stops(10)).is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }));
    let restore = json!({"mode": "Command", "protocol": PROTOCOL, "requestKey": fx.key(),
        "body": {"kind": "projects.restore", "projectId": "project-a", "expectedRevision": "2"}});
    let reply = fx.command(&fx.user, "projects.restore", restore);
    assert!(
        matches!(&reply, CommandReply::Recorded { receipt, .. }
            if matches!(receipt.decision, DomainDecision::Applied { .. })),
        "{reply:?}"
    );
    rt.block_on(app.shutdown());
}

#[test]
fn archival_stops_every_root_of_the_project_immediately() {
    let fx = Fx::new();
    applied(&fx.start(&fx.user, "root-f", 1, 1));
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    applied(&fx.start(&fx.user, "root-b", 1, 1));
    let owned = fx.launch("supervisor-1");
    let host = Arc::new(FakeHost::default());
    let (sup, processes) = supervisor(&fx, host, None);
    spawn_tree(&fx, &processes, "root-f", "project-a");
    spawn_tree(&fx, &processes, "root-b", "project-b");
    let rt = runtime();
    let stopped = rt
        .block_on(sup.stop_project_now(&ProjectId(id("project-a"))))
        .unwrap();
    assert_eq!(stopped.len(), 3, "root-f, root-g and root-i");
    // The owned (launched) activation has not stopped yet: its root's stop stays pending.
    for (r, state) in &stopped {
        let want = if r == &owned.root_id {
            OperationState::Dispatching
        } else {
            OperationState::ObservedComplete
        };
        assert_eq!(*state, want, "{r:?}");
    }
    let (x, e) = (owned.id.clone(), owned.owner_epoch);
    fx.tx(move |svc, tx| svc.finish_activation(tx, &x, e, ActivationOutcome::Cancelled, None))
        .unwrap();
    let tick = rt.block_on(sup.tick()).unwrap();
    assert_eq!(
        tick.stops,
        vec![(owned.root_id.clone(), OperationState::ObservedComplete)]
    );
    for r in ["root-f", "root-g", "root-i"] {
        assert_eq!(fx.control(r).desired, DesiredExecution::Paused);
    }
    assert_eq!(fx.control("root-b").desired, DesiredExecution::Enabled);
    assert!(fx.live("root-b").is_some(), "other projects continue");
    assert_eq!(processes.owned().len(), 1);
}

// ---------------------------------------------------------------------------------------------
// CONTROL-06 / CONTROL-07 / CONTROL-11: categorized retries, waits, wake

#[test]
fn transient_failures_retry_with_bounded_backoff_then_escalate_once() {
    let fx = Fx::new();
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let max = fx.service.config().max_attempts;
    let mut delays = Vec::new();
    for attempt in 1..max {
        let a = fx.launch("supervisor-1");
        let (x, e) = (a.id.clone(), a.owner_epoch);
        let p = fx
            .tx(move |svc, tx| {
                svc.report_failure(
                    tx,
                    &x,
                    e,
                    ActivationFailure::Transient("connection lost".into()),
                )
            })
            .unwrap();
        assert_eq!((p.phase, p.attempts), (ActivationPhase::Pending, attempt));
        delays.push(p.ready_at_ms - fx.h.now());
        // Not claimable before its ready time.
        assert!(
            fx.tx(|svc, tx| svc.claim_activation(tx, &id("supervisor-1")))
                .is_none()
        );
        fx.h.advance(p.ready_at_ms - fx.h.now());
    }
    assert!(delays.windows(2).all(|w| w[0] < w[1]), "{delays:?}");
    let a = fx.launch("supervisor-1");
    let (x, e) = (a.id.clone(), a.owner_epoch);
    let failed = fx
        .tx(move |svc, tx| {
            svc.report_failure(
                tx,
                &x,
                e,
                ActivationFailure::Transient("connection lost".into()),
            )
        })
        .unwrap();
    assert_eq!(
        (failed.phase, failed.outcome),
        (ActivationPhase::Stopped, Some(ActivationOutcome::Failed))
    );
    let escalations: Vec<_> = fx
        .holds("root-i")
        .into_iter()
        .filter(|b| b.cause == HoldCause::Policy)
        .collect();
    assert_eq!(escalations.len(), 1);
    assert!(escalations[0].reason.contains("retries exhausted"));
    let seen = fx.inspection("root-i");
    assert_eq!(seen.condition, RunCondition::Blocked);
    assert!(seen.reason.unwrap().contains("Escalated"));
    assert_eq!(seen.activations[0].outcome, Some(ActivationOutcome::Failed));
    // A repeated report on the stopped activation is fenced: one actionable escalation.
    let (x, e) = (a.id.clone(), a.owner_epoch);
    assert!(
        fx.tx(move |svc, tx| svc.report_failure(
            tx,
            &x,
            e,
            ActivationFailure::Deterministic("again".into())
        ))
        .is_err()
    );
    // The root's pause state was never touched.
    assert_eq!(fx.control("root-i").desired, DesiredExecution::Enabled);
}

#[test]
fn rate_limits_wait_visibly_and_clear_automatically() {
    let fx = Fx::new();
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let a = fx.launch("supervisor-1");
    let until = fx.h.now() + 60_000;
    let (x, e) = (a.id.clone(), a.owner_epoch);
    fx.tx(move |svc, tx| {
        svc.report_failure(
            tx,
            &x,
            e,
            ActivationFailure::RateLimited {
                until_ms: until,
                detail: "429 from provider".into(),
            },
        )
    })
    .unwrap();
    let snap = fx.inspect(&fx.user, "root-i");
    assert_eq!(snap["blockers"]["items"][0]["cause"], "Capacity");
    let hold = fx.holds("root-i").pop().unwrap();
    assert!(hold.reason.contains("RateLimited"));
    let seen = fx.inspection("root-i");
    assert_eq!(seen.condition, RunCondition::RateLimited);
    assert!(seen.reason.unwrap().contains("429"));
    assert!(
        fx.tx(|svc, tx| svc.claim_activation(tx, &id("supervisor-1")))
            .is_none()
    );
    fx.h.advance(60_000);
    assert!(
        fx.tx(|svc, tx| svc.claim_activation(tx, &id("supervisor-1")))
            .is_some()
    );
    assert!(fx.holds("root-i").is_empty());
}

#[test]
fn wake_reasserts_running_leases_and_expiry_retries_without_manual_holds() {
    let fx = Fx::new();
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    applied(&fx.start(&fx.user, "root-b", 1, 1));
    let a = fx.launch("supervisor-1");
    let b = fx.launch("supervisor-2");
    // The machine slept past every lease.
    fx.h.advance(fx.service.config().lease_ms * 3);
    let n = fx.tx(|svc, tx| svc.reassert_leases(tx, &id("supervisor-1")));
    assert_eq!(n, 1);
    let woke = fx.activation(&a.id);
    assert_eq!(woke.phase, ActivationPhase::Running);
    assert!(woke.lease_until_ms.unwrap() > fx.h.now());
    // Without a visible wake (or for another owner), expiry is a transient retry.
    assert_eq!(fx.tx(|svc, tx| svc.recover_expired(tx)), 1);
    let retried = fx.activation(&b.id);
    assert_eq!(
        (retried.phase, retried.attempts),
        (ActivationPhase::Pending, 1)
    );
    for r in ["root-i", "root-b"] {
        assert_eq!(fx.control(r).desired, DesiredExecution::Enabled);
        assert!(fx.holds(r).is_empty(), "sleep/wake adds no manual hold");
    }
    // Wake never clears an existing pause either.
    applied(&fx.pause(&fx.user, "root-i"));
    fx.h.advance(fx.service.config().lease_ms * 3);
    fx.tx(|svc, tx| svc.reassert_leases(tx, &id("supervisor-1")));
    assert_eq!(fx.control("root-i").desired, DesiredExecution::Paused);
}

// ---------------------------------------------------------------------------------------------
// The dispatcher's root gate: queued work cannot bypass a pause

#[test]
fn root_scoped_deliveries_of_a_paused_root_wait_until_continue() {
    let fx = Fx::new();
    let handler = Recorder::ok();
    let mut c = consumer("root-work", handler.clone());
    c.eligibility = Some(Arc::new(RunGate::new(fx.service.clone(), None)));
    let rt = runtime();
    let dispatcher = dispatcher(&fx.h, fx.store.clone(), vec![c]);
    rt.block_on(dispatcher.recover()).unwrap();
    applied(&fx.pause(&fx.user, "root-i"));
    seed_delivery(&fx, "root-i");
    seed_delivery(&fx, "root-b");
    rt.block_on(dispatcher.run_until_idle()).unwrap();
    assert_eq!(
        handler.calls.load(Ordering::SeqCst),
        1,
        "only the enabled root's work ran"
    );
    let held = read(&*fx.store, |s| {
        s.runs().root_deliveries(&rid("root-i"), None, 10)
    });
    let e = read(&*fx.store, |s| s.queue().entry(&held[0])).unwrap();
    assert_eq!(
        (e.state, e.awaiting_eligibility),
        (QueueState::Reconciling, true)
    );
    applied(&fx.resume(&fx.user, "root-i"));
    dispatcher.wakeup().wake();
    rt.block_on(dispatcher.run_until_idle()).unwrap();
    assert_eq!(handler.calls.load(Ordering::SeqCst), 2);
}

// ---------------------------------------------------------------------------------------------
// Launch supervision, restart (CONTROL-10) and shutdown (CONTROL-08/09) through runtime::boot

/// A coordinator stand-in: runs until asked to stop, then returns at its "safe boundary".
#[derive(Default)]
struct Scripted {
    launched: Mutex<Vec<StoredActivation>>,
    /// Launch futures dropped (their task ended or was aborted).
    dropped: Arc<AtomicUsize>,
}
/// A coordinator that keeps working after a stop request until the test releases it (its
/// admitted work has not quiesced yet). Each launch has its own release gate and keeps the stop
/// signal it was given, so the test can see whether a stop reached it.
#[derive(Default)]
struct Gated {
    releases: Mutex<Vec<tokio::sync::watch::Sender<bool>>>,
    stops: Mutex<Vec<tokio::sync::watch::Receiver<bool>>>,
    dropped: Arc<AtomicUsize>,
}
impl Gated {
    fn release(&self, launch: usize) {
        self.releases.lock().unwrap()[launch].send_replace(true);
    }
    fn release_all(&self) {
        for r in self.releases.lock().unwrap().iter() {
            r.send_replace(true);
        }
    }
    fn stop_seen(&self, launch: usize) -> bool {
        *self.stops.lock().unwrap()[launch].borrow()
    }
    fn launches(&self) -> usize {
        self.releases.lock().unwrap().len()
    }
}
impl ActivationLauncher for Gated {
    fn launch(&self, launch: Launch) -> LaunchFuture {
        let (tx, mut release) = tokio::sync::watch::channel(false);
        self.releases.lock().unwrap().push(tx);
        self.stops.lock().unwrap().push(launch.stop);
        let guard = DropCount(self.dropped.clone());
        Box::pin(async move {
            let _guard = guard;
            while !*release.borrow() {
                if release.changed().await.is_err() {
                    break;
                }
            }
            LaunchEnd::Finished {
                outcome: ActivationOutcome::Cancelled,
                detail: None,
            }
        })
    }
}
struct DropCount(Arc<AtomicUsize>);
impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl ActivationLauncher for Scripted {
    fn launch(&self, launch: Launch) -> LaunchFuture {
        self.launched
            .lock()
            .unwrap()
            .push(launch.activation.clone());
        let mut stop = launch.stop;
        let guard = DropCount(self.dropped.clone());
        Box::pin(async move {
            let _guard = guard;
            while !*stop.borrow() {
                if stop.changed().await.is_err() {
                    break;
                }
            }
            LaunchEnd::Finished {
                outcome: ActivationOutcome::Cancelled,
                detail: Some("stopped at a safe boundary".into()),
            }
        })
    }
}

#[test]
fn the_supervisor_launches_signals_a_requested_stop_and_records_each_stage() {
    let fx = Fx::new();
    let launcher = Arc::new(Scripted::default());
    let (sup, _) = supervisor(&fx, Arc::new(FakeHost::default()), Some(launcher.clone()));
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let rt = runtime();
    let tick = rt.block_on(sup.tick()).unwrap();
    assert_eq!(tick.launched.len(), 1);
    let a = fx.activation(&tick.launched[0]);
    assert_eq!(a.phase, ActivationPhase::Running);
    applied(&fx.pause(&fx.user, "root-i"));
    rt.block_on(async {
        sup.tick().await.unwrap();
        within("the launch honours the stop", async {
            while !sup.launched().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await;
    });
    let a = fx.activation(&a.id);
    assert_eq!(
        (a.phase, a.outcome),
        (ActivationPhase::Stopped, Some(ActivationOutcome::Cancelled))
    );
    assert!(
        a.stop_requested_at_ms.is_some()
            && a.stop_acknowledged_at_ms.is_some()
            && a.stopped_at_ms.is_some()
    );
    // Continue relaunches with a new generation.
    applied(&fx.resume(&fx.user, "root-i"));
    let tick = rt.block_on(sup.tick()).unwrap();
    assert_eq!(fx.activation(&tick.launched[0]).generation, 2);
}

#[derive(Default)]
struct FakePower(Arc<AtomicUsize>);
struct Held(Arc<AtomicUsize>);
impl PowerAssertion for Held {
    fn status(&mut self) -> PowerRequest {
        PowerRequest::Effective { mechanism: "fake" }
    }
}
impl Drop for Held {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl PowerPort for FakePower {
    fn prevent_idle_sleep(&self) -> Result<Box<dyn PowerAssertion>, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Held(self.0.clone())))
    }
}

fn composition(
    fx: &Fx,
    launcher: Option<Arc<dyn ActivationLauncher>>,
    power: Arc<FakePower>,
) -> Composition {
    composition_with(
        fx,
        launcher,
        power,
        Arc::new(Authority),
        Arc::new(FakeHost::default()),
    )
}
fn composition_with(
    fx: &Fx,
    launcher: Option<Arc<dyn ActivationLauncher>>,
    power: Arc<FakePower>,
    authority: Arc<dyn devworks_core::ports::AuthorityPort>,
    host: Arc<dyn ProcessHost>,
) -> Composition {
    let h = &fx.h;
    let artifacts = Arc::new(ArtifactService::new(
        Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.with_extension("cache"),
        )),
        Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
        Arc::new(NativeFilesystem::new()),
        h.clock(),
        h.ids.clone(),
    ));
    let mut config = RuntimeConfig::new(id("dispatcher-1"));
    config.collect_interval = Duration::from_secs(3600);
    config.housekeeping_interval = Duration::from_secs(3600);
    config.shutdown_grace = Duration::from_secs(5);
    let mut runs = RunComposition::new(fx.service.clone());
    runs.launcher = launcher;
    Composition {
        store: fx.store.clone(),
        clock: h.clock(),
        ids: h.ids.clone(),
        authority,
        artifacts,
        resources: Arc::new(ResourceService::new(h.ids.clone(), h.clock())),
        process_host: host,
        runs,
        power,
        diagnostics: Arc::new(
            devworks_core::services::diagnostics::DiagnosticService::new(
                fx.store.clone(),
                h.ids.clone(),
                h.clock(),
                None,
            ),
        ),
        browser: None,
        use_cases: UseCases::new(),
        consumers: Vec::new(),
        config,
    }
}

#[test]
fn restart_holds_active_and_accepted_unlaunched_work_until_a_selective_continue() {
    let h = Harness::new();
    let store = h.open();
    seed(&h);
    let fx = Fx::over(h, store);
    // Boot 1: root-i running; root-f accepted but never launched; root-g never started.
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let running = fx.launch("supervisor-1");
    applied(&fx.start(&fx.user, "root-f", 1, 1));
    let unlaunched = fx.live("root-f").unwrap();
    assert_eq!(unlaunched.phase, ActivationPhase::Pending);
    // The process dies (no shutdown).
    let (h, store) = fx.into_parts();
    drop(store);
    // Boot 2 through the one boot entry point.
    let store = h.open();
    let fx = Fx::over(h, store);
    let rt = runtime();
    let app = rt
        .block_on(boot(composition(&fx, None, Arc::new(FakePower::default()))))
        .unwrap_or_else(|e| panic!("boot: {e:?}"));
    let mut held = app.report().runs.held.clone();
    held.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    assert_eq!(held, vec![rid("root-f"), rid("root-i")]);
    for x in [&running.id, &unlaunched.id] {
        let a = fx.activation(x);
        assert_eq!(
            (a.phase, a.outcome),
            (
                ActivationPhase::Stopped,
                Some(ActivationOutcome::Interrupted)
            )
        );
    }
    for r in ["root-f", "root-i"] {
        assert_eq!(fx.control(r).desired, DesiredExecution::Paused);
        assert!(
            fx.holds(r)
                .iter()
                .any(|b| b.cause == HoldCause::RestartContinue)
        );
        let (code, _) = refused(&fx.start(&fx.user, r, 1, fx.control(r).revision));
        assert_eq!(code, "ContinueRequired");
    }
    assert_eq!(fx.control("root-g").desired, DesiredExecution::Enabled);
    // Selective: Continue only the issue; the feature stays held.
    applied(&fx.resume(&fx.user, "root-i"));
    assert_eq!(
        fx.live("root-i").unwrap().purpose,
        ActivationPurpose::Resume
    );
    assert!(fx.live("root-f").is_none());
    assert_eq!(fx.control("root-f").desired, DesiredExecution::Paused);
    rt.block_on(app.shutdown());
}

#[test]
fn closing_the_app_pauses_persists_and_requires_continue_on_reopen() {
    let fx = Fx::new();
    let launcher = Arc::new(Scripted::default());
    let power = Arc::new(FakePower::default());
    let rt = runtime();
    let app = rt
        .block_on(boot(composition(
            &fx,
            Some(launcher.clone()),
            power.clone(),
        )))
        .unwrap_or_else(|e| panic!("boot: {e:?}"));
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    app.runs().notify();
    rt.block_on(within("the coordinator launches", async {
        while launcher.launched.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    }));
    let a = launcher.launched.lock().unwrap()[0].clone();
    rt.block_on(app.shutdown());
    let stopped = fx.activation(&a.id);
    assert_eq!(stopped.phase, ActivationPhase::Stopped);
    assert_eq!(stopped.stop_cause, Some(StopCause::Shutdown));
    assert!(stopped.stop_acknowledged_at_ms.is_some());
    assert!(
        fx.holds("root-i")
            .iter()
            .any(|b| b.cause == HoldCause::RestartContinue)
    );
    assert_eq!(
        power.0.load(Ordering::SeqCst),
        0,
        "closing ends the keep-awake request"
    );
    // Admission is fenced once shutdown began.
    let frame = json!({"protocol": PROTOCOL, "requestKey": "late", "command": {"kind": "runs.start",
        "rootId": "root-i", "expectedWorkRevision": "1", "expectedControlRevision": "1"}});
    let late = app
        .command(&fx.user, "runs.start", frame.to_string().as_bytes())
        .unwrap();
    assert!(matches!(late, CommandReply::Rejected { .. }));
    drop(app);
    drop(rt);
    let (h, store) = fx.into_parts();
    drop(store);
    let store = h.open();
    let fx = Fx::over(h, store);
    let rt = runtime();
    let app = rt
        .block_on(boot(composition(&fx, None, Arc::new(FakePower::default()))))
        .unwrap();
    assert!(
        app.report().runs.interrupted.is_empty(),
        "the graceful close already settled it"
    );
    assert_eq!(
        refused(&fx.start(&fx.user, "root-i", 1, fx.control("root-i").revision)).0,
        "ContinueRequired"
    );
    rt.block_on(app.shutdown());
}

// ---------------------------------------------------------------------------------------------
// Review fixes: relaunch bookkeeping, stop quiescence, atomic dispositions, stale owners, drop,
// resume replay

#[test]
fn a_relaunch_after_lease_expiry_keeps_its_own_launch_entry() {
    let fx = Fx::new();
    let launcher = Arc::new(Gated::default());
    let (sup, _) = supervisor(&fx, Arc::new(FakeHost::default()), Some(launcher.clone()));
    // The test clock jumps; no jump is a sleep/wake here.
    let mut sup = Arc::try_unwrap(sup).ok().expect("the only handle");
    sup.wake_jump_ms = i64::MAX;
    let sup = Arc::new(sup);
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let rt = runtime();
    let first = rt.block_on(sup.tick()).unwrap().launched;
    assert_eq!(first.len(), 1);
    let activation = first[0].clone();
    // The first launch is still alive but its lease expired: recovered, then claimed again.
    fx.h.advance(fx.service.config().lease_ms * 3);
    assert_eq!(rt.block_on(sup.tick()).unwrap().expired, 1);
    fx.h.advance(fx.service.config().backoff_ms(1));
    let second = rt.block_on(sup.tick()).unwrap().launched;
    assert_eq!(second, vec![activation.clone()]);
    assert_eq!(fx.activation(&activation).owner_epoch, 2);
    assert_eq!(launcher.launches(), 2);
    assert_eq!(sup.launches(), 2, "both claims are tracked while both run");
    // The old launch ends after the relaunch began and cleans up only its own entry.
    launcher.release(0);
    // (Its future is dropped as it returns; the task's own cleanup follows.)
    rt.block_on(within("the superseded launch ends and cleans up", async {
        while launcher.dropped.load(Ordering::SeqCst) < 1 || sup.launches() == 2 {
            tokio::task::yield_now().await;
        }
    }));
    assert_eq!(sup.launches(), 1, "the relaunch is still tracked");
    assert_eq!(sup.launched(), vec![activation.clone()]);
    // So a pause reaches the relaunched coordinator.
    applied(&fx.pause(&fx.user, "root-i"));
    rt.block_on(sup.tick()).unwrap();
    assert!(launcher.stop_seen(1), "the relaunch was asked to stop");
    launcher.release(1);
    rt.block_on(within(
        "the relaunch stops and records its outcome",
        async {
            while launcher.dropped.load(Ordering::SeqCst) < 2 || sup.launches() > 0 {
                tokio::task::yield_now().await;
            }
        },
    ));
    let a = fx.activation(&activation);
    assert_eq!(
        (a.phase, a.outcome, a.owner_epoch),
        (
            ActivationPhase::Stopped,
            Some(ActivationOutcome::Cancelled),
            2
        )
    );
}

#[test]
fn a_stop_stays_pending_until_the_launched_activation_quiesces() {
    let fx = Fx::new();
    // A coordinator still finishing admitted work: it ends only when the test releases it.
    let launcher = Arc::new(Gated::default());
    let (sup, _) = supervisor(&fx, Arc::new(FakeHost::default()), Some(launcher.clone()));
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let rt = runtime();
    let launched = rt.block_on(sup.tick()).unwrap().launched;
    let op = match decision(&fx.stop(&fx.user, "root-i")) {
        DomainDecision::Accepted { operation_id } => operation_id.clone(),
        other => panic!("stop not accepted: {other:?}"),
    };
    // No owned trees, but the coordinator is still stopping: not observed complete.
    let tick = rt.block_on(sup.tick()).unwrap();
    assert_eq!(
        tick.stops,
        vec![(rid("root-i"), OperationState::Dispatching)]
    );
    assert_eq!(fx.activation(&launched[0]).phase, ActivationPhase::Stopping);
    let pending = read(&*fx.store, |s| s.runs().pending_stops(None, 10));
    assert_eq!(pending.len(), 1);
    // Once it has stopped, the stop is observed.
    // Still pending on a later pass while the coordinator works.
    let tick = rt.block_on(sup.tick()).unwrap();
    assert_eq!(
        tick.stops,
        vec![(rid("root-i"), OperationState::Dispatching)]
    );
    launcher.release_all();
    rt.block_on(within("the launch stops and records its outcome", async {
        while launcher.dropped.load(Ordering::SeqCst) < 1 || sup.launches() > 0 {
            tokio::task::yield_now().await;
        }
    }));
    let tick = rt.block_on(sup.tick()).unwrap();
    assert_eq!(
        tick.stops,
        vec![(rid("root-i"), OperationState::ObservedComplete)]
    );
    let state = read(&*fx.store, |s| s.operations().operation(&op))
        .unwrap()
        .state;
    assert_eq!(state, OperationState::ObservedComplete);
}

#[test]
fn a_refused_disposition_supersedes_nothing() {
    let fx = Fx::new();
    applied(&fx.start(&fx.user, "root-f", 1, 1));
    let coord = fx.assign("root-f", "root-f", None, None);
    let w1 = fx.assign("root-f", "task-f1", Some(&coord.id), None);
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let foreign = fx.assign("root-i", "root-i", None, None);
    // Claim until the feature's activation is the one owned.
    let owner = id("supervisor-1");
    let a = fx.tx(move |svc, tx| {
        loop {
            let x = svc.claim_activation(tx, &owner)?.expect("claimable");
            let started = svc
                .start_activation(tx, &x.id, x.owner_epoch)?
                .expect("started");
            if started.root_id == RootId(Id::new("root-f").unwrap()) {
                return Ok(started);
            }
        }
    });
    let summary = applied(&fx.redirect(&fx.user, "root-f", "drop task 1"));
    let instruction = id(summary["id"].as_str().unwrap());
    let (x, e) = (a.id.clone(), a.owner_epoch);
    fx.tx(move |svc, tx| svc.acknowledge_instructions(tx, &x, e, 1))
        .unwrap();
    for bad in [AssignmentId(id("missing")), foreign.id.clone()] {
        let (x, i, w) = (a.id.clone(), instruction.clone(), w1.id.clone());
        let r = fx.tx(move |svc, tx| {
            svc.incorporate_instruction(
                tx,
                &x,
                e,
                &i,
                Disposition {
                    summary: "partial".into(),
                    superseded: vec![w, bad],
                    pending_plan_revision: None,
                },
            )
        });
        assert!(matches!(r, Err(Fenced::NotFound(_))), "{r:?}");
        let w = read(&*fx.store, |s| s.runs().assignment(&w1.id)).unwrap();
        assert_eq!(w.state, AssignmentState::Ready, "nothing was superseded");
        let f = read(&*fx.store, |s| s.runs().assignment(&foreign.id)).unwrap();
        assert_eq!(f.state, AssignmentState::Ready);
        let i = read(&*fx.store, |s| s.runs().instruction(&instruction)).unwrap();
        assert_eq!(i.stage, InstructionStage::Acknowledged);
    }
}

#[test]
fn results_of_an_interrupted_or_expired_owner_are_not_current() {
    let fx = Fx::new();
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let coord = fx.assign("root-i", "root-i", None, None);
    let a = fx.launch("supervisor-1");
    let claim = ResultClaim {
        assignment: coord.id.clone(),
        execution_id: a.id.clone(),
        owner_epoch: a.owner_epoch,
        plan_revision: None,
    };
    let accept = |fx: &Fx, c: ResultClaim| {
        fx.tx(move |svc, tx| {
            let host = tx.store().host_actor.clone();
            svc.accept_result(tx, &host, &c)
        })
    };
    assert_eq!(accept(&fx, claim.clone()), ResultAcceptance::Current);
    // Lease expiry: requeued, not yet claimed again; the old owner's result is stale.
    fx.h.advance(fx.service.config().lease_ms * 3);
    assert_eq!(fx.tx(|svc, tx| svc.recover_expired(tx)), 1);
    assert!(matches!(
        accept(&fx, claim.clone()),
        ResultAcceptance::Stale(_)
    ));
    // Restart: the interrupted activation's result is stale even with its old epoch.
    fx.h.advance(fx.service.config().backoff_ms(1));
    let b = fx.launch("supervisor-1");
    let claim = ResultClaim {
        owner_epoch: b.owner_epoch,
        ..claim
    };
    assert_eq!(accept(&fx, claim.clone()), ResultAcceptance::Current);
    let (h, store) = fx.into_parts();
    drop(store);
    let store = h.open();
    let fx = Fx::over(h, store);
    let report = fx.tx(|svc, tx| svc.recover_boot(tx, &NoLifecycleHooks));
    assert_eq!(report.interrupted, vec![b.id.clone()]);
    let waiting = read(&*fx.store, |s| s.runs().assignment(&coord.id)).unwrap();
    assert_eq!(waiting.execution_id.as_ref(), Some(&b.id));
    assert!(matches!(accept(&fx, claim), ResultAcceptance::Stale(_)));
    let retained = fx
        .events("root-i")
        .iter()
        .filter(|k| *k == "LateResultRetained")
        .count();
    assert!(retained >= 2, "{retained}");
}

#[test]
fn dropping_the_runtime_aborts_launches_and_releases_the_supervisor() {
    let fx = Fx::new();
    let launcher = Arc::new(Scripted::default());
    let rt = runtime();
    let app = rt
        .block_on(boot(composition(
            &fx,
            Some(launcher.clone()),
            Arc::new(FakePower::default()),
        )))
        .unwrap_or_else(|e| panic!("boot: {e:?}"));
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    app.runs().notify();
    rt.block_on(within("the coordinator launches", async {
        while launcher.launched.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    }));
    let weak = Arc::downgrade(app.runs());
    drop(app);
    rt.block_on(within("the supervisor is released", async {
        while weak.upgrade().is_some() || launcher.dropped.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    }));
    // Nothing was recorded for the aborted launch: the next boot interrupts and holds it.
    let a = launcher.launched.lock().unwrap()[0].clone();
    assert_eq!(fx.activation(&a.id).phase, ActivationPhase::Running);
}

#[test]
fn a_lost_acknowledgment_of_a_granted_continue_replays_for_agents_and_schedules() {
    let fx = Fx::new();
    for (who, ctx) in [("agent-a", fx.agent.clone()), ("sched-a", fx.sched.clone())] {
        applied(&fx.pause(&fx.user, "root-i"));
        let hold = fx.holds("root-i").pop().unwrap();
        let (u, holder, now) = (fx.user.clone(), ActorId(id(who)), fx.h.now());
        let binding = ResumeBinding {
            holds: vec![HoldRef {
                hold_id: devworks_core::protocol::HoldId(hold.id.clone()),
                revision: Revision::new(hold.revision).unwrap(),
            }],
            causes: vec![],
        };
        let grant = write(&*fx.store, move |tx| {
            devworks_core::authority::mint_resume_grant(
                tx,
                &u,
                holder,
                RootId(Id::new("root-i").unwrap()),
                binding,
                GrantBounds {
                    expires_at_ms: now + 60_000,
                    uses: 1,
                },
            )
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
        })
        .unwrap();
        let ctx = ctx.with_authority_source(grant.id.clone());
        let key = fx.key();
        let rev = fx.control("root-i").revision;
        let first = fx.resume_key(&ctx, &key, "root-i", rev, &[(&hold.id, hold.revision)]);
        assert!(
            matches!(decision(&first), DomainDecision::Applied { .. }),
            "{who}: {first:?}"
        );
        // The acknowledgment was lost: the same request replays its receipt.
        let again = fx.resume_key(&ctx, &key, "root-i", rev, &[(&hold.id, hold.revision)]);
        assert!(replayed(&again), "{who}: {again:?}");
        assert_eq!(
            serde_json::to_value(decision(&again)).unwrap(),
            serde_json::to_value(decision(&first)).unwrap()
        );
        // It cleared nothing else and spent no other use; a new request is not authorized.
        let spent = read(&*fx.store, |s| s.grants().grant(&grant.id)).unwrap();
        assert_eq!(spent.uses_remaining, Some(0));
        applied(&fx.pause(&fx.user, "root-i"));
        let fresh = fx.resume(&ctx, "root-i");
        assert!(!matches!(
            decision_or_rejected(&fresh),
            Some(DomainDecision::Applied { .. })
        ));
        assert_eq!(fx.control("root-i").desired, DesiredExecution::Paused);
        applied(&fx.resume(&fx.user, "root-i"));
    }
}

// ---------------------------------------------------------------------------------------------
// Review fixes, round 2: stop after continue, verified stop, late coordinator, feature-wide scope

#[test]
fn a_stop_after_continue_reapplies_the_stop_and_cancels_the_reactivation() {
    let fx = Fx::new();
    let launcher = Arc::new(Gated::default());
    let (sup, _) = supervisor(&fx, Arc::new(FakeHost::default()), Some(launcher.clone()));
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let rt = runtime();
    let launched = rt.block_on(sup.tick()).unwrap().launched;
    let op = match decision(&fx.stop(&fx.user, "root-i")) {
        DomainDecision::Accepted { operation_id } => operation_id.clone(),
        other => panic!("stop not accepted: {other:?}"),
    };
    // The coordinator is still stopping; the user continues, then stops again.
    applied(&fx.resume(&fx.user, "root-i"));
    let continued = fx.control("root-i");
    assert_eq!(continued.desired, DesiredExecution::Enabled);
    assert!(continued.reactivate, "a replacement waits for the stop");
    let again = fx.stop(&fx.user, "root-i");
    match decision(&again) {
        DomainDecision::Accepted { operation_id } => assert_eq!(operation_id, &op),
        other => panic!("the second stop was not reapplied: {other:?}"),
    }
    let stopped = fx.control("root-i");
    assert_eq!(stopped.desired, DesiredExecution::Paused);
    assert!(!stopped.reactivate);
    assert!(
        fx.holds("root-i")
            .iter()
            .any(|b| b.cause == HoldCause::HumanPause)
    );
    // The old coordinator finishes: no replacement is created, and the stop is observed.
    launcher.release_all();
    rt.block_on(within("the launch stops and records its outcome", async {
        while launcher.dropped.load(Ordering::SeqCst) < 1 || sup.launches() > 0 {
            tokio::task::yield_now().await;
        }
    }));
    assert_eq!(fx.activation(&launched[0]).phase, ActivationPhase::Stopped);
    assert!(fx.live("root-i").is_none(), "the latest Stop wins");
    let tick = rt.block_on(sup.tick()).unwrap();
    assert_eq!(
        tick.stops,
        vec![(rid("root-i"), OperationState::ObservedComplete)]
    );
}

#[test]
fn a_stop_verified_later_clears_only_its_own_reconciliation_hold() {
    let fx = Fx::new();
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let host = Arc::new(FakeHost::default());
    host.stubborn.store(true, Ordering::SeqCst);
    let (sup, processes) = supervisor(&fx, host.clone(), None);
    let mut sup = Arc::try_unwrap(sup).ok().expect("the only handle");
    sup.stop_grace = Duration::from_millis(20);
    sup.stop_bound = Duration::from_millis(100);
    let sup = Arc::new(sup);
    spawn_tree(&fx, &processes, "root-i", "project-a");
    let unrelated = fx.hold(
        "root-i",
        Default::default(),
        HoldCause::Dependency,
        "task-x",
    );
    let _ = fx.stop(&fx.user, "root-i");
    let rt = runtime();
    let tick = rt.block_on(sup.tick()).unwrap();
    assert_eq!(
        tick.stops,
        vec![(rid("root-i"), OperationState::OutcomeUnknown)]
    );
    assert!(
        fx.holds("root-i")
            .iter()
            .any(|b| b.cause == HoldCause::Reconciliation)
    );
    // The tree can be stopped after all: the next pass verifies it gone.
    host.stubborn.store(false, Ordering::SeqCst);
    let tick = rt.block_on(sup.tick()).unwrap();
    assert_eq!(
        tick.stops,
        vec![(rid("root-i"), OperationState::ObservedComplete)]
    );
    let causes: Vec<HoldCause> = fx.holds("root-i").iter().map(|b| b.cause).collect();
    assert!(
        !causes.contains(&HoldCause::Reconciliation),
        "the stop's own hold is cleared: {causes:?}"
    );
    assert!(causes.contains(&HoldCause::Dependency), "{causes:?}");
    assert!(fx.holds("root-i").iter().any(|b| b.id == unrelated.id));
}

#[test]
fn a_coordinator_registered_after_a_redirect_must_bind_it_first() {
    let fx = Fx::new();
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    // The redirect arrives before any coordinator exists.
    applied(&fx.redirect(&fx.user, "root-i", "use the new layout"));
    let a = fx.launch("supervisor-1");
    let coord = fx.assign("root-i", "root-i", None, None);
    assert_eq!(coord.instruction_revision, 0);
    let fenced = blocked(fx.dispatch(
        &a,
        Target::Assignment(coord.id.clone()),
        ActivationPurpose::Implementation,
    ));
    assert!(
        fenced.iter().any(|(_, why)| why.contains("redirect")),
        "{fenced:?}"
    );
    let (x, e) = (a.id.clone(), a.owner_epoch);
    let x2 = x.clone();
    fx.tx(move |svc, tx| svc.acknowledge_instructions(tx, &x2, e, 1))
        .unwrap();
    assert!(
        fx.dispatch(
            &a,
            Target::Assignment(coord.id.clone()),
            ActivationPurpose::Implementation
        )
        .is_ok()
    );
    // A replacement coordinator does not inherit the binding: it binds the redirect again.
    let host = ActorId(id("host"));
    let (cid, rev) = (
        coord.id.clone(),
        fx.tx({
            let c = coord.id.clone();
            move |_, tx| Ok(tx.runs().assignment(&c)?.unwrap().revision)
        }),
    );
    fx.tx(move |svc, tx| {
        svc.set_assignment_state(tx, &host, &cid, rev, AssignmentState::Completed, None)
    })
    .unwrap();
    let next = fx.assign("root-i", "root-i", None, None);
    assert!(
        fx.dispatch(
            &a,
            Target::Assignment(next.id.clone()),
            ActivationPurpose::Implementation
        )
        .is_err()
    );
    fx.tx(move |svc, tx| svc.acknowledge_instructions(tx, &x, e, 1))
        .unwrap();
    assert!(
        fx.dispatch(
            &a,
            Target::Assignment(next.id.clone()),
            ActivationPurpose::Implementation
        )
        .is_ok()
    );
}

#[test]
fn the_feature_wide_scope_has_no_key_and_named_keys_are_validated() {
    let fx = Fx::new();
    let f = rid("root-f");
    fx.scope.approve(&f, 1, &[("x", "dx")]);
    fx.scope.set_readiness(&f, None, Readiness::Missing);
    applied(&fx.start(&fx.user, "root-f", 1, 1));
    let a = fx.launch("supervisor-1");
    // The coordinator's feature-wide scope is the absent key, and its readiness gates it.
    let coord = fx.assign("root-f", "root-f", None, None);
    assert_eq!(coord.scope_key, None);
    let gate = blocked(fx.dispatch(
        &a,
        Target::Assignment(coord.id.clone()),
        ActivationPurpose::Implementation,
    ));
    assert!(
        gate.iter().any(|(_, why)| why.contains("readiness")),
        "{gate:?}"
    );
    fx.scope.set_readiness(
        &f,
        None,
        Readiness::Current {
            assessment_revision: 3,
        },
    );
    assert_eq!(
        fx.dispatch(
            &a,
            Target::Assignment(coord.id.clone()),
            ActivationPurpose::Implementation
        )
        .unwrap()
        .readiness_revision,
        Some(3)
    );
    // An empty key is refused as a request error, never reaching the store's constraint.
    let empty = NewRunAssignment {
        root: f.clone(),
        work: rid("task-f1"),
        parent: Some(coord.id.clone()),
        session: fx.session("root-f"),
        scope_key: Some(String::new()),
    };
    let r = fx.tx(move |svc, tx| {
        let host = tx.store().host_actor.clone();
        svc.register_assignment(tx, &host, empty)
    });
    assert!(matches!(r, Err(Fenced::Invalid(_))), "{r:?}");
    let named = fx.assign("root-f", "task-f1", Some(&coord.id), Some("x"));
    assert_eq!(named.scope_key.as_deref(), Some("x"));
    // The production scope as well: an approved feature plan has a feature-wide scope but no
    // empty-key unit, so the empty key is refused before the store.
    let c = rusqlite::Connection::open(fx.h.root.join("workspace.sqlite")).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c.execute_batch(
        "INSERT INTO actors VALUES('planner','human','Planner');
         INSERT INTO feature_plans(feature_id,revision,approved_plan_json,approved_by,approved_at) VALUES('root-g',2,'{}','planner',1);",
    )
    .unwrap();
    drop(c);
    applied(&fx.start(&fx.user, "root-g", 1, 1));
    let g_coord = NewRunAssignment {
        root: rid("root-g"),
        work: rid("root-g"),
        parent: None,
        session: fx.session("root-g"),
        scope_key: Some(String::new()),
    };
    let r = fx.tx(move |svc, tx| {
        let host = tx.store().host_actor.clone();
        svc.register_assignment(tx, &host, g_coord)
    });
    assert!(matches!(r, Err(Fenced::Invalid(_))), "{r:?}");
}

// ---------------------------------------------------------------------------------------------
// Review fixes, round 3: generation-fenced stops, plan-checked results, admission drain,
// rootless archival, archived Continue, no launch once shutdown begins

#[test]
fn a_pending_stop_never_stops_the_generation_a_continue_created() {
    let fx = Fx::new();
    let launcher = Arc::new(Gated::default());
    let host = Arc::new(FakeHost::default());
    let (sup, processes) = supervisor(&fx, host, Some(launcher.clone()));
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let rt = runtime();
    let first = rt.block_on(sup.tick()).unwrap().launched;
    spawn_tree(&fx, &processes, "root-i", "project-a");
    let op = match decision(&fx.stop(&fx.user, "root-i")) {
        DomainDecision::Accepted { operation_id } => operation_id.clone(),
        other => panic!("stop not accepted: {other:?}"),
    };
    // The stop's first pass stops generation 1's tree; generation 1 is still stopping.
    let tick = rt.block_on(sup.tick()).unwrap();
    assert_eq!(
        tick.stops,
        vec![(rid("root-i"), OperationState::Dispatching)]
    );
    assert!(processes.owned().is_empty());
    // Continue while it stops; when it finishes, generation 2 replaces it and starts its work.
    applied(&fx.resume(&fx.user, "root-i"));
    launcher.release_all();
    rt.block_on(within(
        "generation 1 stops and records its outcome",
        async {
            while launcher.dropped.load(Ordering::SeqCst) < 1 || sup.launches() > 0 {
                tokio::task::yield_now().await;
            }
        },
    ));
    assert_eq!(fx.activation(&first[0]).phase, ActivationPhase::Stopped);
    let second = fx.launch("supervisor-2");
    assert_eq!(second.generation, 2);
    spawn_tree(&fx, &processes, "root-i", "project-a");
    // The still-pending stop observes generation 1 only: generation 2 and its tree keep running.
    let tick = rt.block_on(sup.tick()).unwrap();
    assert_eq!(
        tick.stops,
        vec![(rid("root-i"), OperationState::ObservedComplete)]
    );
    assert_eq!(
        processes.owned().len(),
        1,
        "generation 2's tree is not stopped"
    );
    assert_eq!(fx.activation(&second.id).phase, ActivationPhase::Running);
    let state = read(&*fx.store, |s| s.operations().operation(&op))
        .unwrap()
        .state;
    assert_eq!(state, OperationState::ObservedComplete);
}

#[test]
fn a_result_after_a_plan_change_is_checked_against_the_active_plan() {
    let fx = Fx::new();
    let f = rid("root-f");
    fx.scope.approve(&f, 1, &[("x", "dx"), ("y", "dy")]);
    applied(&fx.start(&fx.user, "root-f", 1, 1));
    let coord = fx.assign("root-f", "root-f", None, None);
    let wx = fx.assign("root-f", "task-f1", Some(&coord.id), Some("x"));
    let wy = fx.assign("root-f", "task-f2", Some(&coord.id), Some("y"));
    let a = fx.launch("supervisor-1");
    let accept = |w: &AssignmentId, plan: i64| {
        let c = ResultClaim {
            assignment: w.clone(),
            execution_id: a.id.clone(),
            owner_epoch: a.owner_epoch,
            plan_revision: Some(plan),
        };
        fx.tx(move |svc, tx| {
            let host = tx.store().host_actor.clone();
            svc.accept_result(tx, &host, &c)
        })
    };
    assert_eq!(accept(&wx.id, 1), ResultAcceptance::Current);
    // A replacement is approved (x changed, y kept); no safe boundary has adopted it yet.
    fx.scope.approve(&f, 2, &[("x", "dx2"), ("y", "dy")]);
    assert!(matches!(accept(&wx.id, 1), ResultAcceptance::Stale(_)));
    assert!(matches!(accept(&wy.id, 1), ResultAcceptance::Stale(_)));
    assert_eq!(accept(&wy.id, 2), ResultAcceptance::Current);
}

/// Authority that holds a `runs.start` in authorization until the test releases it.
struct HeldAuthority {
    entered: Mutex<std::sync::mpsc::Sender<()>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}
impl devworks_core::ports::AuthorityPort for HeldAuthority {
    fn authorize(
        &self,
        ctx: &InvocationContext,
        request: &devworks_core::ports::AuthorityRequest<'_>,
    ) -> Result<devworks_core::ports::Authorization, devworks_core::ports::AuthorityDenial> {
        if request.descriptor.name == "runs.start" {
            let _ = self.entered.lock().unwrap().send(());
            let _ = self.release.lock().unwrap().recv();
        }
        Authority.authorize(ctx, request)
    }
}

#[test]
fn shutdown_drains_admitted_commands_before_the_quiescence_snapshot() {
    let fx = Fx::new();
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let authority = Arc::new(HeldAuthority {
        entered: Mutex::new(entered_tx),
        release: Mutex::new(release_rx),
    });
    let rt = runtime();
    let app = rt
        .block_on(boot(composition_with(
            &fx,
            None,
            Arc::new(FakePower::default()),
            authority,
            Arc::new(FakeHost::default()),
        )))
        .unwrap_or_else(|e| panic!("boot: {e:?}"));
    let frame = json!({"protocol": PROTOCOL, "requestKey": fx.key(), "command": {
        "kind": "runs.start", "rootId": "root-i", "expectedWorkRevision": "1",
        "expectedControlRevision": "1"}});
    let reply = std::thread::scope(|scope| {
        let (app, user) = (&app, &fx.user);
        let command = scope.spawn(move || {
            app.command(user, "runs.start", frame.to_string().as_bytes())
                .unwrap()
        });
        // The Start is admitted and waiting in authorization when shutdown begins.
        entered.recv().unwrap();
        // Released only once shutdown is waiting for it, or (without a drain) once the
        // quiescence snapshot was already taken.
        let releaser = scope.spawn(move || {
            loop {
                let stage = app.shutdown_stage();
                if stage == ShutdownStage::DrainingAdmission || stage >= ShutdownStage::StoppingWork
                {
                    break;
                }
                std::thread::yield_now();
            }
            release.send(()).unwrap();
        });
        rt.block_on(app.shutdown());
        releaser.join().unwrap();
        command.join().unwrap()
    });
    // It committed before the run-control snapshot, which therefore held its root.
    applied(&reply);
    assert!(
        fx.holds("root-i")
            .iter()
            .any(|b| b.cause == HoldCause::RestartContinue),
        "{:?}",
        fx.holds("root-i")
    );
    assert!(
        fx.live("root-i").is_none(),
        "nothing is left to run without a Continue"
    );
    assert_eq!(fx.control("root-i").desired, DesiredExecution::Paused);
    // After shutdown, admission is closed.
    let late = json!({"protocol": PROTOCOL, "requestKey": fx.key(), "command": {
        "kind": "runs.pause", "rootId": "root-g", "expectedControlRevision": "1"}});
    let refused = app
        .command(&fx.user, "runs.pause", late.to_string().as_bytes())
        .unwrap();
    assert!(matches!(refused, CommandReply::Rejected { .. }));
}

#[test]
fn archiving_a_project_without_roots_still_stops_its_processes() {
    let fx = Fx::new();
    let c = rusqlite::Connection::open(fx.h.root.join("workspace.sqlite")).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c.execute_batch(
        "INSERT INTO projects(id,title,lifecycle,revision,scope_revision,created_at) VALUES('project-c','C','Active',1,1,0);",
    )
    .unwrap();
    drop(c);
    let host = Arc::new(FakeHost::default());
    let (sup, processes) = supervisor(&fx, host, None);
    spawn_owned(&fx, &processes, None, "project-c");
    spawn_tree(&fx, &processes, "root-b", "project-b");
    let stopped = runtime()
        .block_on(sup.stop_project_now(&ProjectId(id("project-c"))))
        .unwrap();
    assert!(stopped.is_empty(), "the project has no roots");
    assert_eq!(
        processes.owned().len(),
        1,
        "only the other project's tree runs"
    );
}

#[test]
fn continue_is_refused_while_the_project_is_archived() {
    let fx = Fx::new();
    applied(&fx.pause(&fx.user, "root-i"));
    let c = rusqlite::Connection::open(fx.h.root.join("workspace.sqlite")).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c.execute_batch("UPDATE projects SET lifecycle='Archived' WHERE id='project-a';")
        .unwrap();
    drop(c);
    let (code, _) = refused(&fx.resume(&fx.user, "root-i"));
    assert_eq!(code, "Conflict");
    assert_eq!(fx.control("root-i").desired, DesiredExecution::Paused);
    assert!(fx.live("root-i").is_none());
}

/// A launcher whose first launch begins shutdown (the supervisor stops mid-tick).
#[derive(Default)]
struct StopsOnLaunch {
    supervisor: Mutex<Option<std::sync::Weak<RunSupervisor>>>,
    launched: AtomicUsize,
}
impl ActivationLauncher for StopsOnLaunch {
    fn launch(&self, launch: Launch) -> LaunchFuture {
        self.launched.fetch_add(1, Ordering::SeqCst);
        if let Some(s) = self
            .supervisor
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|w| w.upgrade())
        {
            s.stop();
        }
        let mut stop = launch.stop;
        Box::pin(async move {
            while !*stop.borrow() {
                if stop.changed().await.is_err() {
                    break;
                }
            }
            LaunchEnd::Finished {
                outcome: ActivationOutcome::Cancelled,
                detail: None,
            }
        })
    }
}

#[test]
fn no_further_activation_is_claimed_once_shutdown_begins_mid_tick() {
    let fx = Fx::new();
    let launcher = Arc::new(StopsOnLaunch::default());
    let (sup, _) = supervisor(&fx, Arc::new(FakeHost::default()), Some(launcher.clone()));
    *launcher.supervisor.lock().unwrap() = Some(Arc::downgrade(&sup));
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    applied(&fx.start(&fx.user, "root-b", 1, 1));
    let tick = runtime().block_on(sup.tick()).unwrap();
    assert_eq!(tick.launched.len(), 1);
    assert_eq!(launcher.launched.load(Ordering::SeqCst), 1);
    let pending = ["root-i", "root-b"]
        .iter()
        .filter(|r| {
            fx.live(r)
                .is_some_and(|a| a.phase == ActivationPhase::Pending)
        })
        .count();
    assert_eq!(pending, 1, "the second activation was not claimed");
}

// ---------------------------------------------------------------------------------------------
// Review fix, round 4: uncertain stop targets survive a Continue

#[test]
fn trees_a_stop_could_not_verify_stay_held_after_a_continue() {
    let fx = Fx::new();
    let launcher = Arc::new(Gated::default());
    let host = Arc::new(FakeHost::default());
    let (sup, processes) = supervisor(&fx, host.clone(), Some(launcher.clone()));
    let mut sup = Arc::try_unwrap(sup).ok().expect("the only handle");
    sup.stop_grace = Duration::from_millis(20);
    sup.stop_bound = Duration::from_millis(100);
    let sup = Arc::new(sup);
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let rt = runtime();
    let first = rt.block_on(sup.tick()).unwrap().launched;
    // Generation 1 runs a tree that does not stop when asked.
    host.stubborn.store(true, Ordering::SeqCst);
    spawn_tree(&fx, &processes, "root-i", "project-a");
    let old_tree = processes.owned()[0].clone();
    let op = match decision(&fx.stop(&fx.user, "root-i")) {
        DomainDecision::Accepted { operation_id } => operation_id.clone(),
        other => panic!("stop not accepted: {other:?}"),
    };
    // The stop cannot verify the tree while generation 1 is still stopping: it waits, and the
    // unverified tree is held for reconciliation already.
    let tick = rt.block_on(sup.tick()).unwrap();
    assert_eq!(
        tick.stops,
        vec![(rid("root-i"), OperationState::Dispatching)]
    );
    let hold = fx
        .holds("root-i")
        .into_iter()
        .find(|b| b.cause == HoldCause::Reconciliation)
        .expect("the unverified tree is held");
    assert_eq!(hold.origin_id, op.0.as_str());
    assert_eq!(hold.clearance["processes"], json!([old_tree.as_str()]));
    // Continue; generation 1 finishes; the stop is now superseded.
    applied(&fx.resume(&fx.user, "root-i"));
    launcher.release_all();
    rt.block_on(within(
        "generation 1 stops and records its outcome",
        async {
            while launcher.dropped.load(Ordering::SeqCst) < 1 || sup.launches() > 0 {
                tokio::task::yield_now().await;
            }
        },
    ));
    assert_eq!(fx.activation(&first[0]).phase, ActivationPhase::Stopped);
    // Settlement alone (no native report, as boot recovery or a project stop settles) still
    // requires the held tree reconciled.
    let (r, o) = (rid("root-i"), op.clone());
    let settled = fx.tx(move |svc, tx| svc.settle_stop(tx, &r, &o, &[]));
    assert_eq!(settled, OperationState::OutcomeUnknown);
    // The held tree is still unverified: the stop is not complete and resumed work does not run.
    let tick = rt.block_on(sup.tick()).unwrap();
    assert_eq!(
        tick.stops,
        vec![(rid("root-i"), OperationState::OutcomeUnknown)]
    );
    assert!(
        tick.launched.is_empty(),
        "resumed work waits for reconciliation"
    );
    assert!(
        fx.holds("root-i")
            .iter()
            .any(|b| b.cause == HoldCause::Reconciliation)
    );
    // Generation fencing still holds with the hold present: once the held tree can be stopped,
    // a superseded pass stops exactly it, by identity, and no other tree of the root.
    host.stubborn.store(false, Ordering::SeqCst);
    spawn_tree(&fx, &processes, "root-i", "project-a");
    let newer = processes
        .owned()
        .into_iter()
        .find(|p| p != &old_tree)
        .unwrap();
    let tick = rt.block_on(sup.tick()).unwrap();
    assert_eq!(
        tick.stops,
        vec![(rid("root-i"), OperationState::ObservedComplete)]
    );
    assert_eq!(
        processes.owned(),
        vec![newer],
        "only the held tree was stopped"
    );
    assert!(
        !fx.holds("root-i")
            .iter()
            .any(|b| b.cause == HoldCause::Reconciliation)
    );
    assert_eq!(
        tick.launched.len(),
        1,
        "the replacement runs once reconciled"
    );
    let state = read(&*fx.store, |s| s.operations().operation(&op))
        .unwrap()
        .state;
    assert_eq!(state, OperationState::ObservedComplete);
}

// ---------------------------------------------------------------------------------------------
// Review fixes, round 5: one stop pointer per root, fair pending-stop passes

#[test]
fn archival_reuses_an_unfinished_stop_instead_of_orphaning_it() {
    let fx = Fx::new();
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let host = Arc::new(FakeHost::default());
    host.stubborn.store(true, Ordering::SeqCst);
    let (sup, processes) = supervisor(&fx, host.clone(), None);
    let mut sup = Arc::try_unwrap(sup).ok().expect("the only handle");
    sup.stop_grace = Duration::from_millis(20);
    sup.stop_bound = Duration::from_millis(100);
    let sup = Arc::new(sup);
    spawn_tree(&fx, &processes, "root-i", "project-a");
    let op = match decision(&fx.stop(&fx.user, "root-i")) {
        DomainDecision::Accepted { operation_id } => operation_id.clone(),
        other => panic!("stop not accepted: {other:?}"),
    };
    let rt = runtime();
    let tick = rt.block_on(sup.tick()).unwrap();
    assert_eq!(
        tick.stops,
        vec![(rid("root-i"), OperationState::OutcomeUnknown)]
    );
    // A second Stop answers the unfinished one unchanged.
    let again = unchanged(&fx.stop(&fx.user, "root-i"));
    assert_eq!(again["operationId"], op.0.as_str());
    // Archival widens the unfinished stop rather than replacing it.
    rt.block_on(sup.stop_project_now(&ProjectId(id("project-a"))))
        .unwrap();
    assert_eq!(fx.control("root-i").stop_operation_id, Some(op.clone()));
    assert!(fx.events("root-i").iter().any(|k| k == "RootStopReapplied"));
    // Once the tree can be stopped, the one stop settles and its hold clears.
    host.stubborn.store(false, Ordering::SeqCst);
    let tick = rt.block_on(sup.tick()).unwrap();
    assert!(
        tick.stops
            .contains(&(rid("root-i"), OperationState::ObservedComplete)),
        "{:?}",
        tick.stops
    );
    let state = read(&*fx.store, |s| s.operations().operation(&op))
        .unwrap()
        .state;
    assert_eq!(state, OperationState::ObservedComplete);
    assert!(
        !fx.holds("root-i")
            .iter()
            .any(|b| b.cause == HoldCause::Reconciliation),
        "{:?}",
        fx.holds("root-i")
    );
    assert!(processes.owned().is_empty());
}

#[test]
fn pending_stops_beyond_one_batch_are_executed_in_rotation() {
    let fx = Fx::new();
    let (sup, _) = supervisor(&fx, Arc::new(FakeHost::default()), None);
    let mut sup = Arc::try_unwrap(sup).ok().expect("the only handle");
    sup.stop_batch = 2;
    let sup = Arc::new(sup);
    // Three roots whose stops stay pending (each covered activation is still owned).
    for r in ["root-b", "root-g", "root-i"] {
        applied(&fx.start(&fx.user, r, 1, 1));
        fx.launch("supervisor-x");
    }
    for r in ["root-b", "root-g", "root-i"] {
        assert!(matches!(
            decision(&fx.stop(&fx.user, r)),
            DomainDecision::Accepted { .. }
        ));
    }
    let rt = runtime();
    let roots = |tick: RunTick| -> Vec<RootId> { tick.stops.into_iter().map(|(r, _)| r).collect() };
    let first = roots(rt.block_on(sup.tick()).unwrap());
    let second = roots(rt.block_on(sup.tick()).unwrap());
    let third = roots(rt.block_on(sup.tick()).unwrap());
    assert_eq!(first, vec![rid("root-b"), rid("root-g")]);
    assert_eq!(second, vec![rid("root-i")], "the third root is not starved");
    assert_eq!(third, first, "the rotation wraps around");
}

// ---------------------------------------------------------------------------------------------
// Review fixes, round 6: supervision wakes the dispatcher, bounded shutdown, bounded hold
// reasons and per-root isolation

fn hold_with(
    fx: &Fx,
    root: &str,
    cause: HoldCause,
    origin: &str,
    reason: &str,
    clearance: Value,
) -> StoredBlocker {
    let (root, origin, reason) = (fx.root(root), origin.to_owned(), reason.to_owned());
    fx.tx(move |_, tx| {
        let host = tx.store().host_actor.clone();
        devworks_core::services::runs::raise_blocker(
            tx,
            &host,
            &root,
            Default::default(),
            cause,
            &origin,
            1,
            &reason,
            clearance,
        )
    })
}

#[test]
fn supervision_commits_that_unblock_a_root_wake_the_dispatcher() {
    let fx = Fx::new();
    let handler = Recorder::ok();
    let mut c = consumer("root-work", handler.clone());
    c.eligibility = Some(Arc::new(RunGate::new(fx.service.clone(), None)));
    let rt = runtime();
    let dispatcher = dispatcher(&fx.h, fx.store.clone(), vec![c]);
    rt.block_on(dispatcher.recover()).unwrap();
    let host = Arc::new(FakeHost::default());
    let (sup, processes) = supervisor(&fx, host.clone(), None);
    let mut sup = Arc::try_unwrap(sup).ok().expect("the only handle");
    sup.stop_grace = Duration::from_millis(20);
    sup.stop_bound = Duration::from_millis(100);
    let sup = Arc::new(sup);
    sup.set_dispatcher_wakeup(dispatcher.wakeup());
    // A rate-limit wait holds root-i's queued work until it expires.
    let until = fx.h.now() + 1_000;
    hold_with(
        &fx,
        "root-i",
        HoldCause::Capacity,
        "provider",
        "RateLimited: 429",
        json!({"kind": "RateLimited", "readyAtMs": until}),
    );
    seed_delivery(&fx, "root-i");
    rt.block_on(dispatcher.run_until_idle()).unwrap();
    assert_eq!(handler.calls.load(Ordering::SeqCst), 0);
    fx.h.advance(1_000);
    let tick = rt.block_on(sup.tick()).unwrap();
    assert_eq!(tick.swept, vec![rid("root-i")]);
    rt.block_on(dispatcher.run_until_idle()).unwrap();
    assert_eq!(
        handler.calls.load(Ordering::SeqCst),
        1,
        "the expired wait woke the dispatcher"
    );
    // A stop's reconciliation hold holds root-b's work until settlement verifies its tree.
    applied(&fx.start(&fx.user, "root-b", 1, 1));
    host.stubborn.store(true, Ordering::SeqCst);
    spawn_tree(&fx, &processes, "root-b", "project-b");
    assert!(matches!(
        decision(&fx.stop(&fx.user, "root-b")),
        DomainDecision::Accepted { .. }
    ));
    let tick = rt.block_on(sup.tick()).unwrap();
    assert_eq!(
        tick.stops,
        vec![(rid("root-b"), OperationState::OutcomeUnknown)]
    );
    applied(&fx.resume(&fx.user, "root-b"));
    seed_delivery(&fx, "root-b");
    rt.block_on(dispatcher.run_until_idle()).unwrap();
    assert_eq!(
        handler.calls.load(Ordering::SeqCst),
        1,
        "held by reconciliation"
    );
    host.stubborn.store(false, Ordering::SeqCst);
    let tick = rt.block_on(sup.tick()).unwrap();
    assert_eq!(
        tick.stops,
        vec![(rid("root-b"), OperationState::ObservedComplete)]
    );
    rt.block_on(dispatcher.run_until_idle()).unwrap();
    assert_eq!(
        handler.calls.load(Ordering::SeqCst),
        2,
        "the settled stop woke the dispatcher"
    );
}

#[test]
fn shutdown_respects_its_bound_while_root_stops_are_pending() {
    let fx = Fx::new();
    let host = Arc::new(FakeHost::default());
    host.stubborn.store(true, Ordering::SeqCst);
    let mut composition = composition_with(
        &fx,
        None,
        Arc::new(FakePower::default()),
        Arc::new(Authority),
        host.clone(),
    );
    let bound = Duration::from_secs(2);
    composition.config.shutdown_bound = bound;
    composition.config.shutdown_grace = Duration::from_millis(100);
    let rt = runtime();
    let app = rt
        .block_on(boot(composition))
        .unwrap_or_else(|e| panic!("boot: {e:?}"));
    let roots = [
        ("root-b", "project-b"),
        ("root-f", "project-a"),
        ("root-g", "project-a"),
        ("root-i", "project-a"),
    ];
    for (r, p) in roots {
        applied(&fx.start(&fx.user, r, 1, 1));
        spawn_tree(&fx, app.processes(), r, p);
    }
    for (r, _) in roots {
        assert!(matches!(
            decision(&fx.stop(&fx.user, r)),
            DomainDecision::Accepted { .. }
        ));
    }
    // Shutdown begins while the first root's stop is blocked on a tree that will not stop.
    app.runs().notify();
    let first = fx.control("root-b").stop_operation_id.unwrap();
    rt.block_on(within("the first root stop is executing", async {
        loop {
            let o = first.clone();
            let state = read(&*fx.store, move |s| s.operations().operation(&o))
                .unwrap()
                .state;
            if state != OperationState::IntentRecorded {
                break;
            }
            tokio::task::yield_now().await;
        }
    }));
    let started = std::time::Instant::now();
    rt.block_on(app.shutdown());
    let took = started.elapsed();
    assert!(
        took < bound + Duration::from_secs(1),
        "shutdown took {took:?} with a {bound:?} bound"
    );
}

#[test]
fn extending_a_large_reconciliation_hold_bounds_its_reason_and_keeps_every_identity() {
    let fx = Fx::new();
    let host = Arc::new(FakeHost::default());
    host.stubborn.store(true, Ordering::SeqCst);
    let (sup, processes) = supervisor(&fx, host, None);
    let mut sup = Arc::try_unwrap(sup).ok().expect("the only handle");
    sup.stop_grace = Duration::from_millis(20);
    sup.stop_bound = Duration::from_millis(100);
    let sup = Arc::new(sup);
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    spawn_tree(&fx, &processes, "root-i", "project-a");
    let real = processes.owned()[0].clone();
    let op = match decision(&fx.stop(&fx.user, "root-i")) {
        DomainDecision::Accepted { operation_id } => operation_id.clone(),
        other => panic!("stop not accepted: {other:?}"),
    };
    // The stop already holds many trees (its display reason is near its bound).
    let earlier: Vec<String> = (0..400)
        .map(|n| format!("process-earlier-{n:04}"))
        .collect();
    hold_with(
        &fx,
        "root-i",
        HoldCause::Reconciliation,
        op.0.as_str(),
        "Owned processes could not be verified stopped",
        json!({"operationId": op.0.as_str(), "processes": earlier}),
    );
    let tick = runtime().block_on(sup.tick()).unwrap();
    assert!(tick.failed.is_empty(), "{:?}", tick.failed);
    assert_eq!(
        tick.stops,
        vec![(rid("root-i"), OperationState::OutcomeUnknown)]
    );
    let hold = fx
        .holds("root-i")
        .into_iter()
        .find(|b| b.cause == HoldCause::Reconciliation)
        .unwrap();
    assert!(hold.reason.chars().count() <= 4096);
    let names = hold.clearance["processes"].as_array().unwrap();
    assert_eq!(names.len(), 401);
    assert!(names.iter().any(|n| n == real.as_str()));
}

#[test]
fn one_roots_failure_does_not_block_other_roots_stops_or_claims() {
    let fx = Fx::new();
    let (sup, processes) = supervisor(&fx, Arc::new(FakeHost::default()), None);
    // root-b's stop operation record is unreadable; root-i's is fine.
    for (r, p) in [("root-b", "project-b"), ("root-i", "project-a")] {
        applied(&fx.start(&fx.user, r, 1, 1));
        spawn_tree(&fx, &processes, r, p);
        assert!(matches!(
            decision(&fx.stop(&fx.user, r)),
            DomainDecision::Accepted { .. }
        ));
    }
    let broken = fx.control("root-b").stop_operation_id.unwrap();
    let c = rusqlite::Connection::open(fx.h.root.join("workspace.sqlite")).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c.execute(
        "UPDATE operations SET state='Unreadable' WHERE id=?1",
        [broken.0.as_str()],
    )
    .unwrap();
    let tick = runtime().block_on(sup.tick()).unwrap();
    assert_eq!(
        tick.stops,
        vec![(rid("root-i"), OperationState::ObservedComplete)]
    );
    assert_eq!(tick.failed.len(), 1);
    assert_eq!(tick.failed[0].0, rid("root-b"));
    // Claims too: root-g's record is unreadable, root-f's activation is still claimed.
    applied(&fx.start(&fx.user, "root-f", 1, 1));
    applied(&fx.start(&fx.user, "root-g", 1, 1));
    let bad = hold_with(
        &fx,
        "root-g",
        HoldCause::Dependency,
        "task-x",
        "fixture",
        json!({}),
    );
    c.execute(
        "UPDATE blockers SET cause='Unreadable' WHERE id=?1",
        [bad.id.as_str()],
    )
    .unwrap();
    drop(c);
    let owner = id("supervisor-1");
    let first = fx.tx({
        let owner = owner.clone();
        move |svc, tx| svc.claim_activation(tx, &owner)
    });
    assert_eq!(first.map(|a| a.root_id), Some(rid("root-f")));
    let second = fx.tx(move |svc, tx| svc.claim_activation(tx, &owner));
    assert!(
        second.is_none(),
        "the unreadable root is skipped, not fatal"
    );
}

#[test]
fn no_root_stop_is_admitted_once_shutdown_is_requested() {
    let fx = Fx::new();
    let (sup, processes) = supervisor(&fx, Arc::new(FakeHost::default()), None);
    for (r, p) in [("root-b", "project-b"), ("root-i", "project-a")] {
        applied(&fx.start(&fx.user, r, 1, 1));
        spawn_tree(&fx, &processes, r, p);
        assert!(matches!(
            decision(&fx.stop(&fx.user, r)),
            DomainDecision::Accepted { .. }
        ));
    }
    // Shutdown is requested: a pass executes no further root stop, leaving the trees to the
    // shared, bounded shutdown of owned trees and the stops to settle on the next boot.
    sup.stop();
    let tick = runtime().block_on(sup.tick()).unwrap();
    assert!(tick.stops.is_empty(), "{:?}", tick.stops);
    assert_eq!(processes.owned().len(), 2);
    let pending = read(&*fx.store, |s| s.runs().pending_stops(None, 10));
    assert_eq!(pending.len(), 2);
}
