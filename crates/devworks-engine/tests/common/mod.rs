//! Shared fixtures: the real SQLite adapter in a temporary data root, a controllable wall clock,
//! deterministic identifiers, scripted handlers and executors. No inference, network or sleeps
//! used as synchronization.
#![allow(dead_code, clippy::result_large_err)]
use devworks_adapters::storage::sqlite::{
    DATABASE_FILE, SqliteConfig, SqliteStore, seed_migrated_store,
};
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::ports::storage::*;
use devworks_core::ports::{
    AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization, Clock, IdKind, IdSource,
};
use devworks_core::protocol::{ActorId, Counter, EventId, Id, OperationId, Revision};
use devworks_core::registry::AuthorityClass;
use devworks_engine::{
    Consumer, Delivery, DeliveryHandler, Dispatcher, DispatcherConfig, EffectExecutor,
    EffectFuture, EffectResult, Outcome, RetryPolicy,
};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[path = "native_runtime_case.rs"]
mod case_budget;
#[path = "help_startup.rs"]
mod help_startup;

/// Acquire first in a heavy case so its owned permit drops after every app/runtime and on panic.
/// Multiple apps, commands and tasks within the case retain their original concurrency.
#[must_use = "hold the case permit until runtime cleanup has finished"]
pub fn native_runtime_case() -> impl Drop {
    case_budget::acquire()
}

/// A large indexing fixture owns the complete case budget; internal parser/runtime work
/// remains concurrent and every original watchdog still starts after case admission.
#[must_use = "hold the case permit until runtime cleanup has finished"]
pub fn exclusive_native_runtime_case() -> impl Drop {
    case_budget::acquire_exclusive()
}

/// Failure-only metadata snapshot; no source/document bodies are read or printed.
pub fn help_startup_diagnostics<'a>(
    store: &'a dyn ApplicationStore,
    dispatcher: &'a Dispatcher,
) -> impl Drop + 'a {
    help_startup::watch(store, dispatcher)
}

pub const T0: i64 = 1_700_000_000_000;

pub struct TestClock(pub AtomicI64);
impl Clock for TestClock {
    fn now_ms(&self) -> Counter {
        Counter::new(self.0.load(Ordering::SeqCst)).unwrap()
    }
}
pub struct TestIds(AtomicU64);
impl IdSource for TestIds {
    fn next_id(&self, kind: IdKind) -> Id {
        let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        Id::new(format!("{kind:?}-{n}").to_lowercase()).unwrap()
    }
}

/// A temporary data root reopened across simulated process restarts.
pub struct Harness {
    pub root: PathBuf,
    pub clock: Arc<TestClock>,
    pub ids: Arc<TestIds>,
    /// Read connections of every store opened (0: the adapter's default).
    pub readers: AtomicUsize,
}
impl Harness {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // Cargo's per-target scratch directory, not the system temp directory: on GitHub's
        // Windows runners %TEMP% is on the C: OS disk, whose flush latency stalls the hundreds of
        // durable commits of native Help startup past the `within` guard.
        let root = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "devworks-engine-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&root);
        Self {
            root,
            clock: Arc::new(TestClock(AtomicI64::new(T0))),
            ids: Arc::new(TestIds(AtomicU64::new(0))),
            readers: AtomicUsize::new(0),
        }
    }
    /// Opens (or reopens, as a new boot) the store.
    pub fn open(&self) -> Arc<dyn ApplicationStore> {
        Arc::new(self.open_sqlite())
    }
    pub fn open_sqlite(&self) -> SqliteStore {
        let mut config = SqliteConfig::new(
            self.root.clone(),
            self.clock.clone(),
            self.ids.clone(),
            ActorId(Id::new("host").unwrap()),
        );
        // An empty root gets a migrated copy with the identity, ledger time and build a fresh
        // bootstrap would write, instead of re-running every migration in every test.
        if !self.root.join(DATABASE_FILE).exists() {
            seed_migrated_store(
                &self.root,
                self.ids.next_id(IdKind::Store).as_str(),
                self.now(),
                &config.app_build,
            );
        }
        let readers = self.readers.load(Ordering::SeqCst);
        if readers > 0 {
            config.read_connections = readers;
        }
        SqliteStore::open(config).expect("open store")
    }
    pub fn now(&self) -> i64 {
        self.clock.0.load(Ordering::SeqCst)
    }
    pub fn advance(&self, ms: i64) {
        self.clock.0.fetch_add(ms, Ordering::SeqCst);
    }
    pub fn clock(&self) -> Arc<dyn Clock> {
        self.clock.clone()
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Failure-only guard for a test body whose regression mode is a hang (it awaits a signal that
/// only arrives if the code under test works). It never orders anything: a passing run finishes
/// long before it, and on expiry it fails the test instead of leaving it hung.
pub async fn within<F: std::future::Future>(what: &str, body: F) -> F::Output {
    match tokio::time::timeout(std::time::Duration::from_secs(30), body).await {
        Ok(output) => output,
        Err(_) => panic!("{what}: did not finish within 30 s; the awaited signal never arrived"),
    }
}

/// A current-thread runtime with timers.
pub fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

pub fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}

pub fn write<T: Send + 'static>(
    s: &dyn ApplicationStore,
    f: impl FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
) -> Result<T, StorageError> {
    run_write(s, WriteOptions::default(), f)
}
pub fn read<T>(
    s: &dyn ApplicationStore,
    f: impl FnOnce(&dyn ReadSnapshot) -> Result<T, StorageError>,
) -> T {
    read_snapshot(s, &ReadOptions::default(), f).expect("read")
}

pub fn event(tx: &mut dyn WriteTransaction, kind: &str) -> Result<EventId, StorageError> {
    let e = EventId(tx.new_id(IdKind::Event));
    let actor = tx.store().host_actor.clone();
    tx.history().append(NewEvent {
        id: e.clone(),
        actor,
        project_id: None,
        root_id: None,
        operation_id: None,
        aggregate_kind: "fixture".into(),
        aggregate_id: "a".into(),
        aggregate_revision: 1,
        kind: kind.into(),
        payload: json!({}),
    })?;
    Ok(e)
}
pub fn intent(tx: &mut dyn WriteTransaction) -> Result<OperationId, StorageError> {
    let op = OperationId(tx.new_id(IdKind::Operation));
    tx.operations().record_intent(NewOperation {
        id: op.clone(),
        root_id: None,
        assignment_id: None,
        capability: "fixture.effect".into(),
        schema_version: 1,
        args_digest: "digest".into(),
        payload: json!({}),
    })?;
    Ok(op)
}
pub fn enqueue_tx(
    tx: &mut dyn WriteTransaction,
    consumer: &str,
    partition: Option<&str>,
    operation: Option<OperationId>,
    payload: serde_json::Value,
) -> Result<QueueEntry, StorageError> {
    let event_id = event(tx, "Changed")?;
    let entry = tx.new_id(IdKind::QueueEntry);
    tx.queue().enqueue(NewQueueEntry {
        id: entry,
        consumer: consumer.into(),
        event_id,
        root_id: None,
        operation_id: operation,
        partition: partition.map(str::to_owned),
        priority: 0,
        ready_at_ms: 0,
        payload,
    })
}
pub fn enqueue(
    s: &dyn ApplicationStore,
    consumer: &str,
    partition: Option<&str>,
    payload: serde_json::Value,
) -> QueueEntry {
    let (c, p) = (consumer.to_owned(), partition.map(str::to_owned));
    write(s, move |tx| enqueue_tx(tx, &c, p.as_deref(), None, payload)).unwrap()
}
pub fn entry(s: &dyn ApplicationStore, e: &Id) -> QueueEntry {
    read(s, |r| r.queue().entry(e)).expect("entry")
}

/// A database handler that records each delivery and follows a script.
pub type Script = dyn Fn(&Delivery, usize) -> Outcome + Send + Sync;
pub struct Recorder {
    pub seen: Mutex<Vec<(String, Option<String>, i64)>>,
    pub calls: AtomicUsize,
    pub script: Box<Script>,
}
impl Recorder {
    pub fn new(script: impl Fn(&Delivery, usize) -> Outcome + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            script: Box::new(script),
        })
    }
    pub fn ok() -> Arc<Self> {
        Self::new(|_, _| Outcome::Completed)
    }
    pub fn seen(&self) -> Vec<(String, Option<String>, i64)> {
        self.seen.lock().unwrap().clone()
    }
}
impl DeliveryHandler for Recorder {
    fn apply(&self, _: &mut dyn WriteTransaction, d: &Delivery) -> Result<Outcome, StorageError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let outcome = (self.script)(d, n);
        if outcome == Outcome::Completed {
            self.seen.lock().unwrap().push((
                d.entry.consumer.clone(),
                d.entry.partition.clone(),
                d.entry.event_sequence,
            ));
        }
        Ok(outcome)
    }
}

pub fn consumer(name: &str, handler: Arc<dyn DeliveryHandler>) -> Consumer {
    Consumer {
        name: name.into(),
        handler,
        effect: None,
        async_handler: None,
        eligibility: None,
        stop_grace_ms: 5_000,
        retry: RetryPolicy {
            base_delay_ms: 100,
            max_delay_ms: 10_000,
            max_attempts: 3,
        },
        max_in_flight: None,
    }
}

pub fn dispatcher(
    h: &Harness,
    store: Arc<dyn ApplicationStore>,
    consumers: Vec<Consumer>,
) -> Dispatcher {
    dispatcher_with(h, store, consumers, |_| {})
}
pub fn dispatcher_with(
    h: &Harness,
    store: Arc<dyn ApplicationStore>,
    consumers: Vec<Consumer>,
    tune: impl FnOnce(&mut DispatcherConfig),
) -> Dispatcher {
    let mut cfg = DispatcherConfig::new(id("dispatcher-1"));
    tune(&mut cfg);
    Dispatcher::new(store, h.clock(), consumers, cfg).unwrap()
}

/// An external effect fixture: counts executions/observations, can be gated by the test, and
/// returns scripted results. Clones share state.
#[derive(Clone)]
pub struct Effects(Arc<EffectState>);
pub struct EffectState {
    pub executed: AtomicUsize,
    /// Notified (with a stored permit) each time an effect starts executing.
    pub started: tokio::sync::Notify,
    pub observed: AtomicUsize,
    pub running: AtomicUsize,
    pub max_running: AtomicUsize,
    pub gate: Mutex<Option<tokio::sync::watch::Receiver<bool>>>,
    pub result: Mutex<EffectResult>,
    pub observation: Mutex<EffectResult>,
}
impl std::ops::Deref for Effects {
    type Target = EffectState;
    fn deref(&self) -> &EffectState {
        &self.0
    }
}
impl Effects {
    pub fn new(result: EffectResult) -> Self {
        Self(Arc::new(EffectState {
            executed: AtomicUsize::new(0),
            started: tokio::sync::Notify::new(),
            observed: AtomicUsize::new(0),
            running: AtomicUsize::new(0),
            max_running: AtomicUsize::new(0),
            gate: Mutex::new(None),
            result: Mutex::new(result),
            observation: Mutex::new(EffectResult::Unknown("not observable".into())),
        }))
    }
    /// Effects then wait until the returned sender sends `true`.
    pub fn gated(&self) -> tokio::sync::watch::Sender<bool> {
        let (tx, rx) = tokio::sync::watch::channel(false);
        *self.gate.lock().unwrap() = Some(rx);
        tx
    }
    pub fn executor(&self) -> Arc<dyn EffectExecutor> {
        Arc::new(self.clone())
    }
    pub fn executed(&self) -> usize {
        self.executed.load(Ordering::SeqCst)
    }
}
impl EffectExecutor for Effects {
    fn execute(&self, _: OperationRecord) -> EffectFuture {
        let me = self.clone();
        Box::pin(async move {
            me.executed.fetch_add(1, Ordering::SeqCst);
            me.started.notify_one();
            let now = me.running.fetch_add(1, Ordering::SeqCst) + 1;
            me.max_running.fetch_max(now, Ordering::SeqCst);
            let gate = me.gate.lock().unwrap().clone();
            match gate {
                Some(mut g) => {
                    while !*g.borrow() {
                        if g.changed().await.is_err() {
                            break;
                        }
                    }
                }
                None => tokio::task::yield_now().await,
            }
            me.running.fetch_sub(1, Ordering::SeqCst);
            me.result.lock().unwrap().clone()
        })
    }
    fn observe(&self, _: OperationRecord) -> EffectFuture {
        let me = self.clone();
        Box::pin(async move {
            me.observed.fetch_add(1, Ordering::SeqCst);
            me.observation.lock().unwrap().clone()
        })
    }
}

/// Authority fixture: agents may read the queue but not resolve it (host maintenance).
pub struct Authority;
impl AuthorityPort for Authority {
    fn authorize(
        &self,
        ctx: &InvocationContext,
        request: &AuthorityRequest<'_>,
    ) -> Result<Authorization, AuthorityDenial> {
        match (ctx.principal_kind(), request.binding.authority) {
            (PrincipalKind::Agent, AuthorityClass::HostMaintenance) => {
                Err(AuthorityDenial::PermissionDenied)
            }
            _ => Ok(Authorization::direct()),
        }
    }
}

/// A host-registered principal bound to the open store.
pub fn principal(s: &dyn ApplicationStore, name: &str, kind: PrincipalKind) -> InvocationContext {
    let actor = ActorId(id(name));
    let registered = actor.clone();
    write(s, move |tx| {
        tx.actors().register(&registered, kind, "fixture")
    })
    .unwrap();
    let info = s.info();
    devworks_core::context::HostIngress::new(s).context(
        actor,
        kind,
        id(&format!("inv-{name}")),
        id(&format!("ingress-{name}")),
        Counter::new(info.restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    )
}

/// Seeds a project and a feature work item `root` directly in the store file: root-scoped
/// deliveries need an owning root and the engine has no work-item port.
pub fn seed_root(h: &Harness, root: &str) {
    let c = rusqlite::Connection::open(h.root.join("workspace.sqlite")).unwrap();
    c.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    c.execute_batch(&format!(
        "INSERT OR IGNORE INTO projects(id,title,lifecycle,revision,scope_revision,created_at) VALUES('project-a','A','Active',1,1,0);
         INSERT INTO work_items VALUES('{root}','project-a','feature',NULL,NULL,NULL,'Fixture',1,0,1,1,1,'{{}}');"
    ))
    .unwrap();
}

/// Submits a `queue.*` resolution for `e` through the shared gateway as `ctx`.
pub fn resolve_queue(
    h: &Harness,
    s: &dyn ApplicationStore,
    ctx: &InvocationContext,
    operation: &str,
    key: &str,
    e: &QueueEntry,
    resolution: &str,
) -> devworks_core::protocol::CommandReply<devworks_core::protocol::results::DomainDecision> {
    let body = format!(
        r#"{{"mode":"Command","protocol":"1.0-draft.1","requestKey":"{key}","body":{{"kind":"{operation}","deliveryId":"{}","expectedRevision":"{}","expectedFence":"{}","resolution":"{resolution}","reason":"operator decision"}}}}"#,
        e.id.as_str(),
        e.revision,
        e.generation
    );
    let ids = h.ids.clone();
    let gateway = devworks_core::gateway::Gateway {
        authority: &Authority,
        store: s,
        ids: &*ids,
    };
    devworks_core::services::UseCases::new()
        .with_queue()
        .command(&gateway, ctx, operation, body.as_bytes())
        .expect("addressed reply")
}
