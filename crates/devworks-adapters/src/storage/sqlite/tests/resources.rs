//! Resource reservations, owned processes and keep-awake end to end on the real SQLite store:
//! `resources.*` through the gateway with receipts and both schema revisions, canonical machine
//! resources raced by two projects, priority/FIFO queues, fencing and owner checks,
//! dispatch-time enforcement, logical expiry, physical transfer only after quiescence, boot,
//! pause, wake and shutdown reconciliation, session end, keep-awake through its setting and
//! migration 10. Nothing here spawns a process: the process supervisor's tests live in the
//! serialized `tests/processes.rs` binary. No inference.
use super::{Harness, T0, through};
use crate::storage::sqlite::{MIGRATION_0010_SHA256, MIGRATIONS, SqliteStore};
use devworks_core::authority::HostAuthority;
use devworks_core::authority::service as auth;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::gateway::Gateway;
use devworks_core::ports::process::{PowerAssertion, PowerPort, PowerRequest};
use devworks_core::ports::storage::contract::StoreHarness;
use devworks_core::ports::storage::*;
use devworks_core::protocol::records::{
    LeaseEnd, LeaseRecord, LeaseState, ResourcePriority, ResourceRecord, ResourceScope,
    SettingAssignment,
};
use devworks_core::protocol::results::{
    DomainCommandReply, DomainDecision, DomainQueryReply, OperationResult,
};
use devworks_core::protocol::support::Page;
use devworks_core::protocol::{
    ActorId, CommandReply, Counter, DomainErrorCode, DomainFault, Id, ProjectId, Revision, RootId,
};
use devworks_core::services::UseCases;
use devworks_core::services::power::{KEEP_AWAKE_KEY, KeepAwake, KeepAwakeStatus};
use devworks_core::services::resources::{
    DESKTOP, DispatchFailure, DispatchRefusal, MachineResource, ReconcileCause, ResourceService,
    desktop, register_machine, required_resource,
};
use devworks_core::settings::catalog::{AgentRole, descriptor};
use devworks_core::settings::model;
use devworks_core::settings::view::Health;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn actor(s: &str) -> ActorId {
    ActorId(id(s))
}
fn project(s: &str) -> ProjectId {
    ProjectId(id(s))
}
fn root(s: &str) -> RootId {
    RootId(id(s))
}

struct Fx {
    h: Harness,
    store: Arc<SqliteStore>,
    svc: Arc<ResourceService>,
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
                     INSERT INTO work_items VALUES('root-a','project-a','feature',NULL,NULL,NULL,'Feature A',1,0,1,1,1,'{}');
                     INSERT INTO work_items VALUES('root-b','project-b','feature',NULL,NULL,NULL,'Feature B',1,0,1,1,1,'{}');",
                ))
            })
            .unwrap();
        fx.write(|tx| {
            for (a, k) in [
                ("user-a", PrincipalKind::Human),
                ("agent-a", PrincipalKind::Agent),
                ("agent-c", PrincipalKind::Agent),
                ("agent-b", PrincipalKind::Agent),
            ] {
                tx.actors().register(&actor(a), k, a)?;
            }
            Ok(())
        });
        let user = fx.ctx("user-a", PrincipalKind::Human, None);
        for (who, p) in [
            ("agent-a", "project-a"),
            ("agent-c", "project-a"),
            ("agent-b", "project-b"),
        ] {
            let by = user.clone();
            let binding = NewPrincipalBinding {
                actor: actor(who),
                kind: PrincipalKind::Agent,
                role: Some(AgentRole::Worker),
                project_id: Some(project(p)),
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
        let svc = Arc::new(ResourceService::new(h.ids.clone(), h.clock.clone()));
        let cases = UseCases::new().with_resources(svc.clone());
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
        Self::open(h, n)
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
    /// An agent of `p` working on that project's root in `session`.
    fn agent(&self, who: &str, p: &str, session: &str) -> InvocationContext {
        let r = if p == "project-a" { "root-a" } else { "root-b" };
        self.ctx(who, PrincipalKind::Agent, Some(p))
            .with_lineage(Some(project(p)), Some(root(r)), None)
            .with_session(id(session))
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
        op: &str,
        key: &str,
        mut body: Value,
        revision: Option<u32>,
    ) -> DomainCommandReply {
        body["kind"] = json!(op);
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
        let key = format!("key-{}", self.n.fetch_add(1, Ordering::SeqCst));
        self.command_keyed(ctx, op, &key, body, Some(2))
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
    fn acquire(&self, ctx: &InvocationContext, resource: &str, extra: Value) -> DomainCommandReply {
        let mut body = json!({"resourceId": resource, "expectedRevision": "1", "units": 1,
            "leaseMs": "60000"});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        self.command(ctx, "resources.acquire", body)
    }
    fn holders(&self, ctx: &InvocationContext, resource: &str) -> Page<LeaseRecord> {
        match result(self.query_at(
            ctx,
            "resources.holders",
            json!({"resourceId": resource, "page": {"limit": 50}}),
            Some(2),
        )) {
            OperationResult::ResourcesHolders { value } => value,
            other => panic!("{other:?}"),
        }
    }
    fn waiters(
        &self,
        ctx: &InvocationContext,
        resource: &str,
        limit: u32,
        cursor: Option<&str>,
    ) -> Page<LeaseRecord> {
        let mut page = json!({"limit": limit});
        if let Some(c) = cursor {
            page["cursor"] = json!(c);
        }
        match result(self.query_at(
            ctx,
            "resources.waiters",
            json!({"resourceId": resource, "page": page}),
            Some(2),
        )) {
            OperationResult::ResourcesWaiters { value } => value,
            other => panic!("{other:?}"),
        }
    }
    fn stored(&self, lease: &LeaseRecord) -> StoredLease {
        let l = lease.id.clone();
        self.read(move |r| r.resources().lease(&l)).unwrap()
    }
    fn register(&self, def: MachineResource) -> StoredResource {
        self.write(move |tx| register_machine(tx, &def))
    }
    fn reconcile(
        &self,
        cause: ReconcileCause,
    ) -> devworks_core::services::resources::ReconcileReport {
        let svc = self.svc.clone();
        self.write(move |tx| svc.reconcile(tx, cause))
    }
    fn dispatch(
        &self,
        ctx: &InvocationContext,
        resource: &str,
        lease: &LeaseRecord,
        fence: i64,
    ) -> Result<devworks_core::services::resources::DispatchTicket, DispatchFailure> {
        let (svc, ctx, resource, lease) = (
            self.svc.clone(),
            ctx.clone(),
            resource.to_owned(),
            lease.id.clone(),
        );
        self.write(move |tx| Ok(svc.authorize_dispatch(tx, &ctx, &resource, &lease, fence)))
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
}

fn recorded(reply: DomainCommandReply) -> (DomainDecision, bool) {
    match reply {
        CommandReply::Recorded {
            receipt, replayed, ..
        } => (receipt.decision, replayed),
        other => panic!("expected a recorded decision, got {other:?}"),
    }
}
fn lease(reply: DomainCommandReply) -> LeaseRecord {
    match recorded(reply).0 {
        DomainDecision::Applied { result } | DomainDecision::NoChange { result } => match result {
            OperationResult::ResourcesAcquire { value }
            | OperationResult::ResourcesRenew { value }
            | OperationResult::ResourcesRelease { value } => value,
            other => panic!("{other:?}"),
        },
        other => panic!("expected an applied decision, got {other:?}"),
    }
}
fn unchanged(reply: DomainCommandReply) -> LeaseRecord {
    match recorded(reply).0 {
        DomainDecision::NoChange {
            result:
                OperationResult::ResourcesAcquire { value }
                | OperationResult::ResourcesRelease { value },
        } => value,
        other => panic!("expected NoChange, got {other:?}"),
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
fn fault(reply: DomainQueryReply) -> DomainFault {
    match reply {
        DomainQueryReply::Rejected { fault, .. } => fault,
        other => panic!("expected a fault, got {other:?}"),
    }
}
fn st(l: &LeaseRecord) -> &'static str {
    match l.state {
        LeaseState::Waiting => "Waiting",
        LeaseState::Held => "Held",
        LeaseState::Releasing => "Releasing",
        LeaseState::Uncertain => "Uncertain",
        LeaseState::Released => "Released",
    }
}
fn fence(l: &LeaseRecord) -> i64 {
    l.fence.get()
}
fn release(fx: &Fx, ctx: &InvocationContext, l: &LeaseRecord, f: i64) -> DomainCommandReply {
    fx.command(
        ctx,
        "resources.release",
        json!({"leaseId": l.id.as_str(), "expectedFence": f.to_string()}),
    )
}

// ---------------------------------------------------------------------------------------------
// resources.* through the gateway

#[test]
fn acquire_creates_a_project_resource_and_grants_a_fenced_lease() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", "project-a", "session-a");
    let l = lease(fx.acquire(
        &a,
        "GPU-box",
        json!({"purpose": "Run the suite", "priority": "High",
        "capacity": 2, "labels": ["gpu", "cuda"]}),
    ));
    assert_eq!(st(&l), "Held");
    assert_eq!(fence(&l), 1);
    assert_eq!(
        l.resource_id.0.as_str(),
        "gpu-box",
        "names compare ignoring case"
    );
    assert_eq!(l.purpose.as_ref().unwrap().0, "Run the suite");
    assert!(matches!(l.priority, Some(ResourcePriority::High)));
    assert_eq!(l.project_id, Some(project("project-a")));
    assert_eq!(l.root_id, Some(root("root-a")));
    assert_eq!(l.session_id, Some(id("session-a")));
    assert_eq!(l.owner_id, actor("agent-a"));
    assert_eq!(l.granted_at_ms.as_ref().map(Counter::get), Some(T0));
    assert_eq!(l.expires_at_ms.get(), T0 + 60_000);
    let list = match result(fx.query_at(
        &a,
        "resources.list",
        json!({"scope": {"kind": "Project", "projectId": "project-a"}, "page": {"limit": 10}}),
        Some(2),
    )) {
        OperationResult::ResourcesList { value } => value,
        other => panic!("{other:?}"),
    };
    let r: &ResourceRecord = &list.items.0[0];
    assert_eq!(r.canonical_key.0, "project:project-a:gpu-box");
    assert!(matches!(r.scope, Some(ResourceScope::Project)));
    assert_eq!(r.capacity.get(), 2);
    assert_eq!(r.used.as_ref().map(Counter::get), Some(1));
    assert_eq!(r.available, Some(true));
    assert_eq!(r.physical, Some(false));
    assert_eq!(
        r.labels
            .as_ref()
            .unwrap()
            .0
            .iter()
            .map(|x| x.0.as_str())
            .collect::<Vec<_>>(),
        ["cuda", "gpu"]
    );
    assert_eq!(r.holders.0, vec![l.id.clone()]);
    assert_eq!(fx.events("ResourceCreated").len(), 1);
    assert_eq!(fx.events("LeaseGranted").len(), 1);
    // Another project neither sees nor addresses it.
    let b = fx.agent("agent-b", "project-b", "session-b");
    assert_eq!(
        fault(fx.query_at(
            &b,
            "resources.holders",
            json!({"resourceId": "gpu-box", "page": {"limit": 5}}),
            Some(2)
        ))
        .code,
        DomainErrorCode::NotFound
    );
    assert_eq!(
        refused(release(&fx, &b, &l, 1)).code,
        DomainErrorCode::NotFound
    );
    assert_eq!(
        fault(fx.query_at(
            &b,
            "resources.list",
            json!({"scope": {"kind": "Project", "projectId": "project-a"}, "page": {"limit": 10}}),
            Some(2)
        ))
        .code,
        DomainErrorCode::NotFound
    );
    assert_eq!(
        fault(fx.query_at(
            &b,
            "resources.list",
            json!({"scope": {"kind": "Workspace"}, "page": {"limit": 10}}),
            Some(2)
        ))
        .code,
        DomainErrorCode::PermissionDenied
    );
}

#[test]
fn waiters_are_granted_by_priority_then_first_come() {
    let fx = Fx::new();
    let holder = fx.agent("agent-a", "project-a", "s-holder");
    let first = lease(fx.acquire(&holder, "db", json!({})));
    let low = lease(fx.acquire(
        &fx.agent("agent-c", "project-a", "s-low"),
        "db",
        json!({"priority": "Low"}),
    ));
    let high1 = lease(fx.acquire(
        &fx.agent("agent-c", "project-a", "s-high1"),
        "db",
        json!({"priority": "High"}),
    ));
    let high2 = lease(fx.acquire(
        &fx.agent("agent-a", "project-a", "s-high2"),
        "db",
        json!({"priority": "High"}),
    ));
    assert!(
        [&low, &high1, &high2]
            .iter()
            .all(|l| st(l) == "Waiting" && fence(l) == 0)
    );
    assert_eq!(low.queue_position.as_ref().map(Counter::get), Some(1));
    assert_eq!(high1.queue_position.as_ref().map(Counter::get), Some(1));
    assert_eq!(high2.queue_position.as_ref().map(Counter::get), Some(2));
    // The waiter listing pages in grant order with exact positions.
    let page1 = fx.waiters(&holder, "db", 2, None);
    let order: Vec<_> = page1
        .items
        .0
        .iter()
        .map(|l| (l.id.clone(), l.queue_position.as_ref().unwrap().get()))
        .collect();
    assert_eq!(order, vec![(high1.id.clone(), 1), (high2.id.clone(), 2)]);
    let page2 = fx.waiters(
        &holder,
        "db",
        2,
        page1.next_cursor.as_ref().map(|c| c.0.as_str()),
    );
    assert_eq!(page2.items.0.len(), 1);
    assert_eq!(page2.items.0[0].id, low.id);
    assert_eq!(page2.items.0[0].queue_position.as_ref().unwrap().get(), 3);
    assert!(!page2.has_more);
    // A cursor is bound to its listing and principal.
    let other = fx.agent("agent-c", "project-a", "s-x");
    assert_eq!(fault(fx.query_at(&other, "resources.waiters",
        json!({"resourceId": "db", "page": {"limit": 2, "cursor": page1.next_cursor.unwrap().0}}), Some(2))).code,
        DomainErrorCode::CursorExpired);
    // Releasing the holder grants the oldest highest-priority waiter with the next fence.
    lease(release(&fx, &holder, &first, 1));
    let granted = fx.stored(&high1);
    assert_eq!((granted.phase, granted.fence), (LeasePhase::Held, 2));
    assert_eq!(fx.stored(&high2).phase, LeasePhase::Waiting);
    assert_eq!(fx.stored(&low).phase, LeasePhase::Waiting);
    let holders = fx.holders(&holder, "db");
    assert_eq!(
        holders
            .items
            .0
            .iter()
            .map(|l| l.id.clone())
            .collect::<Vec<_>>(),
        vec![high1.id.clone()]
    );
}

#[test]
fn same_holder_repeats_answer_its_lease_and_waiters_keep_their_place() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", "project-a", "s-a");
    let c = fx.agent("agent-c", "project-a", "s-c");
    let held = lease(fx.acquire(&a, "port-8080", json!({"leaseMs": "600000"})));
    assert_eq!(
        unchanged(fx.acquire(&a, "port-8080", json!({}))).id,
        held.id
    );
    let waiting = lease(fx.acquire(&c, "port-8080", json!({})));
    assert_eq!(waiting.expires_at_ms.get(), T0 + 120_000);
    fx.h.set_now(T0 + 100_000);
    let again = lease(fx.acquire(&c, "port-8080", json!({})));
    assert_eq!(again.id, waiting.id, "same waiter");
    assert_eq!(
        again.expires_at_ms.get(),
        T0 + 220_000,
        "its place is kept for another linger"
    );
    // A waiter that stops polling lapses and leaves the queue.
    fx.h.set_now(T0 + 230_000);
    let a2 = fx.agent("agent-a", "project-a", "s-other");
    lease(fx.acquire(&a2, "port-8080", json!({"leaseMs": "60000"})));
    let lapsed = fx.stored(&waiting);
    assert_eq!(
        (lapsed.phase, lapsed.end_reason),
        (LeasePhase::Released, Some(LeaseEndReason::Expired))
    );
}

#[test]
fn renew_and_release_need_the_owner_and_the_current_fence() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", "project-a", "s-a");
    let c = fx.agent("agent-c", "project-a", "s-c");
    let held = lease(fx.acquire(&a, "staging-db", json!({})));
    let renew = |ctx: &InvocationContext, f: &str, ms: &str| {
        fx.command(
            ctx,
            "resources.renew",
            json!({"leaseId": held.id.as_str(), "expectedFence": f, "leaseMs": ms}),
        )
    };
    assert_eq!(
        refused(renew(&a, "7", "1000")).code,
        DomainErrorCode::LeaseLost
    );
    assert_eq!(
        refused(renew(&c, "1", "1000")).code,
        DomainErrorCode::PermissionDenied
    );
    assert_eq!(
        refused(renew(&a, "1", "86400001")).code,
        DomainErrorCode::InvalidArgument
    );
    fx.h.set_now(T0 + 5_000);
    let renewed = lease(renew(&a, "1", "90000"));
    assert_eq!(
        (
            renewed.expires_at_ms.get(),
            renewed.renewed_at_ms.as_ref().map(Counter::get)
        ),
        (T0 + 95_000, Some(T0 + 5_000))
    );
    assert_eq!(fence(&renewed), 1, "a renewal keeps its fence");
    // Wrong owner and stale fence cannot release; the owner can, and an ended lease answers
    // unchanged.
    assert_eq!(
        refused(release(&fx, &c, &held, 1)).code,
        DomainErrorCode::PermissionDenied
    );
    assert_eq!(
        refused(release(&fx, &a, &held, 2)).code,
        DomainErrorCode::LeaseLost
    );
    let ended = lease(release(&fx, &a, &held, 1));
    assert_eq!(
        (
            st(&ended),
            ended.end_reason.clone().map(|e| format!("{e:?}"))
        ),
        ("Released", Some("Released".into()))
    );
    let again = unchanged(release(&fx, &a, &held, 1));
    assert_eq!(st(&again), "Released");
    // A renewal never grants again.
    assert_eq!(
        refused(renew(&a, "1", "1000")).code,
        DomainErrorCode::Conflict
    );
    // A waiter releases with fence 0 and leaves the queue.
    let blocker = lease(fx.acquire(&c, "staging-db", json!({})));
    let waiter = lease(fx.acquire(&a, "staging-db", json!({})));
    assert_eq!(st(&waiter), "Waiting");
    assert_eq!(
        refused(renew(&a, "0", "1000")).code,
        DomainErrorCode::LeaseLost,
        "renew names its own lease"
    );
    let left = lease(release(&fx, &a, &waiter, 0));
    assert!(matches!(left.end_reason, Some(LeaseEnd::Cancelled)));
    let _ = blocker;
}

#[test]
fn definition_conflicts_and_bounds_are_refused() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", "project-a", "s-a");
    lease(fx.acquire(&a, "runner", json!({"capacity": 3, "labels": ["linux"]})));
    let c = fx.agent("agent-c", "project-a", "s-c");
    assert_eq!(
        refused(fx.acquire(&c, "runner", json!({"capacity": 2}))).code,
        DomainErrorCode::Conflict
    );
    assert_eq!(
        refused(fx.acquire(&c, "runner", json!({"labels": ["mac"]}))).code,
        DomainErrorCode::Conflict
    );
    assert_eq!(
        refused(fx.acquire(&c, "runner", json!({"units": 4}))).code,
        DomainErrorCode::InvalidArgument
    );
    assert_eq!(
        refused(fx.acquire(&c, "runner", json!({"expectedRevision": "2"}))).code,
        DomainErrorCode::RevisionConflict
    );
    assert_eq!(
        refused(fx.acquire(&c, "runner", json!({"leaseMs": "86400001"}))).code,
        DomainErrorCode::InvalidArgument
    );
    assert_eq!(
        refused(fx.acquire(&c, "runner", json!({"purpose": "x".repeat(501)}))).code,
        DomainErrorCode::InvalidArgument
    );
    let labels: Vec<String> = (0..21).map(|i| format!("l{i}")).collect();
    assert_eq!(
        refused(fx.acquire(&c, "fresh", json!({"labels": labels}))).code,
        DomainErrorCode::InvalidArgument
    );
    assert_eq!(
        refused(fx.acquire(&c, "fresh", json!({"expectedRevision": "3"}))).code,
        DomainErrorCode::NotFound
    );
    // Units share capacity: three of three.
    lease(fx.acquire(&c, "runner", json!({"units": 2})));
    let third = lease(fx.acquire(
        &fx.agent("agent-a", "project-a", "s-x"),
        "runner",
        json!({"units": 1}),
    ));
    assert_eq!(st(&third), "Waiting");
    // A workspace invocation addresses machine resources only.
    let user = fx.ctx("user-a", PrincipalKind::Human, None);
    assert_eq!(
        refused(fx.acquire(&user, "runner", json!({}))).code,
        DomainErrorCode::NotFound
    );
}

#[test]
fn machine_resources_are_one_canonical_identity_enforced_at_dispatch() {
    let fx = Fx::new();
    fx.register(desktop());
    // Registration is idempotent.
    let r = fx.register(desktop());
    assert_eq!((r.revision, r.capacity, r.physical), (1, 1, true));
    assert_eq!(required_resource("desktop.input"), Some(DESKTOP));
    assert_eq!(required_resource("desktop.capture"), None);
    let a = fx.agent("agent-a", "project-a", "s-a");
    let b = fx.agent("agent-b", "project-b", "s-b");
    // Two projects race for the one desktop: one is granted, the other waits.
    let la = lease(fx.acquire(&a, "Desktop", json!({"purpose": "UI test"})));
    let lb = lease(fx.acquire(&b, "desktop", json!({"purpose": "Screenshot"})));
    assert_eq!((st(&la), st(&lb)), ("Held", "Waiting"));
    assert_eq!(
        fx.stored(&la).resource_id,
        fx.stored(&lb).resource_id,
        "one canonical resource"
    );
    // Both see its holder and waiter.
    assert_eq!(fx.holders(&b, "desktop").items.0[0].id, la.id);
    assert_eq!(fx.waiters(&a, "desktop", 10, None).items.0[0].id, lb.id);
    // Dispatch: only the holder, with its fence.
    let ticket = fx
        .dispatch(&a, "desktop", &la, 1)
        .expect("holder dispatches");
    assert!(matches!(
        fx.dispatch(&b, "desktop", &lb, 0),
        Err(DispatchFailure::Refused(DispatchRefusal::LeaseLost))
    ));
    assert!(matches!(
        fx.dispatch(&b, "desktop", &la, 1),
        Err(DispatchFailure::Refused(DispatchRefusal::NotOwner))
    ));
    assert!(matches!(
        fx.dispatch(&a, "desktop", &la, 2),
        Err(DispatchFailure::Refused(DispatchRefusal::LeaseLost))
    ));
    assert!(matches!(
        fx.dispatch(&a, "nothing", &la, 1),
        Err(DispatchFailure::Refused(DispatchRefusal::NotFound))
    ));
    assert_eq!(
        fx.svc.dispatches().in_flight(&la.id),
        1,
        "refused checks hold nothing"
    );
    drop(ticket);
    assert_eq!(fx.svc.dispatches().in_flight(&la.id), 0);
    // An expired lease does not dispatch.
    fx.h.set_now(T0 + 61_000);
    assert!(matches!(
        fx.dispatch(&a, "desktop", &la, 1),
        Err(DispatchFailure::Refused(
            DispatchRefusal::LeaseLost | DispatchRefusal::Expired
        ))
    ));
}

#[test]
fn logical_leases_expire_and_transfer() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", "project-a", "s-a");
    let c = fx.agent("agent-c", "project-a", "s-c");
    let held = lease(fx.acquire(&a, "fixture", json!({"leaseMs": "1000"})));
    let waiting = lease(fx.acquire(&c, "fixture", json!({})));
    fx.h.set_now(T0 + 1_500);
    // As of now the lapsed lease no longer holds, before any write.
    assert!(fx.holders(&c, "fixture").items.0.is_empty());
    // The waiter's poll settles the resource and is granted.
    let granted = lease(fx.acquire(&c, "fixture", json!({})));
    assert_eq!(
        (granted.id.clone(), st(&granted), fence(&granted)),
        (waiting.id, "Held", 2)
    );
    let old = fx.stored(&held);
    assert_eq!(
        (old.phase, old.end_reason),
        (LeasePhase::Released, Some(LeaseEndReason::Expired))
    );
    // The stale owner is refused.
    assert_eq!(
        refused(fx.command(
            &a,
            "resources.renew",
            json!({"leaseId": held.id.as_str(), "expectedFence": "1", "leaseMs": "1000"})
        ))
        .code,
        DomainErrorCode::Conflict
    );
    assert_eq!(fx.events("LeaseExpired").len(), 1);
}

#[test]
fn physical_leases_transfer_only_after_their_owner_is_quiescent() {
    let fx = Fx::new();
    fx.register(MachineResource {
        name: "gpu".into(),
        capacity: 1,
        physical: true,
        labels: vec![],
    });
    let a = fx.agent("agent-a", "project-a", "s-a");
    let b = fx.agent("agent-b", "project-b", "s-b");
    let la = lease(fx.acquire(&a, "gpu", json!({"leaseMs": "1000"})));
    let ticket = fx.dispatch(&a, "gpu", &la, 1).unwrap();
    let lb = lease(fx.acquire(&b, "gpu", json!({})));
    // Expired while a dispatch is still in flight: uncertain, never transferred.
    fx.h.set_now(T0 + 2_000);
    let report = fx.reconcile(ReconcileCause::Sweep);
    assert_eq!((report.uncertain, report.granted), (1, 0));
    let held = fx.holders(&b, "gpu").items.0;
    assert_eq!(held.len(), 1);
    assert_eq!(st(&held[0]), "Uncertain");
    assert_eq!(
        held[0]
            .owner_unreachable_since_ms
            .as_ref()
            .map(Counter::get),
        Some(T0 + 1_000)
    );
    assert_eq!(fx.stored(&lb).phase, LeasePhase::Waiting);
    // The owner's release cannot free it either while its dispatch runs.
    assert_eq!(st(&unchanged(release(&fx, &a, &la, 1))), "Uncertain");
    drop(ticket);
    let report = fx.reconcile(ReconcileCause::Sweep);
    assert_eq!((report.reconciled, report.granted), (1, 1));
    assert_eq!(fx.stored(&la).end_reason, Some(LeaseEndReason::Reconciled));
    let granted = fx.stored(&lb);
    assert_eq!((granted.phase, granted.fence), (LeasePhase::Held, 2));
    // A release while a dispatch runs waits for it (Releasing), then transfers.
    let ticket = fx.dispatch(&b, "gpu", &lb.clone(), 2).unwrap();
    assert_eq!(st(&lease(release(&fx, &b, &lb, 2))), "Releasing");
    drop(ticket);
    fx.reconcile(ReconcileCause::Sweep);
    assert_eq!(fx.stored(&lb).end_reason, Some(LeaseEndReason::Released));
}

#[test]
fn a_retried_request_replays_its_receipt_and_revision_one_omits_new_members() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", "project-a", "s-a");
    let body = json!({"resourceId": "cache", "expectedRevision": "1", "units": 1, "leaseMs": "60000", "purpose": "warm"});
    let first = lease(fx.command_keyed(&a, "resources.acquire", "retry-1", body.clone(), Some(2)));
    let (decision, replayed) =
        recorded(fx.command_keyed(&a, "resources.acquire", "retry-1", body, Some(2)));
    assert!(replayed);
    assert!(
        matches!(decision, DomainDecision::Applied { result: OperationResult::ResourcesAcquire { value } } if value.id == first.id)
    );
    let leases = fx.read(|r| r.resources().open_leases(&OpenLeaseFilter::All, None, 50));
    assert_eq!(leases.len(), 1, "no second lease");
    // Revision 1: no purpose accepted, none returned.
    let r1 =
        json!({"resourceId": "cache2", "expectedRevision": "1", "units": 1, "leaseMs": "60000"});
    let l1 = lease(fx.command_keyed(&a, "resources.acquire", "r1-1", r1, None));
    assert!(
        l1.purpose.is_none()
            && l1.priority.is_none()
            && l1.project_id.is_none()
            && l1.units.is_none()
    );
    let rejected = fx.command_keyed(&a, "resources.acquire", "r1-2",
        json!({"resourceId": "cache3", "expectedRevision": "1", "units": 1, "leaseMs": "1", "purpose": "p"}), None);
    assert!(
        matches!(rejected, CommandReply::Rejected { .. }),
        "{rejected:?}"
    );
    match result(fx.query_at(
        &a,
        "resources.holders",
        json!({"resourceId": "cache", "page": {"limit": 5}}),
        None,
    )) {
        OperationResult::ResourcesHolders { value } => {
            assert!(value.items.0[0].purpose.is_none() && value.items.0[0].session_id.is_none());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn boot_recovery_reconciles_leases_of_earlier_boots() {
    let fx = Fx::new();
    fx.register(MachineResource {
        name: "gpu".into(),
        capacity: 1,
        physical: true,
        labels: vec![],
    });
    let a = fx.agent("agent-a", "project-a", "s-a");
    let b = fx.agent("agent-b", "project-b", "s-b");
    let logical = lease(fx.acquire(&a, "suite", json!({})));
    let physical = lease(fx.acquire(&a, "gpu", json!({})));
    let waiting = lease(fx.acquire(&b, "gpu", json!({})));
    let fx = fx.restart();
    let report = fx.reconcile(ReconcileCause::Boot);
    assert_eq!(
        (
            report.cancelled,
            report.uncertain,
            report.unreachable,
            report.reconciled
        ),
        (1, 1, 1, 1)
    );
    // A logical lease survives its owner's absence, marked unreachable (CodeGraph quirk).
    let l = fx.stored(&logical);
    assert_eq!(
        (l.phase, l.unreachable_since_ms),
        (LeasePhase::Held, Some(T0))
    );
    // The physical lease had nothing still running from the earlier boot: reconciled.
    assert_eq!(
        fx.stored(&physical).end_reason,
        Some(LeaseEndReason::Reconciled)
    );
    assert_eq!(
        fx.stored(&waiting).end_reason,
        Some(LeaseEndReason::Cancelled)
    );
    let pass = fx.events("ResourcesReconciled");
    assert_eq!(pass.last().unwrap().payload["cause"], "Boot");
    // The owner seen again from this boot is reachable again.
    let a = fx.agent("agent-a", "project-a", "s-a");
    let back = lease(fx.acquire(&a, "suite", json!({})));
    assert_eq!(back.id, logical.id);
    assert!(back.owner_unreachable_since_ms.is_none());
}

#[test]
fn pause_wake_and_shutdown_reconciliation_is_recorded() {
    let fx = Fx::new();
    fx.register(desktop());
    let a = fx.agent("agent-a", "project-a", "s-a");
    let held = lease(fx.acquire(&a, "suite", json!({"leaseMs": "1000"})));
    let desk = lease(fx.acquire(&a, "desktop", json!({})));
    let paused = fx.reconcile(ReconcileCause::Pause(root("root-a")));
    assert_eq!(paused.paused, 2);
    assert_eq!(
        fx.stored(&held).phase,
        LeasePhase::Held,
        "a pause retains reservations"
    );
    // Sleep: the lease expired while the machine slept.
    fx.h.set_now(T0 + 10_000);
    let woke = fx.reconcile(ReconcileCause::Wake);
    assert_eq!(woke.expired, 1);
    let shut = fx.reconcile(ReconcileCause::Shutdown);
    assert_eq!(
        (shut.uncertain, shut.reconciled),
        (1, 1),
        "nothing of this boot was still running"
    );
    assert_eq!(
        fx.stored(&desk).end_reason,
        Some(LeaseEndReason::Reconciled)
    );
    let causes: Vec<String> = fx
        .events("ResourcesReconciled")
        .iter()
        .map(|e| e.payload["cause"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(causes, ["Pause", "Wake", "Shutdown"]);
    assert_eq!(fx.events("LeaseOwnerPaused").len(), 2);
    // A sweep that changes nothing records nothing.
    fx.reconcile(ReconcileCause::Sweep);
    assert_eq!(fx.events("ResourcesReconciled").len(), 3);
}

#[test]
fn ending_a_session_frees_its_leases() {
    let fx = Fx::new();
    let a = fx.agent("agent-a", "project-a", "s-end");
    let c = fx.agent("agent-c", "project-a", "s-c");
    let held = lease(fx.acquire(&a, "suite", json!({})));
    let other = lease(fx.acquire(&a, "lab", json!({})));
    let blocker = lease(fx.acquire(&c, "queue", json!({})));
    let waiting = lease(fx.acquire(&a, "queue", json!({})));
    let next = lease(fx.acquire(&c, "suite", json!({})));
    let n = fx.write(|tx| {
        devworks_core::services::resources::end_session_leases(tx, &actor("agent-a"), &id("s-end"))
    });
    assert_eq!(n, 3);
    assert_eq!(fx.stored(&held).end_reason, Some(LeaseEndReason::Released));
    assert_eq!(fx.stored(&other).end_reason, Some(LeaseEndReason::Released));
    assert_eq!(
        fx.stored(&waiting).end_reason,
        Some(LeaseEndReason::Cancelled)
    );
    assert_eq!(
        fx.stored(&next).phase,
        LeasePhase::Held,
        "the next waiter is granted"
    );
    assert_eq!(fx.stored(&blocker).phase, LeasePhase::Held);
}

// ---------------------------------------------------------------------------------------------
// Keep-awake through the existing setting

#[derive(Default)]
struct FakePower {
    held: Arc<AtomicUsize>,
    refuse: AtomicBool,
}
struct FakeAssertion(Arc<AtomicUsize>);
impl PowerAssertion for FakeAssertion {
    fn status(&mut self) -> PowerRequest {
        PowerRequest::Effective { mechanism: "fake" }
    }
}
impl Drop for FakeAssertion {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl PowerPort for FakePower {
    fn prevent_idle_sleep(&self) -> Result<Box<dyn PowerAssertion>, String> {
        if self.refuse.load(Ordering::SeqCst) {
            return Err("no power facility".into());
        }
        self.held.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeAssertion(self.held.clone())))
    }
}

#[test]
fn keep_awake_follows_the_setting_and_reports_effective_or_unavailable() {
    let fx = Fx::new();
    let set = |on: bool| {
        fx.write(move |tx| {
            let current = model::current_layer(tx.configuration(), &ConfigScope::Workspace)?;
            let schema =
                serde_json::to_value(descriptor(KEEP_AWAKE_KEY).unwrap().schema_ref()).unwrap();
            let a: SettingAssignment = serde_json::from_value(
                json!({"key": KEEP_AWAKE_KEY, "value": {"schema": schema, "value": on}}),
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
        })
    };
    let port = Arc::new(FakePower::default());
    let keep = KeepAwake::new(port.clone());
    let refresh = || fx.read(|r| keep.refresh(r.configuration()));
    // Off by default: nothing requested.
    assert_eq!(refresh(), KeepAwakeStatus::Off);
    assert_eq!(keep.health(), Health::NotChecked);
    set(true);
    assert_eq!(refresh(), KeepAwakeStatus::Effective { mechanism: "fake" });
    assert_eq!(refresh(), KeepAwakeStatus::Effective { mechanism: "fake" });
    assert_eq!(port.held.load(Ordering::SeqCst), 1, "one request, held");
    assert_eq!(keep.health(), Health::Available);
    // Disabling releases it.
    set(false);
    assert_eq!(refresh(), KeepAwakeStatus::Off);
    assert_eq!(port.held.load(Ordering::SeqCst), 0);
    // Unavailable is reported with its reason, distinct from the configured value.
    port.refuse.store(true, Ordering::SeqCst);
    set(true);
    assert_eq!(
        refresh(),
        KeepAwakeStatus::Unavailable("no power facility".into())
    );
    assert_eq!(
        keep.health(),
        Health::Unavailable("no power facility".into())
    );
    // Closing the app (dropping the controller) ends a held request.
    port.refuse.store(false, Ordering::SeqCst);
    assert!(matches!(refresh(), KeepAwakeStatus::Effective { .. }));
    drop(keep);
    assert_eq!(port.held.load(Ordering::SeqCst), 0);
    let d = descriptor(KEEP_AWAKE_KEY).unwrap();
    assert_eq!(d.default_value(), json!(false));
}

// ---------------------------------------------------------------------------------------------
// Migration 10

#[test]
fn migration_10_adds_reservations_and_processes_to_a_version_9_store() {
    let m = MIGRATIONS
        .iter()
        .find(|m| m.version == 10)
        .expect("migration 10");
    assert_eq!(
        m.checksum(),
        MIGRATION_0010_SHA256,
        "an applied migration was edited"
    );
    assert_eq!((m.minimum_reader, m.minimum_writer), (8, 9));
    assert!(
        MIGRATIONS
            .iter()
            .filter(|e| e.version < 10)
            .all(|e| e.minimum_reader <= m.minimum_reader && e.minimum_writer <= m.minimum_writer)
    );
    assert!(!m.rebuilds_tables);
    let h = Harness::new();
    drop(SqliteStore::open_with(h.config(), &through(9)).unwrap());
    let c = h.raw();
    c.execute_batch("INSERT INTO projects VALUES('project-a','A','Active',1,1,1);")
        .unwrap();
    drop(c);
    let s = h.store();
    let report = s.compatibility().unwrap();
    assert_eq!(
        (
            report.store_schema_version,
            report.minimum_reader,
            report.minimum_writer
        ),
        (10, 8, 9)
    );
    let created = run_write(&s, WriteOptions::default(), |tx| {
        tx.resources().insert_resource(NewResource {
            id: Id::new("res-1").unwrap(),
            name: "gpu".into(),
            scope: ResourceScopeKind::Project,
            project_id: Some(project("project-a")),
            physical: false,
            capacity: 2,
            labels: vec!["a".into()],
        })
    })
    .unwrap();
    assert_eq!(created.canonical_key, "project:project-a:gpu");
    // The store-level invariants hold: a second open lease of one holder, and a reused fence,
    // are refused; a project resource names its project.
    let lease = |id: &str, holder: &str, fence: i64, phase: LeasePhase| StoredLease {
        id: Id::new(id).unwrap(),
        resource_id: Id::new("res-1").unwrap(),
        phase,
        end_reason: None,
        owner: actor("host"),
        holder_key: holder.into(),
        session_id: None,
        project_id: None,
        root_id: None,
        assignment_id: None,
        boot_id: s.info().boot_id.unwrap(),
        purpose: None,
        priority: 1,
        units: 1,
        lease_ms: 1000,
        queue_seq: 1,
        fence,
        requested_at_ms: 0,
        granted_at_ms: (fence > 0).then_some(0),
        renewed_at_ms: None,
        expires_at_ms: 1000,
        ended_at_ms: None,
        unreachable_since_ms: None,
        revision: 1,
    };
    let (l1, l2, l3) = (
        lease("l-1", "h-1", 1, LeasePhase::Held),
        lease("l-2", "h-1", 0, LeasePhase::Waiting),
        lease("l-3", "h-2", 1, LeasePhase::Held),
    );
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.resources().insert_lease(&l1)
    })
    .unwrap();
    assert!(matches!(
        run_write(&s, WriteOptions::default(), move |tx| tx
            .resources()
            .insert_lease(&l2)),
        Err(StorageError::Duplicate { .. })
    ));
    assert!(
        run_write(&s, WriteOptions::default(), move |tx| tx
            .resources()
            .insert_lease(&l3))
        .is_err(),
        "fence reuse"
    );
    assert!(
        run_write(&s, WriteOptions::default(), |tx| tx
            .resources()
            .insert_resource(NewResource {
                id: Id::new("res-2").unwrap(),
                name: "x".into(),
                scope: ResourceScopeKind::Project,
                project_id: None,
                physical: false,
                capacity: 1,
                labels: vec![],
            }))
        .is_err()
    );
    // A stale revision cannot overwrite a lease.
    let mut stale = lease("l-1", "h-1", 1, LeasePhase::Held);
    stale.revision = 3;
    assert!(matches!(
        run_write(&s, WriteOptions::default(), move |tx| tx
            .resources()
            .update_lease(&stale, 2)),
        Err(StorageError::LeaseLost { .. })
    ));
    drop(s);
    // An older build (version 9) still reads and writes this store: no floor moved.
    let old = SqliteStore::open_with(h.config(), &through(9)).unwrap();
    assert_eq!(old.info().access, AccessMode::ReadWrite);
}

#[test]
fn the_largest_wire_page_lists_within_the_storage_bound() {
    let fx = Fx::new();
    // 501 resources: one full page of 500 and one more.
    fx.write(|tx| {
        for i in 0..501 {
            tx.resources().insert_resource(NewResource {
                id: id(&format!("res-{i:03}")),
                name: format!("r{i:03}"),
                scope: ResourceScopeKind::Machine,
                project_id: None,
                physical: false,
                capacity: 1,
                labels: vec![],
            })?;
        }
        Ok(())
    });
    let user = fx.ctx("user-a", PrincipalKind::Human, None);
    let list = |cursor: Option<String>| {
        let mut page = json!({"limit": 500});
        if let Some(c) = cursor {
            page["cursor"] = json!(c);
        }
        match result(fx.query_at(
            &user,
            "resources.list",
            json!({"scope": {"kind": "Workspace"}, "page": page}),
            Some(2),
        )) {
            OperationResult::ResourcesList { value } => value,
            other => panic!("{other:?}"),
        }
    };
    let first = list(None);
    assert_eq!((first.items.0.len(), first.has_more), (500, true));
    let rest = list(first.next_cursor.map(|c| c.0));
    assert_eq!((rest.items.0.len(), rest.has_more), (1, false));
    assert_eq!(rest.items.0[0].id.0.as_str(), "r500");
    // Holders and waiters accept the largest page too.
    for op in ["resources.holders", "resources.waiters"] {
        let reply = fx.query_at(
            &user,
            op,
            json!({"resourceId": "r000", "page": {"limit": 500}}),
            Some(2),
        );
        assert!(
            matches!(reply, DomainQueryReply::Result { .. }),
            "{op}: {reply:?}"
        );
    }
}

/// A power facility whose requests can lapse and whose next request can be held at a gate.
#[derive(Default)]
struct GatedPower {
    held: Arc<AtomicUsize>,
    lapsed: Arc<AtomicBool>,
    #[allow(clippy::type_complexity)]
    gate: std::sync::Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}
struct GatedAssertion {
    held: Arc<AtomicUsize>,
    lapsed: Arc<AtomicBool>,
}
impl PowerAssertion for GatedAssertion {
    fn status(&mut self) -> PowerRequest {
        if self.lapsed.load(Ordering::SeqCst) {
            PowerRequest::Unavailable("lapsed".into())
        } else {
            PowerRequest::Effective { mechanism: "gated" }
        }
    }
}
impl Drop for GatedAssertion {
    fn drop(&mut self) {
        self.held.fetch_sub(1, Ordering::SeqCst);
    }
}
impl PowerPort for GatedPower {
    fn prevent_idle_sleep(&self) -> Result<Box<dyn PowerAssertion>, String> {
        if let Some((entered, release)) = self.gate.lock().unwrap().take() {
            entered.send(()).unwrap();
            release.recv().unwrap();
        }
        self.held.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(GatedAssertion {
            held: self.held.clone(),
            lapsed: self.lapsed.clone(),
        }))
    }
}

#[test]
fn reading_keep_awake_health_never_undoes_a_disable() {
    use std::sync::mpsc;
    let port = Arc::new(GatedPower::default());
    let keep = Arc::new(KeepAwake::new(port.clone()));
    assert!(matches!(
        keep.apply(true),
        KeepAwakeStatus::Effective { .. }
    ));
    assert_eq!(port.held.load(Ordering::SeqCst), 1);
    // The held request lapses; the next status check re-requests it and is held at the gate
    // inside the OS call.
    let (entered_tx, entered) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    *port.gate.lock().unwrap() = Some((entered_tx, release_rx));
    port.lapsed.store(true, Ordering::SeqCst);
    let reader = {
        let keep = keep.clone();
        std::thread::spawn(move || keep.health())
    };
    entered
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the status check re-requests the lapsed request");
    port.lapsed.store(false, Ordering::SeqCst);
    // The setting is turned off while that request is in flight.
    let (done_tx, done) = mpsc::channel();
    let disabler = {
        let keep = keep.clone();
        std::thread::spawn(move || {
            let s = keep.apply(false);
            done_tx.send(()).unwrap();
            s
        })
    };
    assert!(
        done.recv_timeout(std::time::Duration::from_millis(100))
            .is_err(),
        "the disable waits for the in-flight check instead of interleaving with it"
    );
    release.send(()).unwrap();
    reader.join().unwrap();
    assert_eq!(disabler.join().unwrap(), KeepAwakeStatus::Off);
    // The disable won: nothing is held, and reading health again requests nothing.
    assert_eq!(port.held.load(Ordering::SeqCst), 0);
    assert_eq!(keep.health(), Health::NotChecked);
    assert_eq!(keep.status(), KeepAwakeStatus::Off);
    assert_eq!(port.held.load(Ordering::SeqCst), 0);
}
