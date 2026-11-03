//! The engine's one boot entry point: [`boot`] recovers durable state in a fixed order (queue,
//! artifacts, owned processes and resource leases, run roots), applies keep-awake, starts every
//! required consumer, artifact collection, resource housekeeping and run supervision, and only
//! then returns the
//! [`Runtime`] through which commands and queries are admitted. See the crate documentation ("Boot
//! sequence") for the order and why each step sits where it does.
use crate::dispatch::{self, Consumer, Dispatcher, DispatcherConfig, RecoveryReport};
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
    /// Set when shutdown begins: ordinary admission is fenced in memory at once.
    closing: AtomicBool,
    keep_awake: Arc<KeepAwake>,
    shutdown_grace: Duration,
    shutdown_bound: Duration,
    report: BootReport,
    tasks: Mutex<Vec<JoinHandle<()>>>,
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
/// 7. starts delivery ([`Dispatcher::run`]);
/// 8. starts artifact collection (a pass now, then periodically and after every applied
///    `artifacts.delete`), resource housekeeping ticks and run supervision ticks;
/// 9. returns the handle through which commands are admitted.
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
        use_cases,
        mut consumers,
        config,
    } = composition;
    let use_cases = use_cases
        .with_artifacts(artifacts.clone())
        .with_resources(resources.clone())
        .with_runs(runs.service.clone());
    // No entry path bypasses root controls: every consumer's root-scoped work passes the
    // run-control gate (and its own port, if it has one).
    for c in &mut consumers {
        c.eligibility = Some(Arc::new(RunGate::new(
            runs.service.clone(),
            c.eligibility.take(),
        )));
    }
    let mut replies = reply_consumer(
        Arc::new(ReceiptReplies::new(artifacts.clone())),
        store.clone(),
    );
    replies.max_in_flight = config.reply_max_in_flight;
    consumers.insert(0, replies);
    let run_owner = config.dispatcher.owner.clone();
    let dispatcher = Dispatcher::new(store.clone(), clock.clone(), consumers, config.dispatcher)
        .map_err(BootError::Composition)?;

    let queue = dispatcher.recover().await.map_err(BootError::Queue)?;
    let (svc, st) = (artifacts.clone(), store.clone());
    let recovered = tokio::task::spawn_blocking(move || svc.recover(&*st))
        .await
        .unwrap_or_else(|_| Err(panicked("artifact recovery")))
        .map_err(BootError::Artifacts)?;
    let supervisor = Arc::new(ProcessSupervisor::new(
        store.clone(),
        process_host,
        resources.clone(),
    ));
    let housekeeper = Arc::new(Housekeeper::new(
        store.clone(),
        clock.clone(),
        resources.clone(),
        supervisor.clone(),
    ));
    let recovered_resources = housekeeper
        .boot(config.machine_resources.clone())
        .await
        .map_err(BootError::Resources)?;
    let run_supervisor = RunSupervisor::new(
        store.clone(),
        clock,
        runs.service.clone(),
        supervisor,
        runs.launcher.clone(),
        runs.hooks.clone(),
        run_owner,
    );
    let recovered_runs = run_supervisor.boot().await.map_err(BootError::Runs)?;
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
    .map_err(BootError::Settings)?;

    let run = dispatcher.clone();
    let delivery = tokio::spawn(async move { run.run().await });
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
    let supervision = tokio::spawn(run_supervisor.clone().run(every));
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
        closing: AtomicBool::new(false),
        keep_awake,
        shutdown_grace: config.shutdown_grace,
        shutdown_bound: config.shutdown_bound,
        report: BootReport {
            queue,
            artifacts: recovered,
            resources: recovered_resources,
            runs: recovered_runs,
            keep_awake: applied,
        },
        tasks: Mutex::new(vec![delivery, collection, housekeeping, supervision]),
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
    /// The process supervisor every owned tree is launched through (process tools, package and
    /// helper hosts).
    pub fn processes(&self) -> &Arc<ProcessSupervisor> {
        self.housekeeper.supervisor()
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
        if self.closing.load(Ordering::SeqCst) {
            return closing_reply(
                raw,
                self.ids.next_id(devworks_core::ports::IdKind::Correlation),
            );
        }
        let outcome = self
            .use_cases
            .command_with(&self.gateway(), ctx, operation, raw, options);
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
        if released && operation == SAVES_SETTINGS {
            // A read failure leaves the previous request in place; the next save or explicit
            // refresh applies the setting again.
            let _ = self.refresh_keep_awake();
        }
        outcome
    }

    /// Stops admission of new deliveries, collection and housekeeping, waits for the delivery
    /// loop, the collector, the housekeeper and every in-flight delivery to finish (an
    /// asynchronous handler gets its consumer's stop grace), then stops every owned process tree
    /// within [`RuntimeConfig::shutdown_bound`] (recording the `Shutdown` reconciliation of
    /// resources) and releases the keep-awake request. Every tree is stopped natively even when
    /// the store refuses writes; the report carries each tree's outcome and any write failure.
    pub async fn shutdown(&self) -> ShutdownReport {
        // AD20 step 1: fence admission in memory, then commit the run-control transition: live
        // feature/issue work is held for manual Continue and asked to stop; launches get the
        // cooperative grace. A storage failure here is not fatal: the next boot holds the same
        // work from its boot epoch.
        self.closing.store(true, Ordering::SeqCst);
        let _ = self.runs.quiesce(self.shutdown_grace).await;
        self.stop();
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap_or_else(|p| p.into_inner()));
        for task in tasks {
            let _ = task.await;
        }
        self.dispatcher.idle().await;
        let stopped = self
            .housekeeper
            .shutdown(self.shutdown_grace, self.shutdown_bound)
            .await;
        // AD20 step 4: final known activation state.
        let _ = self.runs.finish().await;
        self.keep_awake.apply(false);
        stopped
    }

    fn stop(&self) {
        self.dispatcher.stop();
        self.collector.stop();
        self.housekeeper.stop();
        self.runs.stop();
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
        // Launched coordinators are signalled and aborted: nothing renews or writes after drop.
        self.runs.abort_launches();
        self.keep_awake.apply(false);
    }
}
