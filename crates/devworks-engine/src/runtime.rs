//! The engine's one boot entry point: [`boot`] recovers durable state in a fixed order (queue,
//! artifacts, owned processes and resource leases, run roots), applies keep-awake, records what
//! recovery established as a diagnostic, starts every required consumer, artifact collection,
//! resource housekeeping, run supervision and the diagnostics monitor, and only then returns the
//! [`Runtime`] through which commands and queries are admitted. See the crate documentation ("Boot
//! sequence") for the order and why each step sits where it does.
use crate::diagnostics::{self as diag, Monitor};
use crate::dispatch::{self, Consumer, Dispatcher, DispatcherConfig, RecoveryReport};
use crate::package_host::{HostConfig, LaunchPorts, PackageHost};
use crate::replies::reply_consumer;
use crate::resources::{BootReport as ResourceBootReport, Housekeeper};
use crate::runs::{ActivationLauncher, RunGate, RunSupervisor};
use devworks_core::context::InvocationContext;
use devworks_core::gateway::{CommandOutcome, Gateway, QueryOutcome};
use devworks_core::ports::files::FileFault;
use devworks_core::ports::process::{PowerPort, ProcessHost};
use devworks_core::ports::storage::{ApplicationStore, StorageError, Unavailable, WriteOptions};
use devworks_core::ports::{AuthorityPort, Clock, IdSource};
use devworks_core::protocol::results::DomainDecision;
use devworks_core::protocol::{CommandReply, Id};
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::artifacts::lifecycle::{
    CollectReport, RecoveryReport as ArtifactRecoveryReport,
};
use devworks_core::services::artifacts::replies::ReceiptReplies;
use devworks_core::services::diagnostics::{DiagnosticService, ExportFailure, LogQuery};
use devworks_core::services::packages::PackageService;
use devworks_core::services::power::{KeepAwake, KeepAwakeStatus};
use devworks_core::services::process::{ProcessSupervisor, ShutdownReport};
use devworks_core::services::resources::{MachineResource, ResourceService, desktop};
use devworks_core::services::runs::ControlService;
use devworks_core::services::runs::recovery::{BootRecovery, LifecycleHooks, NoLifecycleHooks};
use devworks_core::settings::view::Health;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

/// The operation whose applied decision may leave blobs pending physical removal.
const RELEASES_BLOBS: &str = "artifacts.delete";
/// The operation through which the `power.keepAwake` setting changes.
const SAVES_SETTINGS: &str = "settings.save_revision";
/// The operation that records a reported diagnostic (its commit raises the live-log hint).
const RECORDS_DIAGNOSTIC: &str = "workspace.record_diagnostic";

/// Run root control as composed: the one control service (its plan/readiness port inside), the
/// coordinator launcher (none until the agent supervisor exists: accepted activations then stay
/// pending and are held at the next boot) and the schedules/keep-working lifecycle hooks.
#[derive(Clone)]
pub struct RunComposition {
    pub service: Arc<ControlService>,
    pub launcher: Option<Arc<dyn ActivationLauncher>>,
    pub hooks: Arc<dyn LifecycleHooks>,
}
impl RunComposition {
    pub fn new(service: Arc<ControlService>) -> Self {
        Self {
            service,
            launcher: None,
            hooks: Arc::new(NoLifecycleHooks),
        }
    }
}

/// The package lifecycle as composed: the one package service (registered for the `addons.*`
/// use cases) and what its host needs to launch helpers.
pub struct PackageComposition {
    pub service: Arc<PackageService>,
    pub launch: LaunchPorts,
    pub config: HostConfig,
    /// Runtime probing (the cache must be the service's runtime probe).
    pub runtimes: Option<crate::package_host::RuntimePorts>,
    /// Signed publisher index sources (fetch and verification).
    pub index: Option<crate::package_host::IndexPorts>,
}

/// Everything the runtime is composed from. Every part is a core port or core service; the one
/// `store` backs the gateway, the dispatcher, the reply publisher and artifact recovery and
/// collection, and the use cases the caller registered must be built over that same store.
pub struct Composition {
    pub store: Arc<dyn ApplicationStore>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdSource>,
    pub authority: Arc<dyn AuthorityPort>,
    /// The one artifact service: [`boot`] registers the artifact use cases with it (replacing any
    /// the caller registered), publishes receipt replies through it and collects with it, so its
    /// readers, stream grants and deferred staging resolutions are shared by all three.
    pub artifacts: Arc<ArtifactService>,
    /// The one resource service: [`boot`] registers the `resources.*` use cases with it (replacing
    /// any the caller registered), so dispatch tickets, owned processes and leases share one
    /// in-flight registry.
    pub resources: Arc<ResourceService>,
    /// The native process launcher every owned tree is spawned through.
    pub process_host: Arc<dyn ProcessHost>,
    /// Run root control: [`boot`] registers the `runs.*` use cases with its service (replacing
    /// any the caller registered) and gates every consumer's root-scoped deliveries with it.
    pub runs: RunComposition,
    /// The OS power facility the keep-awake setting is applied with.
    pub power: Arc<dyn PowerPort>,
    /// The one diagnostics service: [`boot`] registers the `workspace.diagnostics`, `.logs` and
    /// `.record_diagnostic` use cases with it (replacing any the caller registered), records
    /// boot recovery through it, feeds it the boot report, collection passes and consumers, and
    /// runs its retention and monitor passes.
    pub diagnostics: Arc<DiagnosticService>,
    /// The package lifecycle: [`boot`] registers the `addons.*` use cases with its service
    /// (replacing any the caller registered), starts the package host after process recovery and
    /// stops its helpers cooperatively at shutdown. `None`: no package host (packages are not
    /// offered).
    pub packages: Option<PackageComposition>,
    /// The other use cases (queue, sessions, history, settings, ...).
    pub use_cases: UseCases,
    /// Consumers besides the required ones [`boot`] registers itself (package handlers, ...).
    pub consumers: Vec<Consumer>,
    pub config: RuntimeConfig,
}

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub dispatcher: DispatcherConfig,
    /// Time between periodic artifact collection passes (one runs at boot and one after every
    /// applied `artifacts.delete` regardless). Each pass removes pending blob generations no
    /// reader or pin holds and clears the derived preview cache, so this also bounds how long a
    /// derived preview is kept.
    pub collect_interval: Duration,
    /// In-flight cap of the receipt reply consumer (`None`: the dispatcher's whole capacity).
    pub reply_max_in_flight: Option<usize>,
    /// Physically shared resources registered for the machine at boot (default: the desktop).
    pub machine_resources: Vec<MachineResource>,
    /// Time between resource housekeeping ticks (expiry, transfer, promotion, sleep/wake).
    pub housekeeping_interval: Duration,
    /// Cooperative stop period of owned trees at shutdown, within [`Self::shutdown_bound`].
    pub shutdown_grace: Duration,
    /// The one bound shared by every owned tree's stop at shutdown (AD20 default 30 s).
    pub shutdown_bound: Duration,
    /// Time between diagnostics passes (retention, integrity check, write-owner probe); one
    /// runs at boot regardless. Hourly by default: retention is counted in days, and a full
    /// integrity check reads the whole store.
    pub diagnostics_interval: Duration,
}
impl RuntimeConfig {
    pub fn new(owner: Id) -> Self {
        Self {
            dispatcher: DispatcherConfig::new(owner),
            collect_interval: Duration::from_secs(15 * 60),
            reply_max_in_flight: None,
            machine_resources: vec![desktop()],
            housekeeping_interval: Duration::from_secs(30),
            shutdown_grace: Duration::from_secs(5),
            shutdown_bound: Duration::from_secs(30),
            diagnostics_interval: Duration::from_secs(60 * 60),
        }
    }
}

/// What boot recovery established.
#[derive(Clone, Debug)]
pub struct BootReport {
    /// Queue and operation journal recovery ([`Dispatcher::recover`]).
    pub queue: RecoveryReport,
    /// Artifact staging and managed-file recovery ([`ArtifactService::recover`]).
    pub artifacts: ArtifactRecoveryReport,
    /// Machine resource registration and the recovery of owned processes and resource leases of
    /// earlier boots ([`Housekeeper::boot`]).
    pub resources: ResourceBootReport,
    /// Run-root recovery: interrupted activations and the roots held for manual Continue.
    pub runs: BootRecovery,
    /// The keep-awake setting as applied at boot.
    pub keep_awake: KeepAwakeStatus,
}

/// Why boot stopped; nothing was started and no command can be admitted.
#[derive(Clone, Debug)]
pub enum BootError {
    /// The composition is invalid (dispatcher bounds, a duplicate or reserved consumer name).
    Composition(StorageError),
    /// Queue and operation journal recovery failed.
    Queue(StorageError),
    /// Artifact recovery failed.
    Artifacts(StorageError),
    /// Machine resource registration or the owned-process and lease recovery failed.
    Resources(StorageError),
    /// Run-root recovery failed.
    Runs(StorageError),
    /// Package install-directory recovery failed.
    Packages(StorageError),
    /// The keep-awake setting could not be read.
    Settings(StorageError),
}

/// One artifact collection pass.
#[derive(Clone, Debug)]
pub struct CollectionPass {
    /// 1 for the boot pass, then increasing.
    pub pass: u64,
    pub blobs: Result<CollectReport, StorageError>,
    /// Derived payloads removed.
    pub derived: Result<u64, FileFault>,
}

struct Collector {
    trigger: Notify,
    stopping: AtomicBool,
    status: watch::Sender<Option<CollectionPass>>,
}
impl Collector {
    fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.trigger.notify_one();
    }
}

/// The booted runtime. Commands and queries block the calling (non-UI) thread on the store, as
/// the gateway does; delivery and collection run on the Tokio runtime [`boot`] was awaited on.
/// Dropping it stops delivery and collection (as [`Runtime::shutdown`] does, without waiting).
pub struct Runtime {
    store: Arc<dyn ApplicationStore>,
    ids: Arc<dyn IdSource>,
    authority: Arc<dyn AuthorityPort>,
    artifacts: Arc<ArtifactService>,
    use_cases: UseCases,
    dispatcher: Dispatcher,
    collector: Arc<Collector>,
    resources: Arc<ResourceService>,
    housekeeper: Arc<Housekeeper>,
    runs: Arc<RunSupervisor>,
    /// The shutdown stage ([`ShutdownStage`] as a byte) for the progress surface; the fence
    /// itself is `admission`.
    stage: std::sync::atomic::AtomicU8,
    /// The admission fence: every command holds it shared for its whole admission and unit of
    /// work; shutdown takes it exclusively to close it, which waits for (drains) every command
    /// already admitted. A command never commits after the run-control quiescence snapshot.
    admission: Arc<std::sync::RwLock<bool>>,
    keep_awake: Arc<KeepAwake>,
    diagnostics: Arc<DiagnosticService>,
    monitor: Arc<Monitor>,
    packages: Option<Arc<PackageHost>>,
    shutdown_grace: Duration,
    shutdown_bound: Duration,
    report: BootReport,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    /// The run supervision loop, awaited only within the shutdown bound (a root stop it is
    /// executing may be blocked on a tree).
    supervision: Mutex<Option<JoinHandle<()>>>,
    /// The diagnostics monitor task, stopped first at shutdown (see [`Runtime::shutdown`]).
    monitoring: Mutex<Option<JoinHandle<()>>>,
}

/// Boots the runtime (see the crate documentation, "Boot sequence"):
///
/// 1. composes the dispatcher with the required receipt reply consumer and the caller's
///    consumers (nothing is claimed yet);
/// 2. recovers the queue and operation journal: interrupted operations are classified and
///    claims of earlier boots requeued or held ([`Dispatcher::recover`]);
/// 3. recovers artifact staging and managed files ([`ArtifactService::recover`]);
/// 4. registers the machine resources and recovers owned processes and resource leases of
///    earlier boots ([`Housekeeper::boot`]);
/// 5. recovers run roots ([`RunSupervisor::boot`]): activations of earlier boots are recorded
///    interrupted and their roots held for manual Continue;
/// 6. applies the keep-awake setting;
/// 7. records what recovery established as a `boot` diagnostic;
/// 8. starts delivery ([`Dispatcher::run`]);
/// 9. starts artifact collection (a pass now, then periodically and after every applied
///    `artifacts.delete`), resource housekeeping ticks, run supervision ticks and the
///    diagnostics monitor (retention, store inspection, integrity check and write-owner probe
///    now, then every [`RuntimeConfig::diagnostics_interval`]), and registers the boot report,
///    collection passes and dispatcher as diagnostics health sources;
/// 10. returns the handle through which commands are admitted.
///
/// A boot that stops records why as a failed `boot` diagnostic when the store still accepts it.
///
/// Must be awaited on a Tokio runtime with timers; the tasks it starts run there.
pub async fn boot(composition: Composition) -> Result<Runtime, BootError> {
    let Composition {
        store,
        clock,
        ids,
        authority,
        artifacts,
        resources,
        process_host,
        runs,
        power,
        diagnostics,
        packages,
        use_cases,
        mut consumers,
        config,
    } = composition;
    let use_cases = match &packages {
        Some(p) => use_cases.with_packages(p.service.clone()),
        None => use_cases,
    };
    let use_cases = use_cases
        .with_artifacts(artifacts.clone())
        .with_resources(resources.clone())
        .with_runs(runs.service.clone())
        .with_diagnostics(diagnostics.clone());
    // No entry path bypasses root controls: every consumer's root-scoped work passes the
    // run-control gate (and its own port, if it has one).
    for c in &mut consumers {
        c.eligibility = Some(Arc::new(RunGate::new(
            runs.service.clone(),
            c.eligibility.take(),
        )));
    }
    let failed = |e: BootError| {
        diag::record_boot_failure(&diagnostics, &e);
        e
    };
    let mut replies = reply_consumer(
        Arc::new(ReceiptReplies::new(artifacts.clone())),
        store.clone(),
    );
    replies.max_in_flight = config.reply_max_in_flight;
    consumers.insert(0, replies);
    let run_owner = config.dispatcher.owner.clone();
    diagnostics.set_consumers(consumers.iter().map(|c| c.name.clone()).collect());
    let dispatcher = Dispatcher::new(store.clone(), clock.clone(), consumers, config.dispatcher)
        .map_err(BootError::Composition)
        .map_err(failed)?;

    let queue = dispatcher
        .recover()
        .await
        .map_err(BootError::Queue)
        .map_err(failed)?;
    let (svc, st) = (artifacts.clone(), store.clone());
    let recovered = tokio::task::spawn_blocking(move || svc.recover(&*st))
        .await
        .unwrap_or_else(|_| Err(panicked("artifact recovery")))
        .map_err(BootError::Artifacts)
        .map_err(failed)?;
    let supervisor = Arc::new(ProcessSupervisor::new(
        store.clone(),
        process_host,
        resources.clone(),
    ));
    let booted_at_ms = clock.now_ms().get();
    let housekeeper = Arc::new(Housekeeper::new(
        store.clone(),
        clock.clone(),
        resources.clone(),
        supervisor.clone(),
    ));
    let recovered_resources = housekeeper
        .boot(config.machine_resources.clone())
        .await
        .map_err(BootError::Resources)
        .map_err(failed)?;
    let mut run_supervisor = RunSupervisor::new(
        store.clone(),
        clock,
        runs.service.clone(),
        supervisor.clone(),
        runs.launcher.clone(),
        runs.hooks.clone(),
        run_owner,
    );
    // A root stop never outlasts the application's one shutdown bound.
    if let Some(s) = Arc::get_mut(&mut run_supervisor) {
        s.stop_grace = config.shutdown_grace.min(config.shutdown_bound);
        s.stop_bound = config.shutdown_bound;
    }
    let recovered_runs = run_supervisor
        .boot()
        .await
        .map_err(BootError::Runs)
        .map_err(failed)?;
    // Packages (after owned processes are recovered, before any command is admitted): remove
    // interrupted publications and unreferenced install directories, then start every enabled,
    // available package's helper against the committed registry.
    let package_host = match packages {
        Some(p) => {
            let host = PackageHost::new(
                store.clone(),
                p.service.clone(),
                supervisor.clone(),
                ids.clone(),
                p.launch,
                p.config,
                p.runtimes,
                p.index,
            );
            p.service.set_activity(host.clone());
            let booting = host.clone();
            tokio::task::spawn_blocking(move || booting.boot())
                .await
                .unwrap_or_else(|_| Err(panicked("package boot")))
                .map_err(BootError::Packages)
                .map_err(failed)?;
            Some(host)
        }
        None => None,
    };
    // Applying may start an OS helper (and wait briefly to see it hold), so off the async thread.
    let keep_awake = Arc::new(KeepAwake::new(power));
    let (ka, st) = (keep_awake.clone(), store.clone());
    let applied = tokio::task::spawn_blocking(move || {
        devworks_core::ports::storage::read_snapshot(&*st, &Default::default(), |s| {
            ka.refresh(s.configuration())
        })
    })
    .await
    .unwrap_or_else(|_| Err(panicked("keep-awake")))
    .map_err(BootError::Settings)
    .map_err(failed)?;
    let report = BootReport {
        queue,
        artifacts: recovered,
        resources: recovered_resources,
        runs: recovered_runs,
        keep_awake: applied,
    };
    let (svc, recovered) = (diagnostics.clone(), report.clone());
    let _ = tokio::task::spawn_blocking(move || diag::record_boot(&svc, &recovered)).await;

    let run = dispatcher.clone();
    let delivery = tokio::spawn(async move { run.run().await });
    if let Some(h) = &package_host {
        h.start();
    }
    let (status, _) = watch::channel(None);
    let collector = Arc::new(Collector {
        trigger: Notify::new(),
        stopping: AtomicBool::new(false),
        status,
    });
    let collection = tokio::spawn(collect(
        collector.clone(),
        artifacts.clone(),
        store.clone(),
        config.collect_interval,
    ));
    let (keeper, every) = (housekeeper.clone(), config.housekeeping_interval);
    let housekeeping = tokio::spawn(async move { keeper.run(every).await });
    run_supervisor.set_dispatcher_wakeup(dispatcher.wakeup());
    let supervision = tokio::spawn(run_supervisor.clone().run(every));
    diag::sources(
        &diagnostics,
        report.clone(),
        booted_at_ms,
        collector.status.subscribe(),
        dispatcher.clone(),
    );
    let monitor = Arc::new(Monitor::default());
    let (watcher, svc, every) = (
        monitor.clone(),
        diagnostics.clone(),
        config.diagnostics_interval,
    );
    let monitoring = tokio::spawn(async move { watcher.run(svc, every).await });
    Ok(Runtime {
        store,
        ids,
        authority,
        artifacts,
        use_cases,
        dispatcher,
        collector,
        resources,
        housekeeper,
        runs: run_supervisor,
        stage: std::sync::atomic::AtomicU8::new(ShutdownStage::Open as u8),
        admission: Arc::new(std::sync::RwLock::new(false)),
        keep_awake,
        diagnostics,
        monitor,
        packages: package_host,
        shutdown_grace: config.shutdown_grace,
        shutdown_bound: config.shutdown_bound,
        report,
        tasks: Mutex::new(vec![delivery, collection, housekeeping]),
        supervision: Mutex::new(Some(supervision)),
        monitoring: Mutex::new(Some(monitoring)),
    })
}

fn panicked(what: &str) -> StorageError {
    StorageError::unavailable(Unavailable::HandlerPanicked, format!("{what} panicked"))
}

/// Collection passes until stopped: one now, then after `interval` or a trigger, whichever comes
/// first (a trigger during a pass runs another pass right after it).
async fn collect(
    collector: Arc<Collector>,
    artifacts: Arc<ArtifactService>,
    store: Arc<dyn ApplicationStore>,
    interval: Duration,
) {
    let mut pass = 0;
    while !collector.stopping.load(Ordering::SeqCst) {
        pass += 1;
        let (svc, st) = (artifacts.clone(), store.clone());
        let (blobs, derived) =
            tokio::task::spawn_blocking(move || (svc.collect(&*st), svc.collect_derived()))
                .await
                .unwrap_or_else(|_| {
                    (
                        Err(panicked("artifact collection")),
                        Err(FileFault::Io("artifact collection panicked".into())),
                    )
                });
        collector.status.send_replace(Some(CollectionPass {
            pass,
            blobs,
            derived,
        }));
        let _ = dispatch::first(collector.trigger.notified(), tokio::time::sleep(interval)).await;
    }
}

impl Runtime {
    /// What boot recovery established.
    pub fn report(&self) -> &BootReport {
        &self.report
    }
    pub fn store(&self) -> &Arc<dyn ApplicationStore> {
        &self.store
    }
    /// The artifact service the use cases, reply publication and collection share (artifact
    /// streams are opened through it).
    pub fn artifacts(&self) -> &Arc<ArtifactService> {
        &self.artifacts
    }
    /// The resource service the `resources.*` use cases and dispatch-time checks share.
    pub fn resources(&self) -> &Arc<ResourceService> {
        &self.resources
    }
    /// The diagnostics service the `workspace.diagnostics`/`.logs`/`.record_diagnostic` use
    /// cases, host log channels (indexing, providers) and the live-log watch share.
    pub fn diagnostics(&self) -> &Arc<DiagnosticService> {
        &self.diagnostics
    }
    /// Exports the selected diagnostics (and the current health checks) as an artifact of the
    /// invocation's project: rendered bounded and redacted, staged by the diagnostics service's
    /// export staging (a person's upload ticket in production) and saved by an `artifacts.save`
    /// admitted through this runtime under `request_key`. A retry with the same key replays.
    pub fn export_diagnostics(
        &self,
        ctx: &InvocationContext,
        query: &LogQuery,
        request_key: &str,
    ) -> Result<CommandOutcome, ExportFailure> {
        self.diagnostics.export(ctx, query, request_key, |raw| {
            self.command(ctx, "artifacts.save", raw)
        })
    }
    /// The package host (helper processes, invocations and the quiescence barrier), when packages
    /// were composed.
    pub fn packages(&self) -> Option<&Arc<PackageHost>> {
        self.packages.as_ref()
    }
    /// The process supervisor every owned tree is launched through (process tools, package and
    /// helper hosts).
    pub fn processes(&self) -> &Arc<ProcessSupervisor> {
        self.housekeeper.supervisor()
    }
    /// How far shutdown has progressed (AD20: the progress surface stays responsive).
    pub fn shutdown_stage(&self) -> ShutdownStage {
        ShutdownStage::from_u8(self.stage.load(Ordering::SeqCst))
    }
    fn enter(&self, stage: ShutdownStage) {
        self.stage.store(stage as u8, Ordering::SeqCst);
    }
    /// Run supervision (activation claims, stops, recovery) over the composed control service.
    pub fn runs(&self) -> &Arc<RunSupervisor> {
        &self.runs
    }
    /// The immediate stop of a confirmed project archival (WORK-16): every root of the project is
    /// stopped under a new stop epoch and its owned trees are killed without cooperative grace.
    pub async fn stop_project_now(
        &self,
        project: &devworks_core::protocol::ProjectId,
    ) -> Result<
        Vec<(
            devworks_core::protocol::RootId,
            devworks_core::protocol::OperationState,
        )>,
        StorageError,
    > {
        let stopped = self.runs.stop_project_now(project).await;
        self.dispatcher.wakeup().wake();
        stopped
    }
    /// The keep-awake request as last applied (re-checking a held request).
    pub fn keep_awake(&self) -> KeepAwakeStatus {
        self.keep_awake.status()
    }
    /// Keep-awake runtime health for the Settings view (distinct from the configured value).
    pub fn keep_awake_health(&self) -> Health {
        self.keep_awake.health()
    }
    /// Re-applies the effective `power.keepAwake` setting. Commands through
    /// [`Self::command_with`] do this after every committed `settings.save_revision`; host code
    /// that changes settings outside the runtime's commands (proposal approval, rollback) calls
    /// it after its unit of work commits.
    pub fn refresh_keep_awake(&self) -> Result<KeepAwakeStatus, StorageError> {
        let ka = &self.keep_awake;
        devworks_core::ports::storage::read_snapshot(&*self.store, &Default::default(), |s| {
            ka.refresh(s.configuration())
        })
    }

    /// The running dispatcher (status hints, wakeups, diagnostics).
    pub fn dispatcher(&self) -> &Dispatcher {
        &self.dispatcher
    }
    /// The latest artifact collection pass (`None` until the boot pass finished).
    pub fn collections(&self) -> watch::Receiver<Option<CollectionPass>> {
        self.collector.status.subscribe()
    }
    /// Requests a collection pass now, for a change made outside the runtime's commands that can
    /// let a blob go (for example a released backup pin).
    pub fn request_collection(&self) {
        self.collector.trigger.notify_one();
    }

    fn gateway(&self) -> Gateway<'_> {
        Gateway {
            authority: &*self.authority,
            store: &*self.store,
            ids: &*self.ids,
        }
    }

    pub fn query(&self, ctx: &InvocationContext, operation: &str, raw: &[u8]) -> QueryOutcome {
        self.use_cases.query(&self.gateway(), ctx, operation, raw)
    }

    pub fn command(&self, ctx: &InvocationContext, operation: &str, raw: &[u8]) -> CommandOutcome {
        self.command_with(ctx, operation, raw, WriteOptions::default())
    }

    /// Admits a command. A command that may have committed (`Recorded`, or a lost
    /// acknowledgment) wakes delivery for the reply intent its receipt committed, an applied
    /// `artifacts.delete` starts a collection pass for the blobs it released, and an applied
    /// `settings.save_revision` re-applies keep-awake.
    pub fn command_with(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
        options: WriteOptions,
    ) -> CommandOutcome {
        // Held until the command is decided and committed (or refused): shutdown's closing
        // transition waits for it, and a command after it is refused before admission.
        let open = self.admission.read().unwrap_or_else(|p| p.into_inner());
        if *open {
            return closing_reply(
                raw,
                self.ids.next_id(devworks_core::ports::IdKind::Correlation),
            );
        }
        let outcome = self
            .use_cases
            .command_with(&self.gateway(), ctx, operation, raw, options);
        drop(open);
        let (committed, released) = match &outcome {
            Ok(CommandReply::Recorded { receipt, .. }) => (
                true,
                matches!(receipt.decision, DomainDecision::Applied { .. }),
            ),
            Ok(CommandReply::CommandStatusUnknown { .. }) => (true, true),
            _ => (false, false),
        };
        if committed {
            self.dispatcher.wakeup().wake();
            if operation.starts_with("runs.") {
                // A stop to execute, a stop to signal or a new activation to claim.
                self.runs.notify();
            }
        }
        if released && operation == RELEASES_BLOBS {
            self.collector.trigger.notify_one();
        }
        if committed && operation == RECORDS_DIAGNOSTIC {
            self.diagnostics.committed();
        }
        if released && operation == SAVES_SETTINGS {
            // A read failure leaves the previous request in place; the next save or explicit
            // refresh applies the setting again.
            let _ = self.refresh_keep_awake();
        }
        outcome
    }

    /// Stops admission of new deliveries, collection, housekeeping and the diagnostics monitor,
    /// waits for the delivery loop, the collector, the housekeeper, the monitor and every
    /// in-flight delivery to finish (an
    /// asynchronous handler gets its consumer's stop grace), then stops every owned process tree
    /// within [`RuntimeConfig::shutdown_bound`] (recording the `Shutdown` reconciliation of
    /// resources) and releases the keep-awake request. Every tree is stopped natively even when
    /// the store refuses writes; the report carries each tree's outcome and any write failure.
    pub async fn shutdown(&self) -> ShutdownReport {
        // AD20 step 1: fence admission in memory, then commit the run-control transition: live
        // feature/issue work is held for manual Continue and asked to stop; launches get the
        // cooperative grace. A storage failure here is not fatal: the next boot holds the same
        // work from its boot epoch.
        // One bound for the whole shutdown (AD20): every later step gets what is left of it.
        let deadline = tokio::time::Instant::now() + self.shutdown_bound;
        let left = || deadline.saturating_duration_since(tokio::time::Instant::now());
        // The diagnostics monitor stops first, so no retention, probe or sweep write starts
        // after admission closes. A pass in progress is awaited within the bound; past it the
        // task is abandoned (its unit of work, already admitted, commits or rolls back alone).
        self.monitor.stop();
        let monitoring = self
            .monitoring
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(mut task) = monitoring
            && let dispatch::Either::Left(()) =
                dispatch::first(tokio::time::sleep(left()), &mut task).await
        {
            task.abort();
        }
        // Close admission: wait (off the async thread) for every admitted command to commit, so
        // none commits after the quiescence snapshot.
        self.enter(ShutdownStage::DrainingAdmission);
        let admission = self.admission.clone();
        let _ = tokio::task::spawn_blocking(move || {
            *admission.write().unwrap_or_else(|p| p.into_inner()) = true;
        })
        .await;
        self.enter(ShutdownStage::Quiescing);
        let _ = self.runs.quiesce(self.shutdown_grace.min(left())).await;
        self.enter(ShutdownStage::StoppingWork);
        self.stop();
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap_or_else(|p| p.into_inner()));
        for task in tasks {
            let _ = task.await;
        }
        // Run supervision admits no further root stop once stopping; one it is executing ends
        // within its own bound (at most the shutdown bound, started before this deadline), and
        // is not waited for past the deadline: the owned-tree stop below covers its trees.
        let supervision = self
            .supervision
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(mut task) = supervision
            && let dispatch::Either::Left(()) =
                dispatch::first(tokio::time::sleep(left()), &mut task).await
        {
            task.abort();
        }
        self.dispatcher.idle().await;
        // Package helpers get their cooperative shutdown (cancel in-flight invocations,
        // host.shutdown) before the process owner stops whatever remains.
        if let Some(host) = self.packages.clone() {
            let bound = left();
            let _ = tokio::task::spawn_blocking(move || host.shutdown(bound)).await;
        }
        let bound = left();
        let stopped = self
            .housekeeper
            .shutdown(self.shutdown_grace.min(bound), bound)
            .await;
        // AD20 step 4: final known activation state.
        let _ = self.runs.finish().await;
        self.keep_awake.apply(false);
        self.enter(ShutdownStage::Done);
        stopped
    }

    fn stop(&self) {
        self.dispatcher.stop();
        self.collector.stop();
        self.housekeeper.stop();
        self.runs.stop();
        self.monitor.stop();
    }
}

/// The stages of [`Runtime::shutdown`], in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum ShutdownStage {
    Open = 0,
    /// Admission is closing: commands already admitted are finishing; new ones are refused.
    DrainingAdmission = 1,
    /// Live feature/issue work is being held for manual Continue and asked to stop.
    Quiescing = 2,
    /// Delivery, collection, housekeeping and owned process trees are being stopped.
    StoppingWork = 3,
    Done = 4,
}
impl ShutdownStage {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Open,
            1 => Self::DrainingAdmission,
            2 => Self::Quiescing,
            3 => Self::StoppingWork,
            _ => Self::Done,
        }
    }
}

/// The reply to a command submitted after shutdown began: refused before admission (nothing is
/// recorded), addressed to its request key when the frame names one.
fn closing_reply(raw: &[u8], correlation: Id) -> CommandOutcome {
    use devworks_core::protocol::{CorrelationId, Protocol, Rejection, RequestKey, RetryAdvice};
    let key = serde_json::from_slice::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| {
            v.get("requestKey")
                .and_then(|k| k.as_str())
                .map(str::to_owned)
        })
        .and_then(|k| Id::new(k).ok());
    match key {
        Some(k) => Ok(CommandReply::Rejected {
            protocol: Protocol::Draft1,
            request_key: RequestKey(k),
            code: Rejection::StorageUnavailable,
            retry: RetryAdvice::Never,
            correlation_id: CorrelationId(correlation),
        }),
        None => Err(devworks_core::gateway::GatewayFailure::Unaddressable {
            error: devworks_core::admission::AdmissionError::InvalidEnvelope(
                "the application is shutting down".into(),
            ),
            correlation_id: CorrelationId(correlation),
        }),
    }
}

/// Dropping the runtime also drops the process supervisor (each owned tree it still holds is
/// killed) and the keep-awake request.
impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop();
        // Package helpers are killed at once (no grace): their host threads would otherwise
        // keep the process owner, and so the helper trees, alive.
        if let Some(h) = &self.packages {
            h.abandon();
        }
        // Launched coordinators are signalled and aborted: nothing renews or writes after drop.
        self.runs.abort_launches();
        self.keep_awake.apply(false);
    }
}
