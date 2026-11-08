//! Owned process trees, the process supervisor and native keep-awake, with trivial OS commands
//! (shell sleeps; `cmd`/`ping` on Windows) on the real SQLite store. Every wait is bounded; no
//! inference.
//!
//! Every test that spawns a process lives in this binary and runs serialized: a child spawned
//! while another test thread holds open files can transiently inherit them (observed with the
//! data-root lock), so spawning tests never share a test process with concurrent store reopens.
#![allow(dead_code, clippy::result_large_err)]
use devworks_adapters::fs::NativeFilesystem;
use devworks_adapters::power::NativePower;
use devworks_adapters::process::NativeProcessHost;
use devworks_adapters::storage::sqlite::{SqliteConfig, SqliteStore};
use devworks_core::authority::HostAuthority;
use devworks_core::authority::destructive::ProtectedRoots;
use devworks_core::authority::service as auth;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::gateway::Gateway;
use devworks_core::ports::process::{Probe, ProcessHost, ProcessSpec, ProcessTree, TreeStatus};
use devworks_core::ports::storage::*;
use devworks_core::ports::{Clock, IdKind, IdSource};
use devworks_core::protocol::records::{LeaseRecord, LeaseState};
use devworks_core::protocol::results::{
    DomainCommandReply, DomainDecision, DomainQueryReply, OperationResult,
};
use devworks_core::protocol::support::Page;
use devworks_core::protocol::{
    ActorId, CommandReply, Counter, DomainFault, Id, ProjectId, Revision, RootId,
};
use devworks_core::services::UseCases;
use devworks_core::services::containers::runner::ContainerRunner;
use devworks_core::services::containers::service::ContainerService;
use devworks_core::services::containers::{
    backend::{Backend as ContainerBackend, CreatePlan, ownership_labels, ownership_matches},
    execution::{bound_spec, with_environment},
    inspection::{parse as parse_container_inspect, verified_lifecycle_target},
    probe::{self as container_probe, Availability as ContainerAvailability, HostPlatform},
};
use devworks_core::services::process::tools::{RegisteredExecutable, ToolBindings};
use devworks_core::services::process::{
    ConfirmRefusal, LaunchPolicy, LaunchRefusal, LaunchRequest, LeaseClaim, OwnedProcess,
    ProcessSupervisor, StopFailure, StopOutcome, StopScope, all_recorded,
};
use devworks_core::services::resources::{
    DispatchFailure, DispatchRefusal, MachineResource, ReconcileCause, ResourceService,
    register_machine,
};
use devworks_core::settings::catalog::AgentRole;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const T0: i64 = 1_700_000_000_000;

/// Serializes the tests of this binary (see the module documentation).
fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

struct TestClock(AtomicI64);
impl Clock for TestClock {
    fn now_ms(&self) -> Counter {
        Counter::new(self.0.load(Ordering::SeqCst)).unwrap()
    }
}
struct TestIds(AtomicU64);
impl IdSource for TestIds {
    fn next_id(&self, kind: IdKind) -> Id {
        let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        Id::new(format!("{kind:?}-{n}").to_lowercase()).unwrap()
    }
}

/// A temporary data root reopened across simulated process restarts.
struct Harness {
    root: PathBuf,
    clock: Arc<TestClock>,
    ids: Arc<TestIds>,
}
impl Harness {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "devworks-process-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&root);
        Self {
            root,
            clock: Arc::new(TestClock(AtomicI64::new(T0))),
            ids: Arc::new(TestIds(AtomicU64::new(0))),
        }
    }
    fn store(&self) -> SqliteStore {
        SqliteStore::open(SqliteConfig::new(
            self.root.clone(),
            self.clock.clone(),
            self.ids.clone(),
            ActorId(Id::new("host").unwrap()),
        ))
        .expect("open store")
    }
    fn set_now(&self, ms: i64) {
        self.clock.0.store(ms, Ordering::SeqCst);
    }
    fn connection(&self) -> rusqlite::Connection {
        let c = rusqlite::Connection::open(self.root.join("workspace.sqlite")).unwrap();
        c.busy_timeout(Duration::from_secs(5)).unwrap();
        c
    }
    /// Seeds rows the adapter has no port for yet (projects, work items, repositories).
    fn sql(&self, batch: &str) {
        self.connection().execute_batch(batch).unwrap();
    }
    fn sql_with(&self, statement: &str, value: &str) {
        self.connection()
            .execute(statement, rusqlite::params![value])
            .unwrap();
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A removed-on-drop scratch directory.
struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "devworks-process-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self, rel: &str) -> String {
        self.0.join(rel).to_string_lossy().into_owned()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `script` run by the platform shell.
fn shell(script: &str) -> ProcessSpec {
    let argv: Vec<String> = if cfg!(windows) {
        vec!["cmd".into(), "/C".into(), script.into()]
    } else {
        vec!["/bin/sh".into(), "-c".into(), script.into()]
    };
    ProcessSpec::new(&argv).unwrap()
}
/// A tree of a root and a descendant that each sleep for about `secs` seconds (on Windows a
/// `cmd` root waiting on its `ping` child, passed as plain words so no shell quoting applies).
fn sleeping_tree(secs: u32) -> ProcessSpec {
    if cfg!(windows) {
        let argv: Vec<String> = ["cmd", "/C", "ping", "-n", &secs.to_string(), "127.0.0.1"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        ProcessSpec::new(&argv).unwrap()
    } else {
        shell(&format!("sleep {secs} & sleep {secs}; wait"))
    }
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
        fx.h.sql(
            "INSERT INTO projects VALUES('project-a','A','Active',1,1,1);
             INSERT INTO projects VALUES('project-b','B','Active',1,1,1);
             INSERT INTO work_items VALUES('root-a','project-a','feature',NULL,NULL,NULL,'Feature A',1,0,1,1,1,'{}');
             INSERT INTO work_items VALUES('root-b','project-b','feature',NULL,NULL,NULL,'Feature B',1,0,1,1,1,'{}');",
        );
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
    fn supervisor(&self) -> ProcessSupervisor {
        ProcessSupervisor::new(
            self.store.clone(),
            Arc::new(NativeProcessHost),
            self.svc.clone(),
        )
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
// The native process host

fn until_empty(tree: &mut dyn ProcessTree, bound: Duration) -> Option<TreeStatus> {
    let start = Instant::now();
    while start.elapsed() < bound {
        let s = tree.poll().unwrap();
        if s.empty {
            return Some(s);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

#[test]
fn a_tree_that_finishes_reports_its_exit_code() {
    let _serial = serial();
    let mut tree = NativeProcessHost.spawn(&shell("exit 3")).unwrap();
    let status = until_empty(&mut *tree, Duration::from_secs(10)).expect("finished");
    assert_eq!(status.exit_code, Some(3));
    assert!(!tree.identity().birth.is_empty());
}

/// The private control channel (DW-047's DevTools pipe): what the host writes reaches the
/// child's read end, what the child writes comes back, and the channel ends with the child.
/// On Windows the inherited handle values are named on the command line (here read by
/// PowerShell's anonymous pipe client, as Chromium's `--remote-debugging-io-pipes` reads them).
#[test]
fn a_control_pipe_is_a_private_channel_to_the_child() {
    let _serial = serial();
    let mut spec = if cfg!(windows) {
        let script = "& { param($i, $o); $in = New-Object System.IO.Pipes.AnonymousPipeClientStream([System.IO.Pipes.PipeDirection]::In, [string]$i); $out = New-Object System.IO.Pipes.AnonymousPipeClientStream([System.IO.Pipes.PipeDirection]::Out, [string]$o); $line = (New-Object System.IO.StreamReader($in)).ReadLine(); $b = [System.Text.Encoding]::ASCII.GetBytes('got ' + $line); $out.Write($b, 0, $b.Length); $out.Flush(); $out.Dispose() }";
        ProcessSpec::new(&[
            "powershell".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script.into(),
        ])
        .unwrap()
    } else {
        shell("IFS= read -r line <&3; printf 'got %s' \"$line\" >&4")
    };
    spec.control_pipe = Some(devworks_core::ports::process::ControlPipe {
        windows_args: vec!["{read}".into(), "{write}".into()],
    });
    let mut tree = NativeProcessHost.spawn(&spec).unwrap();
    let (mut to_child, mut from_child) = tree.take_control().expect("a control channel");
    assert!(tree.take_control().is_none(), "handed out once");
    std::io::Write::write_all(&mut to_child, b"hello\n").unwrap();
    std::io::Write::flush(&mut to_child).unwrap();
    let mut reply = String::new();
    std::io::Read::read_to_string(&mut from_child, &mut reply).unwrap();
    assert_eq!(reply, "got hello");
    let status = until_empty(&mut *tree, Duration::from_secs(30)).expect("finished");
    assert_eq!(status.exit_code, Some(0));
    // A child without a control pipe gets none.
    let mut plain = NativeProcessHost.spawn(&shell("exit 0")).unwrap();
    assert!(plain.take_control().is_none());
    until_empty(&mut *plain, Duration::from_secs(10)).expect("finished");
}

#[test]
fn the_forced_stop_reaps_every_descendant() {
    let _serial = serial();
    #[cfg(unix)]
    {
        // The descendant writes its pid, so its end is checked independently of the group.
        let dir = std::env::temp_dir().join(format!("devworks-tree-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("child.pid");
        let script = format!("sleep 30 & echo $! > '{}'; sleep 30; wait", file.display());
        let mut tree = NativeProcessHost.spawn(&shell(&script)).unwrap();
        let start = Instant::now();
        let child: libc::pid_t = loop {
            if let Some(pid) = std::fs::read_to_string(&file)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                break pid;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "no child pid");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(!tree.poll().unwrap().empty);
        tree.kill().unwrap();
        let status = until_empty(&mut *tree, Duration::from_secs(5)).expect("tree emptied");
        assert_eq!(status.exit_code, None, "killed by a signal");
        // The descendant is gone too (collected by init once it died).
        let start = Instant::now();
        loop {
            // SAFETY: a signal-0 existence check. A zombie awaiting its reaper is dead too
            // (containers without an init that reaps keep zombies).
            let zombie = std::fs::read_to_string(format!("/proc/{child}/stat"))
                .ok()
                .and_then(|s| {
                    s.rsplit_once(')')
                        .map(|(_, rest)| rest.trim_start().starts_with('Z'))
                })
                .unwrap_or(false);
            let alive = unsafe { libc::kill(child, 0) } == 0 && !zombie;
            if !alive {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "descendant survived"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // Once empty, the group is never signalled again.
        tree.kill().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[cfg(windows)]
    {
        let mut tree = NativeProcessHost.spawn(&sleeping_tree(30)).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(!tree.poll().unwrap().empty);
        tree.kill().unwrap();
        until_empty(&mut *tree, Duration::from_secs(5)).expect("job emptied");
        assert_eq!(NativeProcessHost.probe(tree.identity()), Probe::Gone);
    }
}

#[cfg(unix)]
#[test]
fn a_cooperative_stop_lets_the_tree_exit_on_its_own() {
    let _serial = serial();
    let mut tree = NativeProcessHost
        .spawn(&shell("trap 'exit 0' TERM; while :; do sleep 0.05; done"))
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(tree.request_stop().unwrap());
    let status = until_empty(&mut *tree, Duration::from_secs(5)).expect("stopped");
    assert_eq!(status.exit_code, Some(0));
}

#[test]
fn an_orphaned_tree_is_verified_by_birth_and_terminated() {
    let _serial = serial();
    let tree = NativeProcessHost.spawn(&sleeping_tree(30)).unwrap();
    let identity = tree.identity().clone();
    // A crash: the owner disappears without stopping the tree.
    std::mem::forget(tree);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(NativeProcessHost.probe(&identity), Probe::Alive);
    // A reused pid (different birth) is never treated as the recorded process.
    let mut other = identity.clone();
    other.birth = different_birth(&identity.birth);
    other.group = None;
    assert_eq!(NativeProcessHost.probe(&other), Probe::Gone);
    assert_eq!(NativeProcessHost.terminate(&other), Probe::Gone);
    assert_eq!(
        NativeProcessHost.probe(&identity),
        Probe::Alive,
        "untouched"
    );
    let after = NativeProcessHost.terminate(&identity);
    assert_eq!(after, Probe::Gone, "{after:?}");
}

#[test]
fn a_missing_program_starts_nothing() {
    let _serial = serial();
    let spec = ProcessSpec::new(&["devworks-no-such-program-xyz".to_owned()]).unwrap();
    assert!(matches!(
        NativeProcessHost.spawn(&spec),
        Err(devworks_core::ports::process::ProcessError::Spawn(_))
    ));
}

#[test]
fn dropping_an_owned_tree_kills_it() {
    let _serial = serial();
    let tree = NativeProcessHost.spawn(&sleeping_tree(30)).unwrap();
    let identity = tree.identity().clone();
    std::thread::sleep(Duration::from_millis(100));
    drop(tree);
    let start = Instant::now();
    while NativeProcessHost.probe(&identity) == Probe::Alive {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "tree survived its owner"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn captured_output_is_readable() {
    let _serial = serial();
    let mut spec = shell("echo devworks-output");
    spec.capture_output = true;
    let mut tree = NativeProcessHost.spawn(&spec).unwrap();
    let (mut out, _err) = tree.take_output().expect("pipes");
    let mut text = String::new();
    std::io::Read::read_to_string(&mut out, &mut text).unwrap();
    assert!(text.contains("devworks-output"), "{text}");
    until_empty(&mut *tree, Duration::from_secs(10)).expect("finished");
    assert!(tree.take_output().is_none(), "taken once");
}

#[test]
fn keep_awake_holds_a_native_request_and_releases_it() {
    let _serial = serial();
    use devworks_core::ports::process::{PowerPort, PowerRequest};
    match NativePower.prevent_idle_sleep() {
        Ok(mut held) => {
            assert!(matches!(held.status(), PowerRequest::Effective { .. }));
            drop(held);
        }
        // Without a power facility (for example no logind on a Linux runner) the request is
        // reported unavailable with a reason, never as effective.
        Err(reason) => {
            assert!(!reason.is_empty());
            let linux = cfg!(all(unix, not(target_os = "macos")));
            assert!(
                linux,
                "macOS and Windows always offer the request: {reason}"
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The process supervisor

fn policy_parts() -> (NativeFilesystem, ProtectedRoots) {
    (
        NativeFilesystem::default(),
        ProtectedRoots::new(vec![], vec![], None),
    )
}

/// Holds a real native child after spawn, before the supervisor can register it.
struct DelayedRegistrationHost {
    spawned: std::sync::mpsc::Sender<()>,
    resume: Mutex<std::sync::mpsc::Receiver<()>>,
}
impl ProcessHost for DelayedRegistrationHost {
    fn platform(&self) -> &'static str {
        NativeProcessHost.platform()
    }
    fn environment(&self) -> Vec<(String, String)> {
        NativeProcessHost.environment()
    }
    fn spawn(
        &self,
        spec: &ProcessSpec,
    ) -> Result<Box<dyn ProcessTree>, devworks_core::ports::process::ProcessError> {
        let tree = NativeProcessHost.spawn(spec)?;
        self.spawned.send(()).expect("test receiver");
        self.resume
            .lock()
            .unwrap()
            .recv()
            .expect("resume registration");
        Ok(tree)
    }
    fn probe(&self, identity: &devworks_core::ports::process::NativeIdentity) -> Probe {
        NativeProcessHost.probe(identity)
    }
    fn terminate(&self, identity: &devworks_core::ports::process::NativeIdentity) -> Probe {
        NativeProcessHost.terminate(identity)
    }
}

#[test]
fn container_cli_runner_uses_owned_process_and_redacts_bounded_native_output() {
    let _serial = serial();
    let fx = Fx::new();
    let runner = ContainerRunner::new(
        Arc::new(fx.supervisor()),
        Arc::new(NativeFilesystem::default()),
        ProtectedRoots::new(Vec::new(), Vec::new(), None),
    );
    let mut spec = shell("echo private-token");
    spec.capture_output = true;
    let result = runner
        .run(
            &fx.agent("agent-a", "project-a", "s-a"),
            None,
            spec,
            Duration::from_secs(10),
            1024,
            &["private-token".into()],
        )
        .unwrap();
    assert_eq!(result.exit_code, Some(0));
    assert!(
        !result
            .stdout
            .windows(b"private-token".len())
            .any(|x| x == b"private-token")
    );
    assert!(result.stderr.is_empty());
    assert!(!result.truncated);
    assert_eq!(fx.events("ProcessReaped").len(), 1);
}

#[test]
#[ignore = "requires a separately supplied running Docker engine"]
fn native_docker_probe_uses_registered_executable_and_owned_processes() {
    let _serial = serial();
    let fx = Fx::new();
    let path = std::env::var_os("DEVWORKS_TEST_DOCKER_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/local/bin/docker"));
    let bindings = ToolBindings::new(
        [RegisteredExecutable {
            id: id("docker"),
            path,
        }],
        std::iter::empty(),
    )
    .unwrap();
    let runner = ContainerRunner::new(
        Arc::new(fx.supervisor()),
        Arc::new(NativeFilesystem::default()),
        ProtectedRoots::new(Vec::new(), Vec::new(), None),
    );
    let outcome = container_probe::probe(
        &runner,
        &bindings,
        &fx.agent("agent-a", "project-a", "s-a"),
        ContainerBackend::Docker,
        HostPlatform {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            macos_major: 0,
        },
    );
    assert_eq!(
        outcome.availability,
        ContainerAvailability::Healthy,
        "{outcome:?}"
    );
    assert!(outcome.version.unwrap().contains("Docker version"));
    assert_eq!(fx.events("ProcessReaped").len(), 2);
}

#[test]
#[ignore = "requires a separately supplied Docker engine with local alpine:latest image"]
fn native_docker_owned_lifecycle_uses_the_exact_planned_container() {
    let _serial = serial();
    let fx = Fx::new();
    let path = std::env::var_os("DEVWORKS_TEST_DOCKER_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/local/bin/docker"));
    let bindings = ToolBindings::new(
        [RegisteredExecutable {
            id: id("docker"),
            path: path.clone(),
        }],
        std::iter::empty(),
    )
    .unwrap();
    let runner = ContainerRunner::new(
        Arc::new(fx.supervisor()),
        Arc::new(NativeFilesystem::default()),
        ProtectedRoots::new(Vec::new(), Vec::new(), None),
    );
    let context = fx.agent("agent-a", "project-a", "s-a");
    let name = format!("dw-native-test-{}", std::process::id());
    struct Cleanup {
        path: PathBuf,
        name: String,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::process::Command::new(&self.path)
                .args(["rm", "-f", &self.name])
                .output();
        }
    }
    let _cleanup = Cleanup {
        path,
        name: name.clone(),
    };
    let backend = ContainerBackend::Docker;
    let run = |args| {
        let spec = bound_spec(&bindings, backend, args, None).unwrap();
        let result = runner
            .run(&context, None, spec, Duration::from_secs(10), 16_384, &[])
            .unwrap();
        assert_eq!(
            result.exit_code,
            Some(0),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        result
    };
    let labels = ownership_labels("operation-a", "project-a", "Agent");
    let command = ["sleep".to_owned(), "30".to_owned()];
    let environment = [("DW_TEST_VALUE".into(), "hello-native".into())];
    let create_args = backend
        .create(&CreatePlan {
            name: &name,
            image: "alpine:latest",
            labels: &labels,
            mounts: &[],
            ports: &[],
            environment: &environment,
            command: &command,
        })
        .unwrap();
    assert!(!create_args.iter().any(|arg| arg.contains("hello-native")));
    let spec = with_environment(
        bound_spec(&bindings, backend, create_args, None).unwrap(),
        backend,
        &environment,
    );
    let created = runner
        .run(&context, None, spec, Duration::from_secs(10), 16_384, &[])
        .unwrap();
    assert_eq!(created.exit_code, Some(0));
    run(backend.start(&name).unwrap());
    let output = run(backend
        .exec(&name, &["printenv".into(), "DW_TEST_VALUE".into()], &[])
        .unwrap());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "hello-native"
    );
    let inspected = run(backend.inspect(&name).unwrap());
    let observed = parse_container_inspect(backend, &inspected.stdout).unwrap();
    assert!(ownership_matches(
        &observed.labels,
        "operation-a",
        "project-a",
        "Agent"
    ));
    let native_id =
        verified_lifecycle_target(backend, &observed, "operation-a", "project-a", "Agent").unwrap();
    assert_eq!(native_id.len(), 64);
    run(backend.logs(&name).unwrap());
    run(backend.stop(&name).unwrap());
}

#[test]
#[ignore = "requires a separately supplied Docker engine with local alpine:latest image"]
fn native_docker_build_uses_artifact_bytes_on_stdin_and_registered_context() {
    let _serial = serial();
    let fx = Fx::new();
    let path = std::env::var_os("DEVWORKS_TEST_DOCKER_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/local/bin/docker"));
    let tag = format!("dw-native-build-{}:test", std::process::id());
    struct Cleanup {
        path: PathBuf,
        tag: String,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::process::Command::new(&self.path)
                .args(["image", "rm", &self.tag])
                .output();
        }
    }
    let _cleanup = Cleanup {
        path: path.clone(),
        tag: tag.clone(),
    };
    let scratch = Scratch::new("container-build");
    let root_path = scratch.0.canonicalize().unwrap();
    let context = root_path.join("context");
    std::fs::create_dir_all(&context).unwrap();
    let project = project("project-a");
    let bindings = ToolBindings::new(
        [RegisteredExecutable {
            id: id("docker"),
            path,
        }],
        [
            devworks_core::services::process::tools::RegisteredWorkingRoot {
                id: id("build-root"),
                project: Some(project.clone()),
                path: root_path,
            },
        ],
    )
    .unwrap();
    let resolved = bindings
        .resolve_registered_path(
            &devworks_core::protocol::records::RegisteredPath {
                root_id: id("build-root"),
                relative: devworks_core::protocol::support::RelativePath("context".into()),
            },
            Some(&project),
        )
        .unwrap();
    let runner = ContainerRunner::new(
        Arc::new(fx.supervisor()),
        Arc::new(NativeFilesystem::default()),
        ProtectedRoots::new(Vec::new(), Vec::new(), None),
    );
    let mut spec = bound_spec(
        &bindings,
        ContainerBackend::Docker,
        ContainerBackend::Docker
            .build("-", &tag, resolved.to_str().unwrap())
            .unwrap(),
        None,
    )
    .unwrap();
    spec.stdin = Some(b"FROM alpine:latest\nCMD [\"true\"]\n".to_vec());
    let result = runner
        .run(
            &fx.agent("agent-a", "project-a", "s-a"),
            None,
            spec,
            Duration::from_secs(60),
            65_536,
            &[],
        )
        .unwrap();
    assert_eq!(
        result.exit_code,
        Some(0),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn container_create_receipt_atomically_records_ownership_before_effect_delivery() {
    let _serial = serial();
    let mut fx = Fx::new();
    let runner = Arc::new(ContainerRunner::new(
        Arc::new(fx.supervisor()),
        Arc::new(NativeFilesystem::default()),
        ProtectedRoots::new(Vec::new(), Vec::new(), None),
    ));
    let service = Arc::new(ContainerService::new(
        fx.store.clone(),
        runner,
        ToolBindings::default(),
        HostPlatform {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            macos_major: 0,
        },
        fx.h.ids.clone(),
    ));
    fx.cases = fx.cases.with_containers(service);
    let context = fx.agent("agent-a", "project-a", "s-a");
    let reply = fx.command_keyed(
        &context,
        "containers.create",
        "container-create-1",
        json!({"spec":{"engineId":"docker","projectId":"project-a","image":"alpine:latest",
            "command":[],"mounts":[],"ports":[],"environment":[],"resourceIds":[]}}),
        Some(1),
    );
    let operation = match recorded(reply).0 {
        DomainDecision::Accepted { operation_id } => operation_id,
        other => panic!("expected durable create intent: {other:?}"),
    };
    let containers = fx.read(|snapshot| {
        snapshot
            .resources()
            .containers(&ContainerFilter::Project(project("project-a")), None, 10)
    });
    assert_eq!(containers.len(), 1);
    assert_eq!(containers[0].phase, ContainerPhase::Creating);
    assert_eq!(containers[0].create_operation_id, operation);
    assert_eq!(containers[0].owner_kind, "Agent");
    assert_eq!(containers[0].root_id, Some(root("root-a")));
    let deliveries = fx.read(|snapshot| {
        snapshot.queue().deliveries(
            &DeliveryFilter {
                consumer: Some("container_effects".into()),
                operation_id: Some(operation),
            },
            None,
            10,
        )
    });
    assert_eq!(deliveries.entries.len(), 1);
}

#[test]
fn cancellation_during_native_launch_stops_the_registered_tree() {
    let _serial = serial();
    let fx = Fx::new();
    let (spawned_tx, spawned_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let supervisor = ProcessSupervisor::new(
        fx.store.clone(),
        Arc::new(DelayedRegistrationHost {
            spawned: spawned_tx,
            resume: Mutex::new(resume_rx),
        }),
        fx.svc.clone(),
    );
    let (fs, roots) = policy_parts();
    let policy = LaunchPolicy {
        fs: &fs,
        roots: &roots,
        protected_branches: &[],
    };
    let actor = fx.agent("agent-a", "project-a", "s-a");
    std::thread::scope(|scope| {
        let launch = scope.spawn(|| {
            supervisor
                .launch(
                    &actor,
                    LaunchRequest {
                        spec: sleeping_tree(30),
                        operation_id: None,
                        lease: None,
                        approved_hold: false,
                    },
                    &policy,
                )
                .unwrap()
        });
        spawned_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("native child spawned");
        let records = fx.read(|r| r.resources().processes(&ProcessFilter::Unsettled, None, 10));
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].phase, ProcessPhase::Launching);
        let target = OwnedProcess {
            id: records[0].id.clone(),
            identity: devworks_core::ports::process::NativeIdentity {
                pid: 0,
                birth: String::new(),
                group: None,
            },
            lease: None,
        };
        assert!(matches!(
            supervisor.cancel(&target, Duration::from_millis(100), Duration::from_secs(5)),
            Ok(StopOutcome::Uncertain(_))
        ));
        resume_tx.send(()).unwrap();
        let owned = launch.join().unwrap();
        let result = supervisor.poll(&owned).unwrap();
        assert!(matches!(result, Some(StopOutcome::Reaped { .. })));
        let id = owned.id.clone();
        assert_eq!(
            fx.read(move |r| r.resources().process(&id)).unwrap().phase,
            ProcessPhase::Reaped
        );
    });
}

#[test]
fn generic_process_approval_cannot_bypass_owned_git_push_resolution() {
    let _serial = serial();
    let fx = Fx::new();
    let supervisor = fx.supervisor();
    let (fs, roots) = policy_parts();
    let policy = LaunchPolicy {
        fs: &fs,
        roots: &roots,
        protected_branches: &[],
    };
    let actor = fx.agent("agent-a", "project-a", "s-a");
    for approved in [false, true] {
        let spec = ProcessSpec::new(&[
            "git".into(),
            "push".into(),
            "--force".into(),
            "origin".into(),
            "abc:refs/heads/task-alias".into(),
        ])
        .unwrap();
        assert!(matches!(
            supervisor.launch(
                &actor,
                LaunchRequest {
                    spec,
                    operation_id: None,
                    lease: None,
                    approved_hold: approved
                },
                &policy
            ),
            Err(LaunchRefusal::GitPushOwnerRequired)
        ));
    }
    assert!(supervisor.owned().is_empty());
}

#[test]
fn launch_preflight_blocks_destructive_commands_before_anything_runs() {
    let _serial = serial();
    let fx = Fx::new();
    let sup = fx.supervisor();
    // The target is a registered primary repository in a scratch directory: refused on every
    // platform, and harmless even if a regression let it run.
    let scratch = Scratch::new("launch");
    let repo = scratch.path("repo");
    std::fs::create_dir_all(format!("{repo}/.git")).unwrap();
    fx.h.sql_with(
        "INSERT INTO repositories(id,canonical_location,revision) VALUES('repo-a',?1,1)",
        &repo,
    );
    let fs = NativeFilesystem::default();
    let roots = ProtectedRoots::read(&*fx.store, Vec::new(), None).unwrap();
    let policy = LaunchPolicy {
        fs: &fs,
        roots: &roots,
        protected_branches: &[],
    };
    let a = fx.agent("agent-a", "project-a", "s-a");
    for approved in [false, true] {
        let spec = ProcessSpec::new(&["rm".into(), "-rf".into(), repo.clone()]).unwrap();
        let refusal = sup
            .launch(
                &a,
                LaunchRequest {
                    spec,
                    operation_id: None,
                    lease: None,
                    approved_hold: approved,
                },
                &policy,
            )
            .unwrap_err();
        assert!(
            matches!(refusal, LaunchRefusal::Blocked(_)),
            "approval never unlocks a refusal: {refusal:?}"
        );
    }
    assert_eq!(fx.events("DestructiveOperationBlocked").len(), 2);
    let none = fx.read(|r| r.resources().processes(&ProcessFilter::Unsettled, None, 10));
    assert!(none.is_empty(), "nothing was recorded or started");
    // A missing program is recorded Failed with its attribution.
    let spec = ProcessSpec::new(&["devworks-no-such-program-xyz".into()]).unwrap();
    assert!(matches!(
        sup.launch(
            &a,
            LaunchRequest {
                spec,
                operation_id: None,
                lease: None,
                approved_hold: false
            },
            &policy
        ),
        Err(LaunchRefusal::Spawn(_))
    ));
    let failed = fx.events("ProcessLaunchFailed");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].root_id, Some(root("root-a")));
}

#[test]
fn bounded_cancellation_reaps_the_tree_and_transfers_its_physical_lease() {
    let _serial = serial();
    let fx = Fx::new();
    fx.register(MachineResource {
        name: "gpu".into(),
        capacity: 1,
        physical: true,
        labels: vec![],
    });
    let sup = fx.supervisor();
    let (fs, roots) = policy_parts();
    let policy = LaunchPolicy {
        fs: &fs,
        roots: &roots,
        protected_branches: &[],
    };
    let a = fx.agent("agent-a", "project-a", "s-a");
    let b = fx.agent("agent-b", "project-b", "s-b");
    let la = lease(fx.acquire(&a, "gpu", json!({"leaseMs": "1000"})));
    // A stale fence never starts a process.
    let stale = sup.launch(
        &a,
        LaunchRequest {
            spec: sleeping_tree(30),
            operation_id: None,
            lease: Some(LeaseClaim {
                resource: "gpu".into(),
                lease: la.id.clone(),
                fence: 9,
            }),
            approved_hold: false,
        },
        &policy,
    );
    assert!(matches!(
        stale,
        Err(LaunchRefusal::Resource(DispatchFailure::Refused(
            DispatchRefusal::LeaseLost
        )))
    ));
    let p = sup
        .launch(
            &a,
            LaunchRequest {
                spec: sleeping_tree(30),
                operation_id: None,
                lease: Some(LeaseClaim {
                    resource: "gpu".into(),
                    lease: la.id.clone(),
                    fence: 1,
                }),
                approved_hold: false,
            },
            &policy,
        )
        .expect("launched");
    let record = {
        let id = p.id.clone();
        fx.read(move |r| r.resources().process(&id)).unwrap()
    };
    assert_eq!(record.phase, ProcessPhase::Running);
    assert_eq!(record.pid, Some(i64::from(p.identity.pid)));
    assert_eq!(
        (
            record.actor.clone(),
            record.root_id.clone(),
            record.lease_id.clone()
        ),
        (actor("agent-a"), Some(root("root-a")), Some(la.id.clone()))
    );
    let lb = lease(fx.acquire(&b, "gpu", json!({})));
    // The lease expires while the tree runs: uncertain, the waiter is not granted.
    fx.h.set_now(T0 + 5_000);
    fx.reconcile(ReconcileCause::Sweep);
    assert_eq!(fx.stored(&la).phase, LeasePhase::Uncertain);
    assert_eq!(fx.stored(&lb).phase, LeasePhase::Waiting);
    assert!(sup.poll(&p).unwrap().is_none());
    // Bounded cancellation reaps the whole tree, then the lease transfers in the same unit.
    let started = std::time::Instant::now();
    let outcome = sup
        .cancel(&p, Duration::from_secs(2), Duration::from_secs(10))
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(matches!(outcome, StopOutcome::Reaped { .. }), "{outcome:?}");
    assert_eq!(NativeProcessHost.probe(&p.identity), Probe::Gone);
    assert_eq!(fx.stored(&la).end_reason, Some(LeaseEndReason::Reconciled));
    let granted = fx.stored(&lb);
    assert_eq!((granted.phase, granted.fence), (LeasePhase::Held, 2));
    assert!(sup.owned().is_empty());
    // A finished tree is reaped by polling.
    let quick = sup
        .launch(
            &b,
            LaunchRequest {
                spec: shell("exit 0"),
                operation_id: None,
                lease: None,
                approved_hold: false,
            },
            &policy,
        )
        .unwrap();
    assert!(matches!(
        sup.wait(&quick, Duration::from_secs(10)).unwrap(),
        Some(StopOutcome::Reaped {
            exit_code: Some(0),
            ..
        })
    ));
    for kind in ["ProcessStarted", "ProcessStopRequested", "ProcessReaped"] {
        assert!(!fx.events(kind).is_empty(), "{kind}");
    }
    // Shutdown with nothing owned only reconciles.
    assert!(
        sup.shutdown(Duration::from_millis(100), Duration::from_secs(1))
            .into_result()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn shutdown_cancels_every_owned_tree_within_one_bound() {
    let _serial = serial();
    let fx = Fx::new();
    let sup = fx.supervisor();
    let (fs, roots) = policy_parts();
    let policy = LaunchPolicy {
        fs: &fs,
        roots: &roots,
        protected_branches: &[],
    };
    let a = fx.agent("agent-a", "project-a", "s-a");
    // A work root's writers are stopped alone (GIT-07 cleanup, archival), others keep running.
    let other = fx.agent("agent-b", "project-b", "s-b");
    let theirs = sup
        .launch(
            &other,
            LaunchRequest {
                spec: sleeping_tree(30),
                operation_id: None,
                lease: None,
                approved_hold: false,
            },
            &policy,
        )
        .unwrap();
    let mine = sup
        .launch(
            &a,
            LaunchRequest {
                spec: sleeping_tree(30),
                operation_id: None,
                lease: None,
                approved_hold: false,
            },
            &policy,
        )
        .unwrap();
    let stopped = all_recorded(sup.stop(
        &StopScope::Root(root("root-a")),
        Duration::from_millis(500),
        Duration::from_secs(10),
    ))
    .unwrap();
    assert_eq!(
        stopped.iter().map(|(i, _)| i.clone()).collect::<Vec<_>>(),
        vec![mine.id.clone()]
    );
    assert_eq!(NativeProcessHost.probe(&mine.identity), Probe::Gone);
    assert_eq!(NativeProcessHost.probe(&theirs.identity), Probe::Alive);
    let launched: Vec<_> = (0..2)
        .map(|_| {
            sup.launch(
                &a,
                LaunchRequest {
                    spec: sleeping_tree(30),
                    operation_id: None,
                    lease: None,
                    approved_hold: false,
                },
                &policy,
            )
            .unwrap()
        })
        .collect();
    let started = std::time::Instant::now();
    let out = sup
        .shutdown(Duration::from_millis(500), Duration::from_secs(10))
        .into_result()
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(
        out.len(),
        3,
        "the other project's tree and the two new ones"
    );
    assert!(
        out.iter()
            .all(|(_, o)| matches!(o, StopOutcome::Reaped { .. })),
        "{out:?}"
    );
    assert_eq!(NativeProcessHost.probe(&theirs.identity), Probe::Gone);
    for p in &launched {
        assert_eq!(NativeProcessHost.probe(&p.identity), Probe::Gone);
    }
    assert_eq!(
        fx.events("ResourcesReconciled").last().unwrap().payload["cause"],
        "Shutdown"
    );
}

#[test]
fn recovery_kills_trees_of_an_earlier_boot_and_holds_lost_launches_for_a_person() {
    let _serial = serial();
    let fx = Fx::new();
    fx.register(MachineResource {
        name: "gpu".into(),
        capacity: 2,
        physical: true,
        labels: vec![],
    });
    let a = fx.agent("agent-a", "project-a", "s-a");
    let running = lease(fx.acquire(&a, "gpu", json!({})));
    let lost = lease(fx.acquire(&fx.agent("agent-c", "project-a", "s-c"), "gpu", json!({})));
    // A crash: the earlier boot launched a tree and recorded it, then died without stopping it.
    let tree = NativeProcessHost.spawn(&sleeping_tree(30)).unwrap();
    let identity = tree.identity().clone();
    std::mem::forget(tree);
    let platform = NativeProcessHost.platform().to_owned();
    let (lr, ll) = (running.id.clone(), lost.id.clone());
    let ident = identity.clone();
    fx.write(move |tx| {
        let mut p = tx.resources().insert_process(NewProcess {
            id: Id::new("process-running").unwrap(),
            platform: platform.clone(),
            program: "sh".into(),
            argv_sha256: "a".repeat(64),
            operation_id: None,
            lease_id: Some(lr),
            actor: actor("agent-a"),
            session_id: None,
            project_id: Some(project("project-a")),
            root_id: Some(root("root-a")),
            assignment_id: None,
        })?;
        p.phase = ProcessPhase::Running;
        p.pid = Some(i64::from(ident.pid));
        p.birth = Some(ident.birth.clone());
        p.group = ident.group;
        p.revision = 2;
        tx.resources().update_process(&p, 1)?;
        // A launch whose identity was never recorded.
        tx.resources().insert_process(NewProcess {
            id: Id::new("process-lost").unwrap(),
            platform,
            program: "sh".into(),
            argv_sha256: "b".repeat(64),
            operation_id: None,
            lease_id: Some(ll),
            actor: actor("agent-c"),
            session_id: None,
            project_id: Some(project("project-a")),
            root_id: Some(root("root-a")),
            assignment_id: None,
        })?;
        Ok(())
    });
    let fx = fx.restart();
    let sup = fx.supervisor();
    let report = sup.recover().unwrap();
    assert_eq!((report.killed, report.lost), (1, 1), "{report:?}");
    assert_eq!(
        NativeProcessHost.probe(&identity),
        Probe::Gone,
        "the orphaned tree was killed"
    );
    let read = |p: &str| {
        let p = id(p);
        fx.read(move |r| r.resources().process(&p)).unwrap()
    };
    assert_eq!(read("process-running").phase, ProcessPhase::Reaped);
    assert_eq!(read("process-lost").phase, ProcessPhase::Lost);
    // The lease served by the killed tree transferred; the lost one stays uncertain.
    assert_eq!(
        fx.stored(&running).end_reason,
        Some(LeaseEndReason::Reconciled)
    );
    assert_eq!(fx.stored(&lost).phase, LeasePhase::Uncertain);
    // Only a person can declare the lost launch stopped.
    let agent = fx.agent("agent-a", "project-a", "s-a");
    assert!(matches!(
        sup.confirm_stopped(&agent, &id("process-lost")),
        Err(ConfirmRefusal::HumanOnly)
    ));
    let user = fx.ctx("user-a", PrincipalKind::Human, None);
    let confirmed = match sup.confirm_stopped(&user, &id("process-lost")) {
        Ok(c) => c,
        Err(e) => panic!("{e:?}"),
    };
    assert_eq!(confirmed.phase, ProcessPhase::Reaped);
    assert_eq!(
        fx.stored(&lost).end_reason,
        Some(LeaseEndReason::Reconciled)
    );
    let statement = fx.events("ProcessConfirmedStopped");
    assert_eq!(statement[0].actor, actor("user-a"));
    // A second recovery finds nothing left to reconcile.
    let again = sup.recover().unwrap();
    assert_eq!(
        (again.killed, again.lost, again.reaped, again.uncertain),
        (0, 0, 0, 0)
    );
}

// ---------------------------------------------------------------------------------------------
// Native stops never depend on a successful store write

/// The store, except that every write is refused while `refuse` is set (a full disk).
struct Refusing {
    inner: Arc<SqliteStore>,
    refuse: Arc<std::sync::atomic::AtomicBool>,
}
impl ApplicationStore for Refusing {
    fn info(&self) -> StoreInfo {
        self.inner.info()
    }
    fn submit_write(
        &self,
        options: WriteOptions,
        job: Box<dyn WriteJob>,
    ) -> Result<(), StorageError> {
        if self.refuse.load(Ordering::SeqCst) {
            return Err(StorageError::unavailable(
                Unavailable::DiskFull,
                "disk full",
            ));
        }
        self.inner.submit_write(options, job)
    }
    fn read(
        &self,
        options: &ReadOptions,
        work: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.inner.read(options, work)
    }
}

#[test]
fn a_refused_record_write_never_skips_the_native_stop() {
    let _serial = serial();
    let fx = Fx::new();
    let refuse = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let store: Arc<dyn ApplicationStore> = Arc::new(Refusing {
        inner: fx.store.clone(),
        refuse: refuse.clone(),
    });
    let sup = ProcessSupervisor::new(store, Arc::new(NativeProcessHost), fx.svc.clone());
    let (fs, roots) = policy_parts();
    let policy = LaunchPolicy {
        fs: &fs,
        roots: &roots,
        protected_branches: &[],
    };
    let a = fx.agent("agent-a", "project-a", "s-a");
    let launch = || {
        sup.launch(
            &a,
            LaunchRequest {
                spec: sleeping_tree(30),
                operation_id: None,
                lease: None,
                approved_hold: false,
            },
            &policy,
        )
        .unwrap()
    };
    let (first, second) = (launch(), launch());
    refuse.store(true, Ordering::SeqCst);
    // The `Stopping` write fails: the tree is still stopped, and the failure is reported with
    // what the native stop verified.
    match sup.cancel(&first, Duration::from_millis(500), Duration::from_secs(10)) {
        Err(StopFailure {
            outcome: StopOutcome::Reaped { .. },
            error: StorageError::StorageUnavailable { .. },
        }) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(NativeProcessHost.probe(&first.identity), Probe::Gone);
    // Shutdown stops every tree although each record write fails, and reports each.
    let report = sup.shutdown(Duration::from_millis(500), Duration::from_secs(10));
    assert_eq!(report.stopped.len(), 2, "the unrecorded tree stays owned");
    assert!(
        report.stopped.iter().all(|(_, r)| matches!(
            r,
            Err(StopFailure {
                outcome: StopOutcome::Reaped { .. },
                ..
            })
        )),
        "{:?}",
        report.stopped
    );
    assert!(report.reconciliation.is_err());
    assert_eq!(NativeProcessHost.probe(&second.identity), Probe::Gone);
    // The records were left where they can still be reconciled: once writes succeed, the
    // retained trees are recorded `Reaped`.
    let read = |p: &OwnedProcess| {
        let id = p.id.clone();
        fx.read(move |r| r.resources().process(&id)).unwrap().phase
    };
    assert_eq!(read(&first), ProcessPhase::Running);
    assert_eq!(read(&second), ProcessPhase::Running);
    refuse.store(false, Ordering::SeqCst);
    let report = sup.shutdown(Duration::from_millis(100), Duration::from_secs(5));
    assert_eq!(report.into_result().unwrap().len(), 2);
    assert_eq!(
        (read(&first), read(&second)),
        (ProcessPhase::Reaped, ProcessPhase::Reaped)
    );
    assert!(sup.owned().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Preflight in the child's environment

#[cfg(unix)]
#[test]
fn launch_preflight_resolves_targets_in_the_childs_environment() {
    let _serial = serial();
    let fx = Fx::new();
    let sup = fx.supervisor();
    // A registered primary repository under a directory that is the child's home, not ours.
    let scratch = Scratch::new("child-home");
    let projects = scratch.path("projects");
    let repo = format!("{projects}/repo");
    std::fs::create_dir_all(format!("{repo}/.git")).unwrap();
    fx.h.sql_with(
        "INSERT INTO repositories(id,canonical_location,revision) VALUES('repo-a',?1,1)",
        &repo,
    );
    let fs = NativeFilesystem::default();
    let roots = ProtectedRoots::read(&*fx.store, Vec::new(), None).unwrap();
    let policy = LaunchPolicy {
        fs: &fs,
        roots: &roots,
        protected_branches: &[],
    };
    let a = fx.agent("agent-a", "project-a", "s-a");
    for (script, var) in [
        ("rm -rf ~/repo", "HOME"),
        ("rm -rf \"$HOME/repo\"", "HOME"),
        ("rm -rf \"$TARGET_ROOT\"/repo", "TARGET_ROOT"),
        ("rm -rf ${TARGET_ROOT}/repo", "TARGET_ROOT"),
    ] {
        for approved in [false, true] {
            let mut spec = shell(script);
            spec.env = vec![(var.to_owned(), projects.clone())];
            let refusal = sup
                .launch(
                    &a,
                    LaunchRequest {
                        spec,
                        operation_id: None,
                        lease: None,
                        approved_hold: approved,
                    },
                    &policy,
                )
                .err();
            assert!(
                matches!(refusal, Some(LaunchRefusal::Blocked(_))),
                "{script} with {var} set (approved {approved}): {refusal:?}"
            );
        }
    }
    assert!(
        std::path::Path::new(&repo).join(".git").exists(),
        "the repository survived"
    );
    // Nothing was launched or recorded.
    let none = fx.read(|r| r.resources().processes(&ProcessFilter::Unsettled, None, 10));
    assert!(none.is_empty());
}

#[test]
fn one_shot_and_interactive_stdin_both_reach_the_owned_child() {
    let _serial = serial();
    for interactive in [false, true] {
        let mut spec =
            ProcessSpec::new(&["git".into(), "hash-object".into(), "--stdin".into()]).unwrap();
        spec.capture_output = true;
        spec.pipe_stdin = interactive;
        spec.stdin = (!interactive).then(|| b"hello\n".to_vec());
        let mut tree = NativeProcessHost.spawn(&spec).unwrap();
        if interactive {
            let mut input = tree
                .take_input()
                .expect("interactive stdin remains owned by the tree");
            std::io::Write::write_all(&mut input, b"hello\n").unwrap();
            drop(input);
        } else {
            assert!(
                tree.take_input().is_none(),
                "one-shot writer owns and closes stdin"
            );
        }
        let (mut stdout, _) = tree.take_output().unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut out = String::new();
            let result = std::io::Read::read_to_string(&mut stdout, &mut out).map(|_| out);
            let _ = sent.send(result);
        });
        let output = received.recv_timeout(Duration::from_secs(30));
        drop(tree); // Stop the owned child even when the read did not finish.
        assert_eq!(
            output.expect("stdin reached EOF").unwrap().trim(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }
}

#[test]
fn conflicting_stdin_modes_are_refused_before_spawn() {
    let _serial = serial();
    let mut spec = ProcessSpec::new(&["a-program-that-must-never-launch".into()]).unwrap();
    spec.pipe_stdin = true;
    spec.stdin = Some(Vec::new());
    assert!(matches!(NativeProcessHost.spawn(&spec),
        Err(devworks_core::ports::process::ProcessError::Spawn(message))
            if message.contains("mutually exclusive")));
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
#[test]
fn current_host_identity_is_the_actual_process_and_wrong_birth_is_not_alive() {
    let _serial = serial();
    let host = NativeProcessHost;
    let identity = host.current_identity().expect("native self birth token");
    assert_eq!(identity.pid, std::process::id());
    assert!(!identity.birth.is_empty());
    assert_ne!(identity.birth, "unavailable");
    assert_eq!(identity.group, None);
    assert_eq!(host.probe(&identity), Probe::Alive);
    assert_eq!(host.current_identity(), Some(identity.clone()));
    let mut reused = identity.clone();
    reused.birth = different_birth(&identity.birth);
    assert_eq!(host.probe(&reused), Probe::Gone);
    let mut unavailable = identity.clone();
    unavailable.birth = "unavailable".into();
    assert!(matches!(host.probe(&unavailable), Probe::Unknown(_)));
    unavailable.birth.clear();
    assert!(matches!(host.probe(&unavailable), Probe::Unknown(_)));
    #[cfg(target_os = "linux")]
    {
        unavailable.birth = ":12345".into();
        assert!(matches!(host.probe(&unavailable), Probe::Unknown(_)));
    }
    assert_eq!(host.probe(&identity), Probe::Alive);
}

/// Change a numeric component while retaining the platform's actual captured token shape.
fn different_birth(birth: &str) -> String {
    let mut changed = birth.to_owned();
    let last = changed.pop().expect("native birth digit");
    changed.push(if last == '0' { '1' } else { '0' });
    changed
}

fn privileged_policy_test_context() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: read-only credential queries.
        unsafe { libc::getuid() == 0 || libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn policy_result(
    spec: &ProcessSpec,
) -> Result<(TreeStatus, String), devworks_core::ports::process::ProcessError> {
    let mut child = NativeProcessHost.spawn(spec)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        let status = child.poll()?;
        if status.empty {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "native policy child did not finish"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut error = String::new();
    if let Some((mut stdout, mut stderr)) = child.take_output() {
        let mut output = String::new();
        std::io::Read::read_to_string(&mut stdout, &mut output).unwrap();
        std::io::Read::read_to_string(&mut stderr, &mut error).unwrap();
    }
    Ok((status, error))
}

#[test]
fn required_no_child_processes_blocks_helpers_before_their_marker_and_keeps_the_parent_usable() {
    let _serial = serial();
    let scratch = Scratch::new("child-policy");
    let root = scratch.path("root-marker");
    let marker = scratch.path("child-marker");
    let finished = scratch.path("root-finished");
    let executable = std::env::current_exe().unwrap();
    let mut spec = if cfg!(windows) {
        ProcessSpec::new(&[
            executable.to_string_lossy().into_owned(),
            "--exact".into(),
            "no_child_policy_root_fixture".into(),
            "--nocapture".into(),
            "--test-threads=1".into(),
        ])
        .unwrap()
    } else {
        shell(&format!(
            "printf root > '{root}'; '{}' --exact no_child_policy_marker_fixture --nocapture; printf finished",
            executable.display()
        ))
    };
    spec.env
        .push(("DEVWORKS_NO_CHILD_POLICY_MARKER".into(), marker.clone()));
    spec.env
        .push(("DEVWORKS_NO_CHILD_POLICY_ROOT".into(), root.clone()));
    spec.env
        .push(("DEVWORKS_NO_CHILD_POLICY_FINISHED".into(), finished.clone()));
    assert!(!spec.no_child_processes);
    spec.no_child_processes = true;
    spec.capture_output = true;
    let (status, error) = match policy_result(&spec) {
        Ok(result) => result,
        Err(devworks_core::ports::process::ProcessError::EnforcementUnavailable(_))
            if privileged_policy_test_context() =>
        {
            assert!(!std::path::Path::new(&root).exists());
            assert!(!std::path::Path::new(&marker).exists());
            return;
        }
        Err(error) => panic!("native no-child policy unavailable: {error}"),
    };
    assert!(
        std::path::Path::new(&root).exists(),
        "the constrained root ran: {status:?} {error}"
    );
    assert!(
        !std::path::Path::new(&marker).exists(),
        "an unreviewed child ran: {status:?} {error}"
    );
    if cfg!(windows) {
        assert!(
            std::path::Path::new(&finished).exists(),
            "the constrained native root remains usable: {status:?} {error}"
        );
    }
    spec.no_child_processes = false;
    let (control, error) = policy_result(&spec).unwrap();
    assert_eq!(control.exit_code, Some(0), "{error}");
    assert!(
        std::path::Path::new(&marker).exists(),
        "the unrestricted positive-control helper must actually run"
    );
}

#[test]
fn no_child_processes_keeps_native_git_add_commit_working_and_blocks_a_new_hook_alias() {
    let _serial = serial();
    let scratch = Scratch::new("child-policy");
    let repo = scratch.0.join("repo");
    let hooks = scratch.0.join("empty-hooks");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&hooks).unwrap();
    let executable = if cfg!(windows) {
        let output = std::process::Command::new("git")
            .arg("--exec-path")
            .output()
            .unwrap();
        assert!(output.status.success());
        let exec = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
        exec.join("../../bin/git.exe")
            .canonicalize()
            .expect("the real Git for Windows executable, not its cmd shim")
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|base| base.join("git"))
            .find(|path| path.is_file())
            .unwrap()
            .canonicalize()
            .unwrap()
    };
    let git = |args: &[&str], limited| {
        let mut argv = vec![executable.to_string_lossy().into_owned()];
        for (key, value) in [
            ("core.hooksPath", hooks.to_str().unwrap()),
            ("core.fsmonitor", ""),
            ("index.threads", "1"),
            ("pack.threads", "1"),
            ("checkout.workers", "1"),
            ("gc.auto", "0"),
            ("maintenance.auto", "false"),
            ("commit.gpgsign", "false"),
            ("tag.gpgsign", "false"),
        ] {
            argv.extend(["-c".into(), format!("{key}={value}")]);
        }
        argv.extend(args.iter().map(|v| v.to_string()));
        let mut spec = ProcessSpec::new(&argv).unwrap();
        spec.cwd = Some(repo.clone());
        spec.capture_output = true;
        spec.no_child_processes = limited;
        spec.clear_env = true;
        spec.env = std::env::vars()
            .filter(|(key, _)| !key.to_ascii_uppercase().starts_with("GIT_"))
            .collect();
        spec.env.extend(vec![
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_CONFIG_GLOBAL".into(), scratch.path("no-global")),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ]);
        policy_result(&spec)
    };
    assert_eq!(git(&["init", "-q"], false).unwrap().0.exit_code, Some(0));
    for args in [
        ["config", "user.name", "Native policy"],
        ["config", "user.email", "native@example.test"],
    ] {
        assert_eq!(git(&args, false).unwrap().0.exit_code, Some(0));
    }
    std::fs::write(repo.join("reviewed"), "first\n").unwrap();
    let staged = match git(&["add", "--", "reviewed"], true) {
        Ok(result) => result,
        Err(devworks_core::ports::process::ProcessError::EnforcementUnavailable(_))
            if privileged_policy_test_context() =>
        {
            return;
        }
        Err(error) => panic!("required native Git policy unavailable: {error}"),
    };
    assert_eq!(staged.0.exit_code, Some(0), "{}", staged.1);
    let committed = git(&["commit", "-m", "reviewed base"], true).unwrap();
    assert_eq!(committed.0.exit_code, Some(0), "{}", committed.1);
    // The capability-specific race supplements the unconditional native child-denial proof.
    // Older supported Git has no friendly hook aliases and keeps its existing version floor.
    let advertised = std::process::Command::new(&executable)
        .args(["help", "--config"])
        .output()
        .unwrap();
    if !String::from_utf8_lossy(&advertised.stdout).contains("hook.<friendly-name>.event") {
        return;
    }
    let marker = scratch.path("late-hook-marker");
    let command = format!(
        "printf unexpected > '{}'",
        marker.replace('\\', "/").replace('\'', "'\\''")
    );
    assert_ne!(
        git(
            &[
                "config",
                "--get-regexp",
                r"^hook\..*\.(event|command|enabled)$"
            ],
            false
        )
        .unwrap()
        .0
        .exit_code,
        Some(0)
    );
    assert_eq!(
        git(
            &["config", "hook.after-enumeration.event", "pre-commit"],
            false
        )
        .unwrap()
        .0
        .exit_code,
        Some(0)
    );
    assert_eq!(
        git(
            &["config", "hook.after-enumeration.command", &command],
            false
        )
        .unwrap()
        .0
        .exit_code,
        Some(0)
    );
    std::fs::write(repo.join("reviewed"), "second\n").unwrap();
    assert_eq!(
        git(&["add", "--", "reviewed"], true).unwrap().0.exit_code,
        Some(0)
    );
    let denied = git(&["commit", "-m", "late helper must not execute"], true).unwrap();
    assert_ne!(denied.0.exit_code, Some(0), "{}", denied.1);
    assert!(
        !std::path::Path::new(&marker).exists(),
        "new hook escaped process policy"
    );
    let control = git(&["commit", "-m", "positive helper control"], false).unwrap();
    assert_eq!(control.0.exit_code, Some(0), "{}", control.1);
    assert!(std::path::Path::new(&marker).exists());
}

#[test]
fn no_child_policy_marker_fixture() {
    if let Some(marker) = std::env::var_os("DEVWORKS_NO_CHILD_POLICY_MARKER") {
        std::fs::write(marker, "the external child actually executed").unwrap();
    }
}

#[cfg(windows)]
#[test]
fn no_child_policy_root_fixture() {
    let Some(root) = std::env::var_os("DEVWORKS_NO_CHILD_POLICY_ROOT") else {
        return;
    };
    std::fs::write(root, "the constrained native root actually executed").unwrap();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "no_child_policy_marker_fixture",
            "--nocapture",
            "--test-threads=1",
        ])
        .status();
    if let Err(error) = child {
        eprintln!("native child launch was refused: {error}");
    }
    std::fs::write(
        std::env::var_os("DEVWORKS_NO_CHILD_POLICY_FINISHED").unwrap(),
        "the constrained root remains usable after attempting a child",
    )
    .unwrap();
}

#[cfg(unix)]
#[test]
fn required_child_policy_refuses_a_set_id_program_before_any_target_effect() {
    use std::os::unix::fs::PermissionsExt;
    let _serial = serial();
    let scratch = Scratch::new("child-policy-setid");
    let target = scratch.0.join("set-id-shell");
    std::fs::copy("/bin/sh", &target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o4755)).unwrap();
    let marker = scratch.path("must-not-run");
    let mut spec = ProcessSpec::new(&[
        target.to_string_lossy().into_owned(),
        "-c".into(),
        format!("printf forbidden > '{marker}'"),
    ])
    .unwrap();
    spec.no_child_processes = true;
    assert!(matches!(
        NativeProcessHost.spawn(&spec),
        Err(devworks_core::ports::process::ProcessError::EnforcementUnavailable(_))
    ));
    assert!(!std::path::Path::new(&marker).exists());
}
