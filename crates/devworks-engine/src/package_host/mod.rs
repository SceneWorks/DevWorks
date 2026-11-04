//! `PackageHost`: launches, monitors and stops package helper processes over the versioned local
//! process protocol (ADDON-01/03/04, EXT-06), and is the package service's
//! [`PackageActivity`] (quiescence barrier and post-commit reconciliation).
//!
//! # Instances
//!
//! The committed registry is the activation intent. [`PackageHost::reconcile`] reads it and makes
//! the running instances match: one instance per enabled, installed, available package, keyed by
//! `(manifest digest, state generation, configuration revision)`. An instance that no longer
//! matches stops admitting invocations and is retired once its in-flight invocations finish; a
//! disabled or uninstalled package's invocations are cancelled first. Every launch goes through
//! the process supervisor (preflight, durable launch record, owned process tree) after the
//! installed files are re-verified byte for byte; the helper gets a cleared environment (no
//! inherited credentials) and its install directory as working directory.
//!
//! # Handshake
//!
//! `host.initialize` (within [`HostConfig::handshake`], default five seconds) offers protocol
//! 1.0–1.`minor`, no required features, the 1 MiB frame limit and the expected manifest digest.
//! The reply must select a version both sides support, echo the expected digest and list
//! exactly the manifest's contribution inventory. `host.activate` then delivers the committed
//! registry revision, state generation, configuration revision and the committed configuration
//! (value and schema identity, when one was saved) and must echo the revisions. Only then is
//! the instance `Ready`; a new configuration is a new instance key, so a reconfigured helper
//! receives it in its own activation before it admits work. A startup session has no grants: the broker refuses every call from
//! a process before it is ready.
//!
//! # Failure, restart and guarded rollback
//!
//! A failed startup or a helper that exits while ready is restarted with backoff up to
//! [`HostConfig::restart_limit`] consecutive times. The failure count resets only after
//! sustained health: the instance stayed ready for [`HostConfig::healthy_after`] or completed an
//! invocation (a helper that dies right after every activation is not healthy). Then the
//! failure is recorded durably
//! (availability `Failed`, only that package affected) and, when no invocation was admitted under
//! the new generation, the service's guarded automatic rollback restores the predecessor.
//! Invocations in flight when a process dies end `OutcomeUnknown` (never a fabricated success).
//!
//! # Invocations, cancellation and the broker
//!
//! [`PackageHost::invoke`] admits an invocation only on a ready instance of the currently active
//! version and only while no quiescence barrier is up for its package (both under one lock, so a
//! barrier and an admission never interleave). It mints a short-lived invocation ID and token
//! bound to that process session. [`InvocationHandle::cancel`] first revokes the token (fencing
//! further broker effects), then sends `host.cancel` (an acknowledgement is not proof of stop),
//! waits the cooperative grace and terminates the owned process tree when needed; whatever was
//! not proven stopped is `OutcomeUnknown`.
//!
//! The broker resolves every package request against the current session, invocation and token.
//! `broker.state` reads and compare-and-sets the host-owned package state of the pinned
//! generation; an **event-handler invocation** may not use `broker.state` compare-and-set or an
//! effectful `broker.call`: its state change, effect intents, processing receipt and checkpoint
//! commit together through `broker.commit_event` ([`crate::package::commit_event`]) or not at
//! all. Brokered host calls (`broker.call` to application operations, HTTP and inference) are
//! the SDK callback boundary of a later item and are refused as `Unavailable` here.
//!
//! The helper is a separate process for failure containment, not an OS sandbox (see the package
//! service's trust model).
pub mod discovery;
pub mod session;

pub use discovery::{IndexPorts, RuntimePorts};

use crate::package::{CommitOutcome, CommitRefusal, PackageInvocation, commit_event};
use devworks_core::authority::destructive::{FilesystemResolver, ProtectedRoots};
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::ports::packages::{Inspection, PackageActivity, QuiescenceGuard};
use devworks_core::ports::process::ProcessSpec;
use devworks_core::ports::storage::{
    ApplicationStore, ReadOptions, StorageError, StoredPackage, WriteOptions, read_snapshot,
    run_write,
};
use devworks_core::ports::{IdKind, IdSource};
use devworks_core::protocol::records::{PackageAvailability, Severity};
use devworks_core::protocol::{Counter, Id, Revision};
use devworks_core::services::packages::{ORPHAN_AGE, PackageService, policy};
use devworks_core::services::process::{
    LaunchPolicy, LaunchRequest, OwnedProcess, ProcessSupervisor,
};
use devworks_extension_protocol::package::{
    Activate, ActivateConfiguration, BrokerCall, Cancel, HostRequest, Initialize,
    InvocationOutcome, InvocationScope, Invoke, PackageRequest, RpcErrorClass, RpcResult,
    RpcVersion, Shutdown, StateAction, WireVersion,
};
use devworks_extension_protocol::scalars as wire;
use serde_json::{Value, json};
use session::{Broker, Reply, Session, Slot, rpc_error};
use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Engineering defaults of the package host.
#[derive(Clone, Debug)]
pub struct HostConfig {
    /// One deadline for `host.initialize` and `host.activate` together.
    pub handshake: Duration,
    /// Cooperative cancellation before the process tree is terminated.
    pub cancel_grace: Duration,
    /// How long an invocation waits for a barrier to lift or an instance to become ready.
    pub admission_wait: Duration,
    /// Consecutive failed starts before the failure is recorded.
    pub restart_limit: u32,
    /// Base restart backoff (doubled per attempt).
    pub restart_backoff: Duration,
    /// One quiescence attempt of a staged update retry.
    pub quiesce_attempt: Duration,
    /// Bytes of helper stderr kept for the log.
    pub stderr_tail: usize,
    /// Host cap on the invocations one helper multiplexes (its declared limit applies below).
    pub max_concurrency: u16,
    /// Time between reconciliation passes besides the post-commit hint.
    pub tick: Duration,
    /// Bound on one runtime version command.
    pub runtime_timeout: Duration,
    /// Time between runtime probe passes (one runs at boot).
    pub runtime_interval: Duration,
    /// Time between index refreshes (one runs when the loop starts).
    pub index_interval: Duration,
    /// How long a ready instance must keep running before its failure count resets (an
    /// invocation it completes resets it too).
    pub healthy_after: Duration,
}
impl Default for HostConfig {
    fn default() -> Self {
        Self {
            handshake: Duration::from_secs(5),
            cancel_grace: Duration::from_secs(5),
            admission_wait: Duration::from_secs(30),
            restart_limit: 3,
            restart_backoff: Duration::from_millis(500),
            quiesce_attempt: Duration::from_secs(30),
            stderr_tail: 16 * 1024,
            max_concurrency: 16,
            tick: Duration::from_secs(30),
            runtime_timeout: Duration::from_secs(3),
            runtime_interval: Duration::from_secs(10 * 60),
            index_interval: Duration::from_secs(6 * 60 * 60),
            healthy_after: Duration::from_secs(30),
        }
    }
}

/// The ports a launch needs (preflight resolution).
pub struct LaunchPorts {
    pub fs: Arc<dyn FilesystemResolver>,
    pub roots: ProtectedRoots,
    pub protected_branches: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    digest: String,
    generation: i64,
    config_revision: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstanceStatus {
    Starting,
    Ready,
    /// No new invocations; retired when its in-flight ones finish.
    Retiring,
    Failed(String),
    Stopped,
}

struct Instance {
    package_id: String,
    key: Key,
    registry_revision: i64,
    inspection: Inspection,
    session: Mutex<Option<Arc<Session>>>,
    process: Mutex<Option<OwnedProcess>>,
    status: Mutex<InstanceStatus>,
    in_flight: AtomicU64,
    admitted: AtomicU64,
    stderr: Arc<Mutex<Vec<u8>>>,
    /// The committed configuration delivered in activation: `(schema, value)`.
    configuration: Option<(Value, Value)>,
    /// Whether this instance's failure was accounted (the restart budget counts it once).
    accounted: AtomicBool,
    /// When the instance became ready, and whether its sustained health was recorded.
    ready_at: Mutex<Option<Instant>>,
    healthy: AtomicBool,
}
impl Instance {
    fn status(&self) -> InstanceStatus {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    fn set(&self, s: InstanceStatus) {
        *self.status.lock().unwrap_or_else(|p| p.into_inner()) = s;
    }
    fn session(&self) -> Option<Arc<Session>> {
        self.session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    /// The owned process (a copy: the lock is never held across a wait or a kill).
    fn process(&self) -> Option<OwnedProcess> {
        self.process
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    fn take_process(&self) -> Option<OwnedProcess> {
        self.process
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }
}

/// What a running invocation is (the broker's rules differ for event handlers).
pub enum InvocationKind {
    Ordinary,
    /// A claimed event delivery: the host-resolved binding `broker.commit_event` checks.
    Event(Box<PackageInvocation>),
}

/// Host context of one invocation slot.
struct SlotContext {
    package_id: String,
    state_generation: i64,
    kind: InvocationKind,
    access: BTreeSet<String>,
}

/// One invocation request.
pub struct InvokeRequest {
    pub package_id: String,
    pub contribution_id: String,
    pub scope: InvocationScope,
    pub input: wire::ArtifactRef,
    pub operation_id: Id,
    pub deadline: Duration,
    pub allowed_operations: Vec<wire::QualifiedId>,
    pub kind: InvocationKind,
}

/// Why an invocation was not admitted. Nothing was sent to a helper.
#[derive(Clone, Debug)]
pub enum InvokeRefusal {
    Unavailable(PackageAvailability, String),
    /// The project disabled the package (project enablement).
    ProjectDisabled,
    /// The active version does not contribute it.
    NoSuchContribution,
    /// No ready instance (or the barrier did not lift) within the admission wait.
    NotReady(String),
    ShuttingDown,
    Storage(String),
}

/// How an invocation ended.
#[derive(Clone, Debug)]
pub enum InvokeOutcome {
    /// The package's terminal answer (including `CancelledBeforeEffect` and `OutcomeUnknown`).
    Finished(InvocationOutcome),
    /// The package refused the invocation with a JSON-RPC error.
    Failed(String),
    /// The outcome could not be established (the process died, was terminated after the
    /// cooperative deadline, or the session failed). Effects may or may not have happened.
    Unknown(String),
}

/// One admitted invocation, shared by its handle and its watcher (which enforces the deadline
/// and releases the admission once the invocation has a terminal outcome, whether or not anyone
/// still holds the handle).
struct Running {
    host: Arc<PackageHost>,
    instance: Arc<Instance>,
    session: Arc<Session>,
    slot: Arc<Slot>,
    cancelling: AtomicBool,
    released: AtomicBool,
}
impl Running {
    /// Revokes the token (fencing further broker effects), sends `host.cancel`, waits the
    /// cooperative grace and terminates the owned tree when the invocation did not stop. Runs
    /// once; afterwards the slot is terminal (a terminated session ends it `Lost`).
    fn cancel(&self) {
        self.slot.revoked.store(true, Ordering::SeqCst);
        if self.slot.terminal().is_some() || self.cancelling.swap(true, Ordering::SeqCst) {
            return;
        }
        // One cancellation grace for the acknowledgement and the stop together.
        let deadline = Instant::now() + self.host.config.cancel_grace;
        let left = || deadline.saturating_duration_since(Instant::now());
        let (sid, iid) = (self.session.id.clone(), self.slot.invocation_id.clone());
        let ack = self.session.request(
            |id| HostRequest::Cancel {
                jsonrpc: RpcVersion::V2,
                id,
                params: Cancel {
                    process_session_id: wid(&sid),
                    invocation_id: wid(&iid),
                    cancellation_revision: rev_w(1),
                },
            },
            left(),
        );
        // An acknowledgement records receipt of the request; it is not proof of stop.
        if matches!(ack, Reply::Result(RpcResult::CancelAcknowledged { .. })) {
            self.session.acknowledge_cancel(&iid);
        }
        if self.slot.wait(left()).is_some() {
            return;
        }
        // Not stopped cooperatively: terminate the owned tree (its other invocations end
        // unknown too); nothing is claimed about what it did.
        self.host.start_failed(
            &self.instance,
            "CancellationIgnored",
            "an invocation did not stop within its cancellation grace",
        );
    }
    fn release(&self) {
        if !self.released.swap(true, Ordering::SeqCst) {
            self.session.forget(&self.slot.invocation_id);
            self.host.release(&self.instance);
        }
    }
}

/// A running invocation.
pub struct InvocationHandle {
    run: Arc<Running>,
}
impl InvocationHandle {
    pub fn invocation_id(&self) -> &str {
        &self.run.slot.invocation_id
    }
    pub fn token(&self) -> &str {
        &self.run.slot.token
    }
    pub fn session_id(&self) -> &str {
        &self.run.session.id
    }
    /// Progress texts so far (bounded) and how many were dropped.
    pub fn progress(&self) -> (Vec<String>, i64) {
        let s = self
            .run
            .slot
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        (
            s.progress.iter().map(|(_, t)| t.clone()).collect(),
            s.dropped_progress,
        )
    }
    /// Waits up to `timeout` for the outcome.
    pub fn wait(&self, timeout: Duration) -> Option<InvokeOutcome> {
        self.run.slot.wait(timeout).map(outcome)
    }
    /// Cancels (see [`Running::cancel`]) and returns what is proven about the outcome.
    pub fn cancel(&self) -> InvokeOutcome {
        self.run.cancel();
        match self.run.slot.wait(self.run.host.config.cancel_grace * 4) {
            Some(r) => outcome(r),
            None => InvokeOutcome::Unknown("the helper could not be stopped".into()),
        }
    }
}

fn outcome(r: Reply) -> InvokeOutcome {
    match r {
        Reply::Result(RpcResult::InvocationFinished { outcome, .. }) => {
            InvokeOutcome::Finished(outcome)
        }
        Reply::Result(other) => InvokeOutcome::Failed(format!("unexpected answer {other:?}")),
        Reply::Error(e) => InvokeOutcome::Failed(e.message),
        Reply::Lost(why) => InvokeOutcome::Unknown(why),
    }
}

fn wid(s: &str) -> wire::Id {
    wire::Id::try_from(s.to_owned()).expect("host identifiers are valid wire IDs")
}
fn rev_w(n: i64) -> wire::Revision {
    wire::Revision::try_from(n.max(1).to_string()).expect("positive revision")
}

#[derive(Default)]
struct Barriers {
    gates: HashMap<String, u32>,
    in_flight: HashMap<String, u64>,
    stopping: bool,
}

/// Per-key start attempts (consecutive failures) and whether work was ever admitted.
#[derive(Default, Clone)]
struct Attempts {
    failures: u32,
    next_at: Option<Instant>,
    admitted: bool,
}

pub struct PackageHost {
    store: Arc<dyn ApplicationStore>,
    service: Arc<PackageService>,
    supervisor: Arc<ProcessSupervisor>,
    ids: Arc<dyn IdSource>,
    launch: LaunchPorts,
    config: HostConfig,
    runtimes: Option<RuntimePorts>,
    index: Option<IndexPorts>,
    instances: Mutex<HashMap<String, Vec<Arc<Instance>>>>,
    attempts: Mutex<HashMap<(String, Key), Attempts>>,
    barriers: Mutex<Barriers>,
    changed: Condvar,
    retry_at: Mutex<HashMap<String, (Instant, Duration)>>,
    wake: (Mutex<bool>, Condvar),
    running: AtomicBool,
    /// Advanced after every committed registry change, before its gate lifts: an admission
    /// that read the registry earlier re-reads it.
    epoch: AtomicU64,
    /// Serializes reconciliation passes.
    pass: Mutex<()>,
    me: Mutex<std::sync::Weak<PackageHost>>,
}

struct Gate {
    host: Arc<PackageHost>,
    package: String,
}
impl QuiescenceGuard for Gate {}
impl Drop for Gate {
    fn drop(&mut self) {
        let mut b = self.host.barriers.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(n) = b.gates.get_mut(&self.package) {
            *n -= 1;
            if *n == 0 {
                b.gates.remove(&self.package);
            }
        }
        drop(b);
        self.host.changed.notify_all();
    }
}

impl PackageHost {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        service: Arc<PackageService>,
        supervisor: Arc<ProcessSupervisor>,
        ids: Arc<dyn IdSource>,
        launch: LaunchPorts,
        config: HostConfig,
        runtimes: Option<RuntimePorts>,
        index: Option<IndexPorts>,
    ) -> Arc<Self> {
        let host = Arc::new(Self {
            store,
            service,
            supervisor,
            ids,
            launch,
            config,
            runtimes,
            index,
            instances: Mutex::new(HashMap::new()),
            attempts: Mutex::new(HashMap::new()),
            barriers: Mutex::new(Barriers::default()),
            changed: Condvar::new(),
            retry_at: Mutex::new(HashMap::new()),
            wake: (Mutex::new(false), Condvar::new()),
            running: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            pass: Mutex::new(()),
            me: Mutex::new(std::sync::Weak::new()),
        });
        *host.me.lock().unwrap_or_else(|p| p.into_inner()) = Arc::downgrade(&host);
        host
    }
    fn arc(&self) -> Arc<PackageHost> {
        self.me
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .upgrade()
            .expect("the package host is alive")
    }
    pub fn service(&self) -> &Arc<PackageService> {
        &self.service
    }
    /// How many registry publications this host has announced (the admission epoch).
    pub fn publications(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    fn host_ctx(&self) -> InvocationContext {
        let info = self.store.info();
        HostIngress::new(&*self.store).context(
            info.host_actor.clone(),
            PrincipalKind::Host,
            self.ids.next_id(IdKind::Command),
            Id::new("package-host").expect("valid namespace"),
            Counter::new(info.restore_epoch.max(0)).expect("non-negative"),
            Revision::new(1).expect("positive"),
        )
    }

    /// Boot step (before admission): removes interrupted publications and unreferenced install
    /// directories of any age, then reconciles (starts every enabled, available package).
    pub fn boot(&self) -> Result<usize, StorageError> {
        let removed = self.service.collect_files(&BTreeSet::new(), None)?;
        // Prerequisite runtimes are known before any helper is started.
        self.refresh_runtimes();
        self.reconcile();
        Ok(removed)
    }

    /// Starts the reconciliation loop (post-commit hints and periodic ticks) on its own thread.
    pub fn start(self: &Arc<Self>) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let host = self.clone();
        std::thread::Builder::new()
            .name("package-host".into())
            .spawn(move || {
                let mut runtimes_at = Instant::now();
                // Index sources are fetched when the loop starts (never during boot: network).
                let mut index_at: Option<Instant> = None;
                while host.running.load(Ordering::SeqCst) {
                    if index_at.is_none_or(|t| t.elapsed() >= host.config.index_interval) {
                        index_at = Some(Instant::now());
                        if host.refresh_indexes() > 0 {
                            host.reconcile();
                        }
                    }
                    if runtimes_at.elapsed() >= host.config.runtime_interval {
                        runtimes_at = Instant::now();
                        host.refresh_runtimes();
                    }
                    {
                        let (m, cv) = &host.wake;
                        let mut woken = m.lock().unwrap_or_else(|p| p.into_inner());
                        if !*woken {
                            woken = cv
                                .wait_timeout(woken, host.config.tick)
                                .unwrap_or_else(|p| p.into_inner())
                                .0;
                        }
                        *woken = false;
                    }
                    if !host.running.load(Ordering::SeqCst) {
                        break;
                    }
                    host.reconcile();
                    host.retry_staged();
                    let in_use = host.in_use();
                    let _ = host.service.collect_files(&in_use, Some(ORPHAN_AGE));
                }
            })
            .expect("spawn package host");
    }

    fn poke(&self) {
        let (m, cv) = &self.wake;
        *m.lock().unwrap_or_else(|p| p.into_inner()) = true;
        cv.notify_all();
    }

    /// `(package, digest)` of every live instance (never collected).
    fn in_use(&self) -> BTreeSet<(String, String)> {
        self.instances
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .flatten()
            .filter(|i| i.status() != InstanceStatus::Stopped)
            .map(|i| (i.package_id.clone(), i.key.digest.clone()))
            .collect()
    }

    /// The instances of a package (current and retiring), for inspection.
    pub fn instance_status(&self, package_id: &str) -> Vec<(String, InstanceStatus)> {
        self.instances
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(package_id)
            .map(|v| {
                v.iter()
                    .map(|i| (i.key.digest.clone(), i.status()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Makes the running instances match the committed registry (module documentation).
    pub fn reconcile(&self) {
        let _pass = self.pass.lock().unwrap_or_else(|p| p.into_inner());
        if self
            .barriers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stopping
        {
            return;
        }
        let svc = self.service.clone();
        let view = read_snapshot(&*self.store, &ReadOptions::default(), move |s| {
            let read = s.packages();
            let reg = svc.registry(read)?;
            let view = policy::Registry {
                packages: &reg,
                runtimes: &**svc.runtimes(),
                host: svc.platform(),
            };
            let registry = read.registry_revision()?;
            let mut desired = HashMap::new();
            for (id, (p, active)) in &reg {
                let (a, _) = view.availability(p, active.as_ref());
                if let (PackageAvailability::Ready, Some(v)) = (a, active)
                    && let Some(i) = policy::inspection(v)
                {
                    desired.insert(
                        id.clone(),
                        (
                            Key {
                                digest: v.manifest_digest.clone(),
                                generation: p.state_generation,
                                config_revision: p.config_revision,
                            },
                            (i, committed_configuration(p)),
                        ),
                    );
                }
            }
            Ok((registry, desired, reg.keys().cloned().collect::<Vec<_>>()))
        });
        let Ok((registry, desired, known)) = view else {
            return;
        };
        // Retire whatever no longer matches.
        let mut to_start = Vec::new();
        {
            let mut map = self.instances.lock().unwrap_or_else(|p| p.into_inner());
            let packages: BTreeSet<String> = map.keys().cloned().chain(known).collect();
            for pkg in packages {
                let want = desired.get(&pkg);
                let list = map.entry(pkg.clone()).or_default();
                let mut has_current = false;
                for inst in list.iter() {
                    let matches = want.is_some_and(|(k, _)| *k == inst.key);
                    match inst.status() {
                        InstanceStatus::Starting | InstanceStatus::Ready if matches => {
                            has_current = true
                        }
                        InstanceStatus::Starting | InstanceStatus::Ready => {
                            inst.set(InstanceStatus::Retiring);
                        }
                        _ => {}
                    }
                }
                if let Some((k, (i, configuration))) = want
                    && !has_current
                {
                    let attempts = self
                        .attempts
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .get(&(pkg.clone(), k.clone()))
                        .cloned()
                        .unwrap_or_default();
                    if attempts.next_at.is_none_or(|t| Instant::now() >= t) {
                        let inst = Arc::new(Instance {
                            package_id: pkg.clone(),
                            key: k.clone(),
                            registry_revision: registry,
                            inspection: i.clone(),
                            session: Mutex::new(None),
                            process: Mutex::new(None),
                            status: Mutex::new(InstanceStatus::Starting),
                            in_flight: AtomicU64::new(0),
                            admitted: AtomicU64::new(0),
                            stderr: Arc::new(Mutex::new(Vec::new())),
                            configuration: configuration.clone(),
                            accounted: AtomicBool::new(false),
                            ready_at: Mutex::new(None),
                            healthy: AtomicBool::new(false),
                        });
                        list.push(inst.clone());
                        to_start.push(inst);
                    }
                }
            }
        }
        self.changed.notify_all();
        // Disabled or uninstalled packages: cancel their invocations (disable cancels).
        let cancel: Vec<Arc<Instance>> = self
            .instances
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(pkg, _)| !desired.contains_key(*pkg))
            .flat_map(|(_, v)| v.clone())
            .filter(|i| i.status() == InstanceStatus::Retiring)
            .collect();
        for inst in cancel {
            if let Some(s) = inst.session() {
                s.fail("the package was disabled or uninstalled");
            }
        }
        self.retire_idle();
        for inst in to_start {
            self.start_instance(&inst);
        }
        self.changed.notify_all();
    }

    /// Stops every retiring instance with nothing in flight.
    fn retire_idle(&self) {
        let idle: Vec<Arc<Instance>> = self
            .instances
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .flatten()
            .filter(|i| {
                matches!(
                    i.status(),
                    InstanceStatus::Retiring | InstanceStatus::Failed(_)
                ) && i.in_flight.load(Ordering::SeqCst) == 0
            })
            .cloned()
            .collect();
        for inst in idle {
            // The same stop phases as before, all within one deadline.
            self.stop_instance(&inst, Instant::now() + self.config.cancel_grace * 4);
        }
        let mut map = self.instances.lock().unwrap_or_else(|p| p.into_inner());
        for list in map.values_mut() {
            list.retain(|i| i.status() != InstanceStatus::Stopped);
        }
        map.retain(|_, v| !v.is_empty());
    }

    /// Stops an instance within one absolute `deadline`: `host.shutdown` and its answer, the
    /// wait for the process to exit, then the cooperative stop and forced kill. Each phase gets
    /// at most the cancellation grace of what remains, recomputed before it; once the deadline
    /// has passed nothing is waited for (the tree is killed at once).
    fn stop_instance(&self, inst: &Instance, deadline: Instant) {
        let grace = self.config.cancel_grace;
        let left = || deadline.saturating_duration_since(Instant::now());
        if let Some(s) = inst.session() {
            let wait = left().min(grace);
            if !s.is_closed() && !wait.is_zero() {
                let sid = s.id.clone();
                let deadline_ms = wait.as_millis().clamp(1, 60_000) as u32;
                let _ = s.request(
                    |id| HostRequest::Shutdown {
                        jsonrpc: RpcVersion::V2,
                        id,
                        params: Shutdown {
                            process_session_id: wid(&sid),
                            deadline_ms,
                        },
                    },
                    wait,
                );
            }
            s.fail("the helper is retired");
        }
        let process = inst.take_process();
        if let Some(p) = process {
            let wait = left().min(grace);
            let exited = !wait.is_zero() && self.supervisor.wait(&p, wait).ok().flatten().is_some();
            if !exited {
                let bound = left();
                let _ = self.supervisor.cancel(&p, bound.min(grace), bound);
            }
        }
        inst.set(InstanceStatus::Stopped);
        self.log_stderr(
            inst,
            Severity::Info,
            "PackageHelperStopped",
            "The helper process stopped",
        );
    }

    fn log_stderr(&self, inst: &Instance, severity: Severity, code: &str, message: &str) {
        let tail = std::mem::take(&mut *inst.stderr.lock().unwrap_or_else(|p| p.into_inner()));
        let output = (!tail.is_empty()).then(|| String::from_utf8_lossy(&tail).into_owned());
        self.service
            .record_log(&inst.package_id, severity, code, message, output);
    }

    /// Records an invocation event in the package's history (best effort: the invocation's
    /// effects are journaled by the operations it reaches; this attributes the version used).
    fn attribute(&self, key: &(String, Value), kind: &str, mut payload: Value) {
        if let (Value::Object(m), Value::Object(k)) = (&mut payload, &key.1) {
            for (a, b) in k {
                m.insert(a.clone(), b.clone());
            }
        }
        let (package, kind) = (key.0.clone(), kind.to_owned());
        let _ = run_write(&*self.store, WriteOptions::default(), move |tx| {
            let actor = tx.store().host_actor.clone();
            let revision = tx.packages().package(&package)?.map_or(1, |p| p.revision);
            let id = devworks_core::protocol::EventId(tx.new_id(IdKind::Event));
            tx.history()
                .append(devworks_core::ports::storage::NewEvent {
                    id,
                    actor,
                    project_id: None,
                    root_id: None,
                    operation_id: None,
                    aggregate_kind: "package".into(),
                    aggregate_id: package,
                    aggregate_revision: revision.max(1),
                    kind,
                    payload,
                })
        });
    }

    /// Invocations one instance multiplexes: its declared limit, capped by the host.
    fn concurrency(&self, inst: &Instance) -> u64 {
        u64::from(
            inst.inspection
                .limits
                .max_concurrent
                .clamp(1, self.config.max_concurrency),
        )
    }

    /// Stops the loop and kills every helper tree at once (a dropped runtime; no grace).
    /// The service no longer reaches this host (it holds the host as its activity view and the
    /// host holds the service: without this the pair, and the store it owns, would outlive the
    /// runtime and keep the data root locked).
    fn detach(&self) {
        self.service
            .set_activity(Arc::new(devworks_core::ports::packages::Idle));
    }

    pub fn abandon(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.detach();
        self.barriers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stopping = true;
        self.poke();
        let all: Vec<Arc<Instance>> = self
            .instances
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain()
            .flat_map(|(_, v)| v)
            .collect();
        for inst in all {
            if let Some(s) = inst.session() {
                s.fail("the application stopped");
            }
            let process = inst.take_process();
            if let Some(p) = process {
                let _ = self
                    .supervisor
                    .cancel(&p, Duration::ZERO, Duration::from_millis(500));
            }
            inst.set(InstanceStatus::Stopped);
        }
    }

    fn release(&self, inst: &Instance) {
        inst.in_flight.fetch_sub(1, Ordering::SeqCst);
        let mut b = self.barriers.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(n) = b.in_flight.get_mut(&inst.package_id) {
            *n = n.saturating_sub(1);
        }
        drop(b);
        self.changed.notify_all();
        if inst.status() != InstanceStatus::Ready {
            self.poke();
        }
    }

    /// Records an instance's sustained health once: it stayed ready for
    /// [`HostConfig::healthy_after`] or completed an invocation. Only then does its key's
    /// failure budget reset.
    fn healthy(&self, inst: &Instance) {
        if inst.status() != InstanceStatus::Ready || inst.healthy.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(a) = self
            .attempts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_mut(&(inst.package_id.clone(), inst.key.clone()))
        {
            a.failures = 0;
            a.next_at = None;
        }
    }

    /// Every failure of an instance (startup, exit, stalled input, ignored cancellation) goes
    /// through here exactly once: it terminates the tree (its invocations end unknown), counts
    /// against the key's restart budget with backoff and, at the limit, records the failure
    /// durably (or the guarded rollback).
    fn start_failed(&self, inst: &Instance, code: &str, message: &str) {
        if inst.accounted.swap(true, Ordering::SeqCst) {
            return;
        }
        inst.set(InstanceStatus::Failed(message.to_owned()));
        if let Some(s) = inst.session() {
            s.fail(message);
        }
        let process = inst.take_process();
        if let Some(p) = process {
            let _ = self
                .supervisor
                .cancel(&p, Duration::from_millis(50), self.config.cancel_grace);
        }
        self.log_stderr(inst, Severity::Warning, code, message);
        let key = (inst.package_id.clone(), inst.key.clone());
        let mut attempts = self.attempts.lock().unwrap_or_else(|p| p.into_inner());
        let a = attempts.entry(key).or_default();
        a.failures += 1;
        a.admitted |= inst.admitted.load(Ordering::SeqCst) > 0;
        let failures = a.failures;
        let admitted = a.admitted;
        a.next_at = Some(
            Instant::now()
                + self.config.restart_backoff
                    * 2u32.saturating_pow(failures.saturating_sub(1)).min(64),
        );
        drop(attempts);
        if failures >= self.config.restart_limit {
            let changed = self.service.startup_failed(
                &inst.package_id,
                &inst.key.digest,
                code,
                message,
                i64::from(failures),
                !admitted,
            );
            // A recorded failure or a guarded rollback changed the registry: admissions that
            // read it earlier read it again (no reconciliation here: this may run inside one).
            if changed.is_ok() {
                self.epoch.fetch_add(1, Ordering::SeqCst);
            }
        }
        self.poke();
    }

    /// Launches an instance and runs the handshake (blocking).
    fn start_instance(&self, inst: &Arc<Instance>) {
        let svc = &self.service;
        let i = &inst.inspection;
        if let Err(e) = svc
            .files()
            .verify(&inst.package_id, &inst.key.digest, &i.files)
        {
            // Tampered or missing files are not retried: the failure is recorded durably at once
            // (the budget does not apply), and admissions re-read the changed registry.
            inst.accounted.store(true, Ordering::SeqCst);
            inst.set(InstanceStatus::Failed(e.to_string()));
            if svc
                .startup_failed(
                    &inst.package_id,
                    &inst.key.digest,
                    "InstallIntegrity",
                    &e.to_string(),
                    1,
                    false,
                )
                .is_ok()
            {
                self.epoch.fetch_add(1, Ordering::SeqCst);
            }
            return;
        }
        let Some(entry) = i.entrypoint(&svc.platform().target_triple) else {
            self.start_failed(inst, "Incompatible", "no helper for this platform");
            return;
        };
        let program = svc.files().path(&inst.package_id, &inst.key.digest, entry);
        let session_id = self.ids.next_id(IdKind::PackageSession).as_str().to_owned();
        let mut spec =
            ProcessSpec::new(&[program.to_string_lossy().into_owned()]).expect("program");
        spec.cwd = Some(svc.files().directory(&inst.package_id, &inst.key.digest));
        spec.clear_env = true;
        spec.env = child_environment();
        spec.env
            .push(("DEVWORKS_PACKAGE_ID".into(), inst.package_id.clone()));
        spec.env
            .push(("DEVWORKS_PACKAGE_SESSION".into(), session_id.clone()));
        spec.capture_output = true;
        spec.pipe_stdin = true;
        let ctx = self.host_ctx();
        let policy = LaunchPolicy {
            fs: &*self.launch.fs,
            roots: &self.launch.roots,
            protected_branches: &self.launch.protected_branches,
        };
        let launched = self.supervisor.launch(
            &ctx,
            LaunchRequest {
                spec,
                operation_id: None,
                lease: None,
                approved_hold: false,
            },
            &policy,
        );
        let owned = match launched {
            Ok(o) => o,
            Err(e) => {
                self.start_failed(
                    inst,
                    "LaunchFailed",
                    &format!("the helper could not be launched: {e:?}"),
                );
                return;
            }
        };
        let streams = (
            self.supervisor.take_input(&owned),
            self.supervisor.take_output(&owned),
        );
        *inst.process.lock().unwrap_or_else(|p| p.into_inner()) = Some(owned);
        let (Some(stdin), Some((stdout, stderr))) = streams else {
            self.start_failed(
                inst,
                "LaunchFailed",
                "the helper's control pipes are unavailable",
            );
            return;
        };
        spawn_tail(stderr, inst.stderr.clone(), self.config.stderr_tail);
        let broker = Arc::new(HostBroker { host: self.arc() });
        let session = Session::start(
            session_id.clone(),
            stdout,
            stdin,
            broker,
            usize::from(i.limits.queue_capacity),
        );
        *inst.session.lock().unwrap_or_else(|p| p.into_inner()) = Some(session.clone());
        match self.handshake(inst, &session) {
            Ok(()) => {
                // Recorded before it admits work, so a ready helper is always attributed.
                if let Some(p) = inst.process() {
                    let _ = svc.record_running(&inst.package_id, &inst.key.digest, &p.id);
                }
                inst.set(InstanceStatus::Ready);
                // The failure budget resets only after sustained health (see `healthy`).
                *inst.ready_at.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
                self.watch(inst.clone(), session);
            }
            Err((code, why)) => self.start_failed(inst, code, &why),
        }
        self.changed.notify_all();
    }

    fn handshake(&self, inst: &Instance, s: &Session) -> Result<(), (&'static str, String)> {
        // `host.initialize` and `host.activate` together within one handshake deadline.
        let deadline = Instant::now() + self.config.handshake;
        let left = || deadline.saturating_duration_since(Instant::now());
        let host = self.service.platform();
        let i = &inst.inspection;
        let sid = s.id.clone();
        let digest = inst.key.digest.clone();
        let reply = s.request(
            |id| HostRequest::Initialize {
                jsonrpc: RpcVersion::V2,
                id,
                params: Initialize {
                    process_session_id: wid(&sid),
                    offered_min: WireVersion {
                        major: host.protocol_major,
                        minor: 0,
                    },
                    offered_max: WireVersion {
                        major: host.protocol_major,
                        minor: host.protocol_minor,
                    },
                    expected_manifest_digest: wire::Digest::try_from(digest).expect("digest"),
                    required_features: Vec::new(),
                    max_frame_bytes: devworks_extension_protocol::frame::MAX_FRAME_BYTES as u32,
                },
            },
            left(),
        );
        let (selected, manifest_digest, contributions) = match reply {
            Reply::Result(RpcResult::Initialized {
                selected,
                manifest_digest,
                contribution_ids,
                ..
            }) => (selected, manifest_digest, contribution_ids),
            Reply::Lost(why) => {
                return Err((
                    "StartupTimeout",
                    format!("the helper did not initialize: {why}"),
                ));
            }
            other => {
                return Err((
                    "HandshakeRefused",
                    format!("the helper refused initialization: {other:?}"),
                ));
            }
        };
        let supported = selected.major == host.protocol_major
            && selected.minor <= host.protocol_minor
            && (i.protocol_min.0, i.protocol_min.1) <= (selected.major, selected.minor)
            && (selected.major, selected.minor) <= (i.protocol_max.0, i.protocol_max.1);
        if !supported {
            return Err((
                "ProtocolMismatch",
                format!(
                    "the helper selected protocol {}.{}",
                    selected.major, selected.minor
                ),
            ));
        }
        if String::from(manifest_digest) != inst.key.digest {
            return Err((
                "ManifestMismatch",
                "the helper reports a different manifest".into(),
            ));
        }
        let reported: Vec<String> = contributions
            .iter()
            .map(|c| c.as_str().to_owned())
            .collect();
        let reported_set: BTreeSet<&str> = reported.iter().map(String::as_str).collect();
        let declared: BTreeSet<&str> = i.contributions.iter().map(|c| c.id.as_str()).collect();
        if reported_set != declared || reported_set.len() != reported.len() {
            return Err((
                "InventoryMismatch",
                "the helper's contribution inventory differs from its manifest".into(),
            ));
        }
        let (registry, generation, config) = (
            inst.registry_revision,
            inst.key.generation,
            inst.key.config_revision,
        );
        let configuration = match &inst.configuration {
            Some((schema, value)) => Some(ActivateConfiguration {
                schema: serde_json::from_value(schema.clone()).map_err(|e| {
                    (
                        "ConfigurationInvalid",
                        format!(
                            "the committed configuration schema is not a schema reference: {e}"
                        ),
                    )
                })?,
                value: value.clone(),
            }),
            None => None,
        };
        let reply = s.request(
            |id| HostRequest::Activate {
                jsonrpc: RpcVersion::V2,
                id,
                params: Activate {
                    process_session_id: wid(&sid),
                    registry_revision: rev_w(registry),
                    state_generation: rev_w(generation),
                    configuration_revision: rev_w(config.max(1)),
                    configuration,
                },
            },
            left(),
        );
        match reply {
            Reply::Result(RpcResult::Activated {
                registry_revision,
                state_generation,
            }) if registry_revision == rev_w(registry) && state_generation == rev_w(generation) => {
                Ok(())
            }
            Reply::Lost(why) => Err((
                "StartupTimeout",
                format!("the helper did not activate: {why}"),
            )),
            other => Err((
                "ActivationMismatch",
                format!("the helper did not activate the committed generation: {other:?}"),
            )),
        }
    }

    /// Watches a ready instance: a session that fails or a process that exits while it was
    /// expected to run is a failure (restarted with backoff).
    fn watch(&self, inst: Arc<Instance>, session: Arc<Session>) {
        let host = self.arc();
        std::thread::Builder::new()
            .name(format!("package-watch-{}", inst.package_id))
            .spawn(move || {
                loop {
                    if session.wait_closed(Duration::from_millis(200)) {
                        break;
                    }
                    if inst
                        .ready_at
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .is_some_and(|t| t.elapsed() >= host.config.healthy_after)
                    {
                        host.healthy(&inst);
                    }
                    // A helper that stopped reading its input is terminated (never waited on).
                    if inst.status() == InstanceStatus::Ready
                        && session.write_stalled(host.config.cancel_grace * 2)
                    {
                        host.start_failed(
                            &inst,
                            "HelperStalled",
                            "the helper stopped reading its input",
                        );
                    }
                    if !host.running.load(Ordering::SeqCst)
                        && host
                            .barriers
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .stopping
                    {
                        return;
                    }
                }
                match inst.status() {
                    InstanceStatus::Ready => {
                        let why = session
                            .failure()
                            .unwrap_or_else(|| "the helper exited".into());
                        host.start_failed(&inst, "HelperExited", &why);
                    }
                    InstanceStatus::Retiring
                    | InstanceStatus::Failed(_)
                    | InstanceStatus::Stopped
                    | InstanceStatus::Starting => {}
                }
                host.poke();
            })
            .expect("spawn package watch");
    }

    /// Retries staged versions waiting only for quiescence or compatibility, with backoff per
    /// package.
    fn retry_staged(&self) {
        let svc = self.service.clone();
        let staged = read_snapshot(&*self.store, &ReadOptions::default(), move |s| {
            let read = s.packages();
            let mut out = Vec::new();
            for (id, (p, _)) in svc.registry(read)? {
                if let Some(d) = &p.pending_digest
                    && let Some(v) = read.package_version(&id, d)?
                    && devworks_core::services::packages::retryable(&v.waits)
                {
                    out.push(id);
                }
            }
            Ok(out)
        });
        for pkg in staged.unwrap_or_default() {
            let now = Instant::now();
            let due = self
                .retry_at
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&pkg)
                .is_none_or(|(t, _)| now >= *t);
            if !due {
                continue;
            }
            let activated = match self.quiesce(&pkg, self.config.quiesce_attempt) {
                Some(guard) => {
                    let r = self.service.retry_pending(&pkg).unwrap_or(false);
                    if r {
                        // As at a command's settlement: the publication is announced (the
                        // epoch advances) before the barrier lifts, so an admission that read
                        // the old registry reads it again instead of entering the old instance.
                        self.published();
                    }
                    drop(guard);
                    r
                }
                None => false,
            };
            let mut retry = self.retry_at.lock().unwrap_or_else(|p| p.into_inner());
            if activated {
                retry.remove(&pkg);
            } else {
                let backoff = retry.get(&pkg).map_or(Duration::from_secs(5), |(_, b)| {
                    (*b * 2).min(Duration::from_secs(300))
                });
                retry.insert(pkg, (now + backoff, backoff));
            }
        }
        self.reconcile();
    }

    /// Admits an invocation (module documentation).
    pub fn invoke(&self, req: InvokeRequest) -> Result<InvocationHandle, InvokeRefusal> {
        let until = Instant::now() + self.config.admission_wait;
        let project = match &req.scope {
            InvocationScope::Workspace => None,
            InvocationScope::Project { project_id }
            | InvocationScope::Root { project_id, .. }
            | InvocationScope::Assignment { project_id, .. } => {
                Some(project_id.0.as_str().to_owned())
            }
        };
        let mut started = false;
        loop {
            let epoch = self.epoch.load(Ordering::SeqCst);
            // The current registry pointer and availability.
            let svc = self.service.clone();
            let pkg_id = req.package_id.clone();
            let current = read_snapshot(&*self.store, &ReadOptions::default(), move |s| {
                let read = s.packages();
                let reg = svc.registry(read)?;
                let Some((p, active)) = reg.get(&pkg_id).cloned() else {
                    return Ok(None);
                };
                let view = policy::Registry {
                    packages: &reg,
                    runtimes: &**svc.runtimes(),
                    host: svc.platform(),
                };
                let (a, why) = view.availability(&p, active.as_ref());
                Ok(Some((p, a, why)))
            })
            .map_err(|e| InvokeRefusal::Storage(format!("{e:?}")))?;
            let Some((p, availability, why)) = current else {
                return Err(InvokeRefusal::Unavailable(
                    PackageAvailability::NotInstalled,
                    "The package is not installed".into(),
                ));
            };
            if !matches!(availability, PackageAvailability::Ready) {
                return Err(InvokeRefusal::Unavailable(availability, why));
            }
            if let Some(pr) = &project
                && project_disabled(&p, pr)
            {
                return Err(InvokeRefusal::ProjectDisabled);
            }
            let key = Key {
                digest: p.active_digest.clone().unwrap_or_default(),
                generation: p.state_generation,
                config_revision: p.config_revision,
            };
            // Admission is atomic with the barrier: both are decided under the barrier lock.
            {
                let mut b = self.barriers.lock().unwrap_or_else(|p| p.into_inner());
                if b.stopping {
                    return Err(InvokeRefusal::ShuttingDown);
                }
                if !b.gates.contains_key(&req.package_id) {
                    let inst = self
                        .instances
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .get(&req.package_id)
                        .and_then(|v| {
                            // A ready instance of the key first; a failed one only when no other.
                            v.iter()
                                .find(|i| i.key == key && i.status() == InstanceStatus::Ready)
                                .or_else(|| {
                                    v.iter().find(|i| {
                                        i.key == key
                                            && !matches!(i.status(), InstanceStatus::Failed(_))
                                    })
                                })
                                .or_else(|| v.iter().find(|i| i.key == key))
                                .cloned()
                        });
                    match inst {
                        // A registry published since this pass read it: read it again.
                        _ if self.epoch.load(Ordering::SeqCst) != epoch => {
                            drop(b);
                            continue;
                        }
                        Some(inst)
                            if inst.status() == InstanceStatus::Ready
                                && inst.in_flight.load(Ordering::SeqCst)
                                    >= self.concurrency(&inst) => {}
                        Some(inst) if inst.status() == InstanceStatus::Ready => {
                            if !inst
                                .inspection
                                .contributions
                                .iter()
                                .any(|c| c.id == req.contribution_id)
                            {
                                return Err(InvokeRefusal::NoSuchContribution);
                            }
                            *b.in_flight.entry(req.package_id.clone()).or_default() += 1;
                            inst.in_flight.fetch_add(1, Ordering::SeqCst);
                            inst.admitted.fetch_add(1, Ordering::SeqCst);
                            drop(b);
                            return self.send_invoke(inst, req, &p);
                        }
                        Some(inst)
                            if matches!(inst.status(), InstanceStatus::Failed(_)) && started =>
                        {
                            return Err(InvokeRefusal::NotReady(format!(
                                "the helper failed: {:?}",
                                inst.status()
                            )));
                        }
                        Some(_) => {}
                        None => {
                            if !started {
                                started = true;
                                drop(b);
                                if self.running.load(Ordering::SeqCst) {
                                    self.poke();
                                } else {
                                    self.reconcile();
                                }
                                continue;
                            }
                        }
                    }
                }
                let now = Instant::now();
                if now >= until {
                    return Err(InvokeRefusal::NotReady(
                        "no ready helper within the admission wait".into(),
                    ));
                }
                let _ = self
                    .changed
                    .wait_timeout(b, (until - now).min(Duration::from_millis(100)))
                    .unwrap_or_else(|p| p.into_inner());
            }
        }
    }

    fn send_invoke(
        &self,
        inst: Arc<Instance>,
        req: InvokeRequest,
        p: &StoredPackage,
    ) -> Result<InvocationHandle, InvokeRefusal> {
        let Some(session) = inst.session() else {
            self.release(&inst);
            return Err(InvokeRefusal::NotReady("the helper session is gone".into()));
        };
        let invocation_id = self.ids.next_id(IdKind::Invocation).as_str().to_owned();
        let token = self.ids.next_id(IdKind::Invocation).as_str().to_owned();
        // An event invocation's commit binding is what the host minted here, never what the
        // caller or the package names.
        let kind = match req.kind {
            InvocationKind::Event(mut inv) => {
                inv.package_id = req.package_id.clone();
                inv.process_session_id = session.id.clone();
                inv.invocation_id = invocation_id.clone();
                inv.invocation_token = token.clone();
                inv.state_generation = p.state_generation;
                InvocationKind::Event(inv)
            }
            k => k,
        };
        let slot = Arc::new(Slot {
            invocation_id: invocation_id.clone(),
            token: token.clone(),
            package_id: req.package_id.clone(),
            contribution_id: req.contribution_id.clone(),
            revoked: AtomicBool::new(false),
            state: Mutex::new(Default::default()),
            changed: Condvar::new(),
            context: Box::new(SlotContext {
                package_id: req.package_id.clone(),
                state_generation: p.state_generation,
                kind,
                access: inst.inspection.access.iter().cloned().collect(),
            }),
        });
        let (req_package, req_contribution) = (req.package_id.clone(), req.contribution_id.clone());
        let operation = req.operation_id.as_str().to_owned();
        let scope_json = serde_json::to_value(&req.scope).unwrap_or(Value::Null);
        let contribution =
            wire::QualifiedId::try_from(req.contribution_id.clone()).map_err(|e| {
                self.release(&inst);
                InvokeRefusal::Storage(e.to_owned())
            })?;
        let (sid, registry, generation) = (
            session.id.clone(),
            inst.registry_revision,
            inst.key.generation,
        );
        let deadline_ms = req.deadline.as_millis().clamp(1, 86_400_000) as u32;
        let wait_for = req.deadline;
        let sent = session.invoke(slot.clone(), |id| HostRequest::Invoke {
            jsonrpc: RpcVersion::V2,
            id,
            params: Invoke {
                process_session_id: wid(&sid),
                invocation_id: wid(&invocation_id),
                invocation_token: wid(&token),
                contribution_id: contribution,
                scope: req.scope,
                allowed_operations: req.allowed_operations,
                registry_revision: rev_w(registry),
                state_generation: rev_w(generation),
                authority_revision: rev_w(1),
                control_generation: rev_w(1),
                operation_id: wire::OperationId(wid(req.operation_id.as_str())),
                deadline_ms,
                input: req.input,
            },
        });
        if let Err(e) = sent {
            self.release(&inst);
            return Err(InvokeRefusal::NotReady(e));
        }
        self.attribute(
            &run_key(&inst, &req_package, &req_contribution),
            "PackageInvoked",
            json!({"invocationId": invocation_id, "operationId": operation, "scope": scope_json}),
        );
        let run = Arc::new(Running {
            host: self.arc(),
            instance: inst,
            session,
            slot,
            cancelling: AtomicBool::new(false),
            released: AtomicBool::new(false),
        });
        // The host enforces the deadline (monotonic clock) and releases the admission once the
        // outcome is terminal.
        let (watched, deadline) = (run.clone(), wait_for);
        std::thread::Builder::new()
            .name("package-invocation".into())
            .spawn(move || {
                if watched.slot.wait(deadline).is_none() {
                    watched.cancel();
                }
                while watched.slot.wait(Duration::from_secs(1)).is_none() {
                    if watched.session.is_closed() {
                        break;
                    }
                }
                let ended = match watched.slot.terminal() {
                    Some(Reply::Result(RpcResult::InvocationFinished { outcome, .. })) => {
                        watched.host.healthy(&watched.instance);
                        serde_json::to_value(&outcome)
                            .ok()
                            .and_then(|o| o["kind"].as_str().map(str::to_owned))
                            .unwrap_or_else(|| "Finished".into())
                    }
                    Some(Reply::Error(_)) => "Failed".into(),
                    _ => "OutcomeUnknown".into(),
                };
                watched.host.attribute(
                    &run_key(
                        &watched.instance,
                        &watched.slot.package_id,
                        &watched.slot.contribution_id,
                    ),
                    "PackageInvocationEnded",
                    json!({"invocationId": watched.slot.invocation_id, "outcome": ended,
                        "cancelled": watched.cancelling.load(Ordering::SeqCst)}),
                );
                watched.release();
            })
            .map_err(|e| InvokeRefusal::NotReady(e.to_string()))?;
        Ok(InvocationHandle { run })
    }

    /// Cooperative shutdown of every helper (before the process owner's own bound): admission
    /// closes, in-flight invocations are cancelled, each helper gets `host.shutdown`, then the
    /// remaining trees are stopped.
    pub fn shutdown(&self, bound: Duration) {
        self.running.store(false, Ordering::SeqCst);
        self.detach();
        self.poke();
        {
            let mut b = self.barriers.lock().unwrap_or_else(|p| p.into_inner());
            b.stopping = true;
        }
        self.changed.notify_all();
        let all: Vec<Arc<Instance>> = self
            .instances
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .flatten()
            .cloned()
            .collect();
        let deadline = Instant::now() + bound;
        for inst in &all {
            inst.set(InstanceStatus::Retiring);
        }
        for inst in all {
            // One deadline for every helper and every stop phase; the process owner stops
            // whatever is left.
            self.stop_instance(&inst, deadline);
        }
        self.instances
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }
}

/// The attribution of an invocation: package, version digest, state generation, configuration
/// revision and contribution.
fn run_key(inst: &Instance, package: &str, contribution: &str) -> (String, Value) {
    (
        package.to_owned(),
        json!({"digest": inst.key.digest, "stateGeneration": inst.key.generation,
            "configurationRevision": inst.key.config_revision, "contributionId": contribution}),
    )
}

/// The package's committed configuration `(schema, value)`, when one was saved against its
/// own schema (host-defined schemas configure the host, not the helper).
fn committed_configuration(p: &StoredPackage) -> Option<(Value, Value)> {
    let c = p.config.as_ref()?;
    match (c.get("schema"), c.get("value")) {
        (Some(schema), Some(value)) if schema.is_object() => Some((schema.clone(), value.clone())),
        _ => None,
    }
}

fn project_disabled(p: &StoredPackage, project: &str) -> bool {
    p.config
        .as_ref()
        .and_then(|c| c["disabledProjects"].as_array())
        .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(project)))
}

/// The environment a helper starts with: no inherited variables except what locates the
/// system and a temporary directory, so stored credentials in the host's environment never
/// reach package code.
pub(crate) fn child_environment() -> Vec<(String, String)> {
    // Windows also needs what locates the system, its command interpreter (for `.cmd` shims)
    // and the per-user program directories runtime launchers live in.
    const KEEP: [&str; 18] = [
        "PATH",
        "HOME",
        "TMPDIR",
        "TEMP",
        "TMP",
        "SystemRoot",
        "SystemDrive",
        "windir",
        "COMSPEC",
        "PATHEXT",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramData",
        "LANG",
        "LC_ALL",
    ];
    // Windows variable names are case-insensitive.
    std::env::vars()
        .filter(|(k, _)| {
            KEEP.iter().any(|w| {
                if cfg!(windows) {
                    w.eq_ignore_ascii_case(k)
                } else {
                    w == k
                }
            })
        })
        .collect()
}

fn spawn_tail(mut stderr: Box<dyn Read + Send>, into: Arc<Mutex<Vec<u8>>>, keep: usize) {
    let _ = std::thread::Builder::new()
        .name("package-stderr".into())
        .spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match stderr.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        let mut t = into.lock().unwrap_or_else(|p| p.into_inner());
                        t.extend_from_slice(&buf[..n]);
                        if t.len() > keep {
                            let cut = t.len() - keep;
                            t.drain(..cut);
                        }
                    }
                }
            }
        });
}

impl PackageActivity for PackageHost {
    fn quiesce(&self, package_id: &str, wait: Duration) -> Option<Box<dyn QuiescenceGuard>> {
        let until = Instant::now() + wait;
        let mut b = self.barriers.lock().unwrap_or_else(|p| p.into_inner());
        *b.gates.entry(package_id.to_owned()).or_default() += 1;
        let guard = Gate {
            host: self.arc(),
            package: package_id.to_owned(),
        };
        loop {
            if b.in_flight.get(package_id).copied().unwrap_or(0) == 0 {
                drop(b);
                return Some(Box::new(guard));
            }
            let now = Instant::now();
            if now >= until {
                drop(b);
                // Dropping the gate lifts the barrier again; nothing was interrupted.
                drop(guard);
                return None;
            }
            b = self
                .changed
                .wait_timeout(b, until - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }
    fn fence(&self, package_id: &str) -> Box<dyn QuiescenceGuard> {
        let mut b = self.barriers.lock().unwrap_or_else(|p| p.into_inner());
        *b.gates.entry(package_id.to_owned()).or_default() += 1;
        drop(b);
        Box::new(Gate {
            host: self.arc(),
            package: package_id.to_owned(),
        })
    }
    fn busy(&self, package_id: &str) -> bool {
        self.barriers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .in_flight
            .get(package_id)
            .copied()
            .unwrap_or(0)
            > 0
    }
    fn published(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.poke();
        if !self.running.load(Ordering::SeqCst) {
            // Without the loop (tests, boot), reconcile on the caller's thread.
            self.reconcile();
        }
    }
}

/// The broker of a host's sessions.
struct HostBroker {
    host: Arc<PackageHost>,
}

fn effectful(call: &BrokerCall) -> bool {
    !matches!(call, BrokerCall::ArtifactRead { .. })
}

impl Broker for HostBroker {
    fn handle(
        &self,
        session: &Session,
        request: &PackageRequest,
    ) -> Result<RpcResult, devworks_extension_protocol::package::RpcError> {
        let (process, invocation, token) = match request {
            PackageRequest::Call { params, .. } => (
                &params.process_session_id,
                &params.invocation_id,
                &params.invocation_token,
            ),
            PackageRequest::State { params, .. } => (
                &params.process_session_id,
                &params.invocation_id,
                &params.invocation_token,
            ),
            PackageRequest::CommitEvent { params, .. } => (
                &params.process_session_id,
                &params.invocation_id,
                &params.invocation_token,
            ),
        };
        // Identity and token are checked against host state; nothing the package sends grants.
        if process.as_str() != session.id {
            return Err(rpc_error(
                RpcErrorClass::PermissionDenied,
                "the request names another process session",
            ));
        }
        let Some(slot) = session.slot(invocation.as_str(), token.as_str()) else {
            return Err(rpc_error(
                RpcErrorClass::PermissionDenied,
                "the invocation is not live or its token is revoked",
            ));
        };
        let Some(ctx) = slot.context.downcast_ref::<SlotContext>() else {
            return Err(rpc_error(RpcErrorClass::Unavailable, "no host context"));
        };
        let event = matches!(ctx.kind, InvocationKind::Event(_));
        match request {
            PackageRequest::Call { params, .. } => {
                if event && effectful(&params.call) {
                    return Err(rpc_error(
                        RpcErrorClass::PermissionDenied,
                        "an event handler's effects are intents of broker.commit_event",
                    ));
                }
                Err(rpc_error(
                    RpcErrorClass::Unavailable,
                    "brokered host calls are not available in this host build",
                ))
            }
            PackageRequest::State { params, .. } => {
                let pinned: i64 = String::from(params.state_generation.clone())
                    .parse()
                    .unwrap_or(0);
                if pinned != ctx.state_generation {
                    return Err(rpc_error(
                        RpcErrorClass::Conflict,
                        "the state generation is not the one pinned for this invocation",
                    ));
                }
                match &params.action {
                    StateAction::Read => self.read_state(&ctx.package_id, pinned),
                    StateAction::CompareAndSet {
                        expected_revision,
                        proposal,
                    } => {
                        if event {
                            return Err(rpc_error(
                                RpcErrorClass::PermissionDenied,
                                "an event handler changes state only through broker.commit_event",
                            ));
                        }
                        if !ctx.access.contains("state.write") {
                            return Err(rpc_error(
                                RpcErrorClass::PermissionDenied,
                                "the package did not request state writes",
                            ));
                        }
                        let expected: i64 = String::from(expected_revision.clone())
                            .parse()
                            .unwrap_or(-1);
                        self.cas_state(&ctx.package_id, pinned, expected, proposal)
                    }
                }
            }
            PackageRequest::CommitEvent { params, .. } => {
                let InvocationKind::Event(inv) = &ctx.kind else {
                    return Err(rpc_error(
                        RpcErrorClass::PermissionDenied,
                        "only an event-handler invocation commits events",
                    ));
                };
                // A commit that changes state needs the same access as compare-and-set.
                if params.state_proposal.is_some() && !ctx.access.contains("state.write") {
                    return Err(rpc_error(
                        RpcErrorClass::PermissionDenied,
                        "the package did not request state writes",
                    ));
                }
                let (inv, commit) = (inv.as_ref().clone(), params.clone());
                let outcome = run_write(&*self.host.store, WriteOptions::default(), move |tx| {
                    commit_event(tx, &inv, &commit)
                });
                match outcome {
                    Ok(CommitOutcome::Accepted(r)) | Ok(CommitOutcome::Replayed(r)) => Ok(r),
                    Ok(CommitOutcome::Refused(why)) => Err(match why {
                        CommitRefusal::Invalid(e) => rpc_error(RpcErrorClass::ProtocolViolation, e),
                        CommitRefusal::BindingMismatch | CommitRefusal::StaleInvocation => {
                            rpc_error(RpcErrorClass::PermissionDenied, &format!("{why:?}"))
                        }
                        other => rpc_error(RpcErrorClass::Conflict, &format!("{other:?}")),
                    }),
                    Err(e) => Err(rpc_error(RpcErrorClass::OutcomeUnknown, &format!("{e:?}"))),
                }
            }
        }
    }
}

impl HostBroker {
    fn read_state(
        &self,
        package: &str,
        generation: i64,
    ) -> Result<RpcResult, devworks_extension_protocol::package::RpcError> {
        let pkg = package.to_owned();
        let state = read_snapshot(&*self.host.store, &ReadOptions::default(), move |s| {
            s.packages().package_state(&pkg)
        })
        .map_err(|e| rpc_error(RpcErrorClass::Unavailable, &format!("{e:?}")))?;
        match state {
            Some(st) if st.generation == generation => {
                Ok(snapshot(&st.state, generation, st.revision))
            }
            _ => Err(rpc_error(
                RpcErrorClass::Conflict,
                "the pinned state generation is no longer current",
            )),
        }
    }
    fn cas_state(
        &self,
        package: &str,
        generation: i64,
        expected: i64,
        proposal: &wire::ArtifactRef,
    ) -> Result<RpcResult, devworks_extension_protocol::package::RpcError> {
        let pkg = package.to_owned();
        let value = json!({"artifact": proposal});
        let written = run_write(&*self.host.store, WriteOptions::default(), move |tx| {
            match tx.packages().package_state(&pkg)? {
                Some(s) if s.generation != generation => {
                    return Err(StorageError::LeaseLost {
                        kind: "package_state",
                        id: pkg.clone(),
                    });
                }
                None => {
                    return Err(StorageError::LeaseLost {
                        kind: "package_state",
                        id: pkg.clone(),
                    });
                }
                _ => {}
            }
            tx.packages()
                .compare_and_set_state(&pkg, generation, expected, value)
        });
        match written {
            Ok(st) => Ok(snapshot(&st.state, generation, st.revision)),
            Err(StorageError::LeaseLost { .. }) => Err(rpc_error(
                RpcErrorClass::Conflict,
                "the state generation was replaced",
            )),
            Err(StorageError::RevisionConflict(_)) => Err(rpc_error(
                RpcErrorClass::Conflict,
                "the state revision changed",
            )),
            Err(e) => Err(rpc_error(RpcErrorClass::OutcomeUnknown, &format!("{e:?}"))),
        }
    }
}

fn snapshot(state: &Value, generation: i64, revision: i64) -> RpcResult {
    RpcResult::StateSnapshot {
        state_generation: rev_w(generation),
        revision: rev_w(revision.max(1)),
        artifact: serde_json::from_value(state["artifact"].clone()).ok(),
    }
}
