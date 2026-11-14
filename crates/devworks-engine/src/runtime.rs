//! The engine's one boot entry point: [`boot`] recovers durable state in a fixed order (queue,
//! artifacts, owned processes and resource leases, run roots), applies keep-awake, records what
//! recovery established as a diagnostic, starts every required consumer, artifact collection,
//! resource housekeeping, run supervision and the diagnostics monitor, and only then returns the
//! [`Runtime`] through which commands and queries are admitted. See the crate documentation ("Boot
//! sequence") for the order and why each step sits where it does.
use crate::approvals::{ResumeAdmission, ResumeDispatcher};
use crate::browser::{BrowserComposition, Supervision as BrowserSupervision};
use crate::diagnostics::{self as diag, Monitor};
use crate::dispatch::{self, Consumer, Dispatcher, DispatcherConfig, RecoveryReport};
use crate::evals::EvalSupervisor;
use crate::indexing::{IndexSupervisor, IndexingConfig};
use crate::jobs::Scheduler;
use crate::notifications::OsNotificationSupervisor;
use crate::package_host::{HostConfig, LaunchPorts, PackageHost};
use crate::replies::reply_consumer;
use crate::resources::{BootReport as ResourceBootReport, Housekeeper};
use crate::runs::{ActivationLauncher, RunGate, RunSupervisor};
use crate::workers::{DrainAt, Loop, StopAt, Workers};
use devworks_core::context::InvocationContext;
use devworks_core::gateway::{CommandOutcome, Gateway, QueryOutcome};
use devworks_core::ports::files::FileFault;
use devworks_core::ports::os_notifications::{OsNotificationPermission, OsNotificationPort};
use devworks_core::ports::process::{PowerPort, ProcessHost};
use devworks_core::ports::storage::{
    ApplicationStore, Cancellation, StorageError, Unavailable, WriteOptions,
};
use devworks_core::ports::{AuthorityPort, Clock, IdSource};
use devworks_core::protocol::records::EvalSuiteSnapshot;
use devworks_core::protocol::results::DomainDecision;
use devworks_core::protocol::{CommandReply, Id};
use devworks_core::services::UseCases;
use devworks_core::services::approvals::{ApprovalCatalog, ApprovalService};
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::artifacts::lifecycle::{
    CollectReport, RecoveryReport as ArtifactRecoveryReport,
};
use devworks_core::services::artifacts::replies::ReceiptReplies;
use devworks_core::services::browser::{BrowserRecovery, BrowserService};
use devworks_core::services::diagnostics::{DiagnosticService, ExportFailure, LogQuery};
use devworks_core::services::evals::EvalService;
use devworks_core::services::jobs::runner::{
    InferenceJobs, JobLifecycle, NativeJobDispatch, NoInferencePipeline,
};
use devworks_core::services::jobs::{JobConfig, JobService, NativeCatalog};
use devworks_core::services::notifications::NotificationService;
use devworks_core::services::packages::PackageService;
use devworks_core::services::power::{KeepAwake, KeepAwakeStatus};
use devworks_core::services::process::{ProcessSupervisor, ShutdownReport};
use devworks_core::services::projects::{ProjectService, RepositoryService};
use devworks_core::services::resources::{MachineResource, ResourceService, desktop};
use devworks_core::services::runs::ControlService;
use devworks_core::services::runs::recovery::{BootRecovery, LifecycleHooks, NoLifecycleHooks};
use devworks_core::services::work::WorkService;
use devworks_core::settings::view::Health;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;
use tokio::sync::{Notify, watch};

/// The operation whose applied decision may leave blobs pending physical removal.
const RELEASES_BLOBS: &str = "artifacts.delete";
/// The operation through which the `power.keepAwake` setting changes.
const SAVES_SETTINGS: &str = "settings.save_revision";
/// The operation that records a reported diagnostic (its commit raises the live-log hint).
const RECORDS_DIAGNOSTIC: &str = "workspace.record_diagnostic";
/// The operation whose commit fences a project and journals its root stops; the engine then
/// kills the project's owned trees at once and executes those stops.
const ARCHIVES_PROJECT: &str = "projects.archive";
/// The operation whose commit cancels an index run (the run it names is signalled at once).
const CANCELS_INDEX_RUN: &str = "indexing.cancel";
/// The query answered after a discovery scan.
const DISCOVERS_REPOSITORIES: &str = "indexing.discover";

/// Repository indexing as composed (DW-023): the parser, the source tree access, the path
/// resolver of helper launches, the indexing bounds and any installed semantic packs. [`boot`]
/// builds the indexing environment over the runtime's store, process supervisor, artifact and
/// diagnostics services, registers the `indexing.*` use cases and starts index supervision.
#[derive(Clone)]
pub struct IndexComposition {
    pub parser: Arc<dyn devworks_core::ports::indexing::StructuralParser>,
    pub source: Arc<dyn devworks_core::ports::indexing::SourceTree>,
    pub fs: Arc<dyn devworks_core::authority::destructive::FilesystemResolver>,
    pub settings: devworks_core::services::indexing::IndexSettings,
    pub config: IndexingConfig,
    pub enrichers: Vec<Arc<dyn devworks_core::ports::indexing::SemanticEnricher>>,
}

/// Run root control as composed: the one control service (its plan/readiness port inside), the
/// coordinator launcher (none until the agent supervisor exists: accepted activations then stay
/// pending and are held at the next boot), further lifecycle hooks (keep-working, a later item;
/// [`boot`] always composes the schedules' own hooks first) and the scheduled-inference dispatch
/// port (none until the coordinator turn pipeline exists: a scheduled inference run then fails
/// before dispatch with a visible reason).
#[derive(Clone)]
pub struct RunComposition {
    pub service: Arc<ControlService>,
    pub launcher: Option<Arc<dyn ActivationLauncher>>,
    pub hooks: Arc<dyn LifecycleHooks>,
    pub inference: Arc<dyn InferenceJobs>,
}
impl RunComposition {
    pub fn new(service: Arc<ControlService>) -> Self {
        Self {
            service,
            launcher: None,
            hooks: Arc::new(NoLifecycleHooks),
            inference: Arc::new(NoInferencePipeline),
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

/// Lifecycle hooks run in order (schedules, then the caller's).
struct ChainedHooks(Vec<Arc<dyn LifecycleHooks>>);
impl LifecycleHooks for ChainedHooks {
    fn quiesce(
        &self,
        tx: &mut dyn devworks_core::ports::storage::WriteTransaction,
    ) -> Result<(), StorageError> {
        self.0.iter().try_for_each(|h| h.quiesce(tx))
    }
    fn recover(
        &self,
        tx: &mut dyn devworks_core::ports::storage::WriteTransaction,
    ) -> Result<(), StorageError> {
        self.0.iter().try_for_each(|h| h.recover(tx))
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
    /// any the caller registered) and gates every consumer's root-scoped deliveries with it. It
    /// also registers the `projects.*` and repository registration use cases, whose archival
    /// stops roots through this same service.
    pub runs: RunComposition,
    /// The OS power facility the keep-awake setting is applied with.
    pub power: Arc<dyn PowerPort>,
    /// The one diagnostics service: [`boot`] registers the `workspace.diagnostics`, `.logs` and
    /// `.record_diagnostic` use cases with it (replacing any the caller registered), records
    /// boot recovery through it, feeds it the boot report, collection passes and consumers, and
    /// runs its retention and monitor passes.
    pub diagnostics: Arc<DiagnosticService>,
    /// Owned browser sessions (`browser.*`): when present, [`boot`] builds the one
    /// [`BrowserService`] over the composition's store, artifact service and process owner,
    /// registers its use cases and effect consumer, recovers earlier boots' sessions after the
    /// owned processes, supervises it and closes its sessions at shutdown. When absent, no
    /// browser operation is registered (nothing unqualified is advertised).
    pub browser: Option<BrowserComposition>,
    /// Native Git owner; missing executable/filesystem is not advertised.
    pub git: Option<crate::git::GitComposition>,
    /// The package lifecycle: [`boot`] registers the `addons.*` use cases with its service
    /// (replacing any the caller registered), starts the package host after process recovery and
    /// stops its helpers cooperatively at shutdown. `None`: no package host (packages are not
    /// offered).
    pub packages: Option<PackageComposition>,
    /// The other use cases (queue, sessions, history, settings, ...).
    pub use_cases: UseCases,
    /// Consumers besides the required ones [`boot`] registers itself (package handlers, ...).
    pub consumers: Vec<Consumer>,
    /// Repository indexing (none: the `indexing.*` operations have no use case).
    pub indexing: Option<IndexComposition>,
    pub config: RuntimeConfig,
}

#[derive(Clone)]
pub struct PurgeComposition {
    pub files: Arc<dyn devworks_core::ports::project_purge::ProjectPurgeFiles>,
    pub client: Arc<dyn devworks_core::ports::project_purge::ProjectPurgeClient>,
}
#[derive(Clone)]
pub struct ProcessToolComposition {
    pub bindings: devworks_core::services::process::tools::ToolBindings,
    pub filesystem: Arc<dyn devworks_core::authority::destructive::FilesystemResolver>,
    pub protected_roots: devworks_core::authority::destructive::ProtectedRoots,
    pub secrets: Arc<dyn devworks_core::ports::secrets::SecretStore>,
}
#[derive(Clone)]
pub struct ContainerComposition {
    pub bindings: devworks_core::services::process::tools::ToolBindings,
    pub filesystem: Arc<dyn devworks_core::authority::destructive::FilesystemResolver>,
    pub protected_roots: devworks_core::authority::destructive::ProtectedRoots,
    pub secrets: Arc<dyn devworks_core::ports::secrets::SecretStore>,
    pub registry_auth: Arc<dyn devworks_core::services::containers::registry_auth::RegistryAuth>,
    pub workload_environment:
        Arc<dyn devworks_core::services::containers::registry_auth::WorkloadEnvironment>,
    pub host: devworks_core::services::containers::probe::HostPlatform<'static>,
}
impl std::fmt::Debug for ContainerComposition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ContainerComposition")
    }
}
impl std::fmt::Debug for ProcessToolComposition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProcessToolComposition")
    }
}
#[derive(Clone)]
pub struct DesktopComposition {
    pub platform: Arc<dyn devworks_core::ports::desktop::DesktopPlatform>,
}
impl std::fmt::Debug for DesktopComposition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DesktopComposition")
    }
}
impl std::fmt::Debug for PurgeComposition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PurgeComposition")
    }
}
#[derive(Clone)]
pub struct McpComposition {
    pub validator: Arc<dyn devworks_core::ports::packages::SchemaValidator>,
    pub factory: Arc<
        dyn Fn(
                Arc<ProcessSupervisor>,
                tokio::runtime::Handle,
            ) -> Arc<dyn devworks_core::ports::mcp::McpRemote>
            + Send
            + Sync,
    >,
}
impl std::fmt::Debug for McpComposition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("McpComposition(..)")
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    /// The native desktop adapter. It remains registered when an OS permission is missing so
    /// inspect can report the actual capability state.
    pub desktop: Option<DesktopComposition>,
    pub purge: Option<PurgeComposition>,
    /// Process tools are offered only when a host supplies owned executable/root and secret
    /// bindings. Missing qualification leaves the family unregistered.
    pub process_tools: Option<ProcessToolComposition>,
    /// Native outgoing MCP connection owner, installed after the process supervisor exists.
    pub mcp: Option<McpComposition>,
    /// Optional selected container backend. Without a qualified host binding the family is absent.
    pub containers: Option<ContainerComposition>,
    pub schemas: Option<crate::schemas::SchemaComposition>,
    /// Native bounded credential-free price source and cadence; absent sources refuse refresh.
    pub price_refresh: Option<crate::prices::PriceComposition>,
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
    /// Time between browser supervision passes (notifications, popups, crash detection).
    pub browser_interval: Duration,
    /// Scheduler policy (due grace, claim batch).
    pub jobs: JobConfig,
    /// Time between scheduler ticks (commands that change jobs request one at once).
    pub scheduler_interval: Duration,
    /// Already authorized immutable suites supplied by the host (DW-72 owns activation).
    /// Empty by default: no automatic grading or transcript scans. Agents cannot activate suites.
    pub automatic_eval_suites: Vec<EvalSuiteSnapshot>,
    /// Maximum delay before a lost terminal-state wakeup is repaired by a durable sweep.
    pub eval_interval: Duration,
    /// Periodic repair of missed approval/hold wakeups; saved operations retain their keys.
    pub approval_resume_interval: Duration,
    /// Native OS delivery adapter. Absent adapters keep the persistent inbox available.
    pub native_notifications: Option<Arc<dyn OsNotificationPort>>,
    /// Native provider HTTP and explicit host capability/embedding-space qualification.
    pub providers: Option<crate::providers::driver::ProviderComposition>,
    /// Durable notification catch-up interval; focus and committed commands also wake it.
    pub notification_interval: Duration,
    /// Time between watcher passes (settle observed outcomes, deliver pending events).
    pub watch_interval: Duration,
    /// File-backed skills and role instructions (`skills.*`, `roles.*`). Absent: neither family
    /// is registered.
    pub instructions: Option<InstructionComposition>,
}

/// The skill roots and their file access, supplied by the host.
#[derive(Clone)]
pub struct InstructionComposition {
    pub source: Arc<dyn devworks_core::ports::indexing::SourceTree>,
    pub files: Arc<dyn devworks_core::ports::skill_files::SkillFiles>,
    pub roots: devworks_core::services::instructions::SkillRoots,
}
impl std::fmt::Debug for InstructionComposition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstructionComposition")
            .field("roots", &self.roots)
            .finish_non_exhaustive()
    }
}
impl RuntimeConfig {
    pub fn new(owner: Id) -> Self {
        Self {
            desktop: None,
            dispatcher: DispatcherConfig::new(owner),
            purge: None,
            process_tools: None,
            mcp: None,
            containers: None,
            price_refresh: None,
            schemas: None,
            collect_interval: Duration::from_secs(15 * 60),
            reply_max_in_flight: None,
            machine_resources: vec![desktop()],
            housekeeping_interval: Duration::from_secs(30),
            shutdown_grace: Duration::from_secs(5),
            shutdown_bound: Duration::from_secs(30),
            diagnostics_interval: Duration::from_secs(60 * 60),
            browser_interval: Duration::from_secs(1),
            jobs: JobConfig::default(),
            scheduler_interval: Duration::from_secs(5),
            automatic_eval_suites: Vec::new(),
            eval_interval: Duration::from_secs(30),
            approval_resume_interval: Duration::from_secs(5),
            native_notifications: None,
            providers: None,
            notification_interval: Duration::from_secs(5),
            watch_interval: Duration::from_millis(250),
            instructions: None,
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
    /// Browser sessions of earlier boots settled from what process recovery verified, and the
    /// orphaned profile roots removed (`None` when no browser is composed).
    pub browser: Option<BrowserRecovery>,
    /// Index runs of earlier boots made claimable to resume from their checkpoints.
    pub index_runs_resumed: usize,
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
    /// Browser session recovery failed.
    Browser(StorageError),
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
    usage: Arc<crate::usage::UsageRuntime>,
    providers: Option<Arc<crate::providers::driver::NativeProviders>>,
    conventions: Arc<devworks_core::services::conventions::ConventionService>,
    instructions: Option<Arc<devworks_core::services::instructions::InstructionService>>,
    instruction_boot:
        Option<Result<devworks_core::services::instructions::SyncReport, StorageError>>,
    schemas: Option<Arc<devworks_core::services::schemas::SchemaService>>,
    /// The shutdown stage ([`ShutdownStage`] as a byte) for the progress surface; the fence
    /// itself is `admission`.
    stage: std::sync::atomic::AtomicU8,
    /// The admission fence: ordinary commands hold it through admission and commit. A retained
    /// receive waits outside it with a registered shutdown token, then holds it for its final
    /// decision. Shutdown closes it exclusively and cancels waiting receives; no command commits
    /// after the run-control quiescence snapshot.
    admission: Arc<std::sync::RwLock<bool>>,
    /// The command path (admission fence, gateway, post-commit wakeups) shared with the
    /// scheduler's native runs.
    admitter: Arc<Admitter>,
    jobs: Arc<JobService>,
    scheduler: Arc<Scheduler>,
    evals: Arc<EvalSupervisor>,
    approvals: Arc<ApprovalService>,
    resumes: Arc<ResumeDispatcher>,
    prices: Option<Arc<crate::prices::PriceSupervisor>>,
    price_service: Arc<devworks_core::services::prices::PriceService>,
    notifications: Option<Arc<OsNotificationSupervisor>>,
    keep_awake: Arc<KeepAwake>,
    diagnostics: Arc<DiagnosticService>,
    browser: Option<Arc<BrowserService>>,
    process_tools: Option<Arc<devworks_core::services::process::tools::ProcessToolService>>,
    watchers: Option<Arc<crate::watch::WatchSupervisor>>,
    containers: Option<Arc<devworks_core::services::containers::service::ContainerService>>,
    desktop: Option<Arc<devworks_core::services::desktop::DesktopService>>,
    git: Option<Arc<devworks_core::services::git::GitService>>,
    packages: Option<Arc<PackageHost>>,
    shutdown_grace: Duration,
    shutdown_bound: Duration,
    report: BootReport,
    /// Index supervision (none when indexing is not composed).
    indexing: Option<Arc<IndexSupervisor>>,
    /// Every background loop boot started, in registration order. Stop, shutdown and drop
    /// iterate it; each loop is awaited only within the shutdown bound (a root stop or a
    /// cancellation one is executing may be blocked on a tree).
    workers: Workers,
    /// What the usage worker's drain established, for the shutdown report.
    usage_drain: Arc<Mutex<Option<Result<crate::usage::UsageDrain, StorageError>>>>,
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
        browser,
        git,
        packages,
        use_cases,
        mut consumers,
        indexing,
        config,
    } = composition;
    // Every error that stops boot passes through this, so each leaves a failed boot diagnostic.
    // The diagnostics service is part of the composition, so no step runs without one; a store
    // that cannot take the record is ignored by `record_boot_failure`.
    let failed = |e: BootError| {
        diag::record_boot_failure(&diagnostics, &e);
        e
    };
    let usage = crate::usage::UsageRuntime::with_process_host(
        store.clone(),
        runs.service.clone(),
        use_cases.inference_router(),
        process_host.clone(),
    );
    let use_cases = use_cases.with_usage(Arc::new(
        devworks_core::services::usage::UsageService::new(ids.clone(), clock.clone()),
    ));
    for suite in &config.automatic_eval_suites {
        devworks_core::protocol::Check::check(suite)
            .map_err(|e| BootError::Composition(StorageError::InvalidRecord(e)))
            .map_err(failed)?;
    }
    // The one process owner: every owned tree (browsers included) is launched through it. It is
    // built first so the browser service can be composed with it; its recovery still runs at
    // step 4.
    let supervisor = Arc::new(ProcessSupervisor::new(
        store.clone(),
        process_host,
        resources.clone(),
    ));
    if let (Some(service), Some(composition)) = (use_cases.mcp_service(), config.mcp.as_ref()) {
        service
            .install_remote(
                store.clone(),
                (composition.factory)(supervisor.clone(), tokio::runtime::Handle::current()),
                composition.validator.clone(),
                artifacts.clone(),
            )
            .map_err(|e| BootError::Composition(StorageError::InvalidRecord(e.into())))
            .map_err(failed)?;
        consumers.push(crate::mcp::consumer(service));
    }
    // Agents' package tool calls run as durable operations of their own consumer; the host it
    // invokes is created after the dispatcher (set below, before deliveries start).
    let package_tools: Arc<OnceLock<std::sync::Weak<PackageHost>>> = Arc::default();
    if packages.is_some() {
        consumers.push(crate::package_tools::consumer(
            store.clone(),
            package_tools.clone(),
        ));
    }
    let process_tools = config.process_tools.as_ref().map(|composition| {
        Arc::new(
            devworks_core::services::process::tools::ProcessToolService::new(
                store.clone(),
                supervisor.clone(),
                authority.clone(),
                composition.secrets.clone(),
                composition.bindings.clone(),
                composition.filesystem.clone(),
                composition.protected_roots.clone(),
                ids.clone(),
            ),
        )
    });
    let containers = config.containers.as_ref().map(|composition| {
        let runner = Arc::new(
            devworks_core::services::containers::runner::ContainerRunner::new(
                supervisor.clone(),
                composition.filesystem.clone(),
                composition.protected_roots.clone(),
            ),
        );
        Arc::new(
            devworks_core::services::containers::service::ContainerService::new(
                store.clone(),
                runner,
                composition.bindings.clone(),
                composition.host,
                ids.clone(),
            )
            .with_artifacts(artifacts.clone())
            .with_resources(resources.clone())
            .with_registry_auth(composition.registry_auth.clone())
            .with_workload_environment(composition.workload_environment.clone())
            .with_secrets(composition.secrets.clone()),
        )
    });
    let git = git.and_then(|composition| {
        let fs = composition.config.filesystem.clone()?;
        let helpers = Arc::new(
            devworks_core::services::indexing::helper::SupervisedHelpers {
                supervisor: supervisor.clone(),
                store: store.clone(),
                fs,
            },
        );
        Some(Arc::new(devworks_core::services::git::GitService::new(
            store.clone(),
            helpers,
            artifacts.clone(),
            ids.clone(),
            clock.clone(),
            composition.config,
        )))
    });
    let git = match git {
        Some(service) => {
            let probe = service.clone();
            if tokio::task::spawn_blocking(move || probe.qualify())
                .await
                .unwrap_or(false)
            {
                Some(service)
            } else {
                None
            }
        }
        None => None,
    };
    let browser = browser.map(|b| {
        Arc::new(
            BrowserService::new(
                store.clone(),
                b.platform,
                artifacts.clone(),
                supervisor.clone(),
                b.fs,
                b.app_data_root,
                ids.clone(),
                clock.clone(),
                b.config,
            )
            .with_resolver(b.resolver)
            .with_git_source(git.clone()),
        )
    });
    let desktop = config.desktop.as_ref().map(|composition| {
        Arc::new(
            devworks_core::services::desktop::DesktopService::new(
                composition.platform.clone(),
                resources.clone(),
                artifacts.clone(),
                store.clone(),
                clock.clone(),
                ids.clone(),
                authority.clone(),
            )
            .with_git_source(git.clone()),
        )
    });
    let mut use_cases = use_cases;
    if let Some(service) = &process_tools {
        use_cases = use_cases.with_process_tools(service.clone());
        consumers.push(crate::process_tools::consumer(service.clone()));
        consumers.push(crate::process_tools::cancel_consumer(service.clone()));
    }
    // Watchers launch through the process tools, so they are composed with them.
    let watchers = process_tools.as_ref().map(|service| {
        Arc::new(crate::watch::WatchSupervisor::new(
            store.clone(),
            ids.clone(),
            Arc::new(
                devworks_core::services::watch::WatchService::new(
                    store.clone(),
                    service.clone(),
                    artifacts.clone(),
                    ids.clone(),
                    clock.clone(),
                )
                .with_root_control(runs.service.clone())
                .with_diagnostics(diagnostics.clone()),
            ),
        ))
    });
    if let (Some(supervisor), Some(service)) = (&watchers, &process_tools) {
        use_cases = use_cases.with_watchers(supervisor.service().clone());
        consumers.push(crate::process_tools::watch_consumer(service.clone()));
    }
    if let Some(service) = &containers {
        use_cases = use_cases.with_containers(service.clone());
        consumers.push(crate::containers::consumer(
            service.clone(),
            authority.clone(),
        ));
        consumers.push(crate::containers::stop_consumer(
            service.clone(),
            authority.clone(),
        ));
    }
    if let Some(service) = &desktop {
        use_cases = use_cases.with_desktop(service.clone());
        consumers.push(crate::desktop::consumer(service.clone()));
    }
    if let Some(service) = &git {
        use_cases = use_cases.with_git(service.clone());
        consumers.push(crate::git::consumer(service.clone()));
    }
    if let Some(b) = &browser {
        use_cases = use_cases.with_browser(b.clone());
        consumers.push(crate::browser::consumer(b.clone()));
    }
    let use_cases = match &packages {
        Some(p) => use_cases.with_packages(p.service.clone()),
        None => use_cases,
    };
    let price_service = Arc::new(devworks_core::services::prices::PriceService::new(
        ids.clone(),
        clock.clone(),
        config.price_refresh.is_some(),
    ));
    let prices = config.price_refresh.as_ref().map(|p| {
        crate::prices::PriceSupervisor::new(store.clone(), price_service.clone(), p.source.clone())
    });
    if let Some(worker) = &prices {
        consumers.push(worker.consumer());
    }
    let use_cases = use_cases.with_prices(price_service.clone());
    let work = Arc::new(
        WorkService::new(ids.clone(), runs.service.clone())
            .with_staged_plans(artifacts.clone())
            .with_clock(clock.clone()),
    );
    let mut projects =
        ProjectService::new(ids.clone(), runs.service.clone()).with_work(work.clone());
    if let Some(p) = &config.purge {
        projects = projects.with_purge(Arc::new(
            devworks_core::services::projects::purge::PurgeCoordinator::new(
                ids.clone(),
                p.files.clone(),
                p.client.clone(),
            ),
        ));
    }
    let purge_worker = config.purge.as_ref().map(|p| {
        Arc::new(devworks_core::services::projects::purge::PurgeWorker::new(
            store.clone(),
            ids.clone(),
            p.files.clone(),
            p.client.clone(),
        ))
    });
    let eval_service = Arc::new(EvalService::new(ids.clone(), clock.clone()));
    let use_cases = use_cases
        .with_evals(eval_service.clone())
        .with_artifacts(artifacts.clone())
        .with_resources(resources.clone())
        .with_runs(runs.service.clone())
        .with_projects(Arc::new(projects))
        .with_work(work)
        .with_repositories(Arc::new(RepositoryService::new(ids.clone())))
        .with_diagnostics(diagnostics.clone());
    let conventions = Arc::new(
        devworks_core::services::conventions::ConventionService::new(
            store.clone(),
            artifacts.clone(),
            ids.clone(),
            clock.clone(),
        ),
    );
    consumers.push(crate::conventions::indexing::consumer(
        store.clone(),
        conventions.indexing(),
    ));
    consumers.push(crate::conventions_import::consumer(conventions.imports()));
    let use_cases = use_cases.with_conventions(conventions.clone());
    let schemas = config.schemas.as_ref().map(|ports| {
        Arc::new(
            devworks_core::services::schemas::SchemaService::new(ids.clone(), clock.clone())
                .with_credentials(store.clone(), ports.secrets.clone())
                .with_runtime(store.clone(), ports.reader.clone(), artifacts.clone()),
        )
    });
    let use_cases = if let Some(service) = &schemas {
        // Namespace/reference-only cleanup precedes password intake and command exposure.
        match service.reconcile_credential_orphans() {
            Ok(_)
            | Err(
                devworks_core::ports::secrets::SecretError::Unavailable(_)
                | devworks_core::ports::secrets::SecretError::Denied,
            ) => {}
            Err(_) => {
                return Err(failed(BootError::Composition(StorageError::unavailable(
                    Unavailable::Io,
                    "schema credential reference reconciliation failed",
                ))));
            }
        }
        consumers.push(crate::schemas::consumer(store.clone(), service.clone()));
        use_cases.with_schemas(service.clone())
    } else {
        use_cases
    };
    // A deliberately supplied native-job fixture may own DOCS. Production composes the
    // complete Help family and its consumer together; it never attaches to a foreign journal.
    let help = (!use_cases.has_command("help.generate_capability_docs")).then(|| {
        Arc::new(devworks_core::services::help::HelpService::new(
            store.clone(),
            artifacts.clone(),
            ids.clone(),
            clock.clone(),
            packages.as_ref().map(|package| package.service.clone()),
        ))
    });
    let use_cases = if let Some(service) = &help {
        consumers.push(crate::help::consumer(service.clone()));
        use_cases.with_help(service.clone())
    } else {
        use_cases
    };
    // Install metadata and the durable consumer together. Transport is installed later, after
    // the complete capability/schema registry exists, and before delivery can start.
    let embedding_spaces = if use_cases.connection_service().is_some() {
        let service = Arc::new(
            devworks_core::services::router::embedding_spaces::EmbeddingSpaceService::new(
                store.clone(),
            ),
        );
        if !service.install_selection_indexer(Arc::downgrade(&conventions.indexing())) {
            return Err(failed(BootError::Composition(StorageError::ScopeDenied)));
        }
        Some(service)
    } else {
        None
    };
    let embedding_probes = embedding_spaces.as_ref().map(|service| {
        let worker = crate::conventions::probes::ProbeRuntime::new(service.clone());
        consumers.push(worker.consumer());
        worker
    });
    let use_cases = if let Some(service) = &embedding_spaces {
        use_cases.with_embedding_spaces(service.clone())
    } else {
        use_cases
    };
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
    // Indexing runs its Git helpers through this same process owner.
    let index_supervisor = indexing.map(|ix| {
        let env = Arc::new(devworks_core::services::indexing::IndexEnv {
            store: store.clone(),
            parser: ix.parser,
            source: ix.source,
            helpers: Arc::new(
                devworks_core::services::indexing::helper::SupervisedHelpers {
                    supervisor: supervisor.clone(),
                    store: store.clone(),
                    fs: ix.fs,
                },
            ),
            clock: clock.clone(),
            ids: ids.clone(),
            artifacts: Some(artifacts.clone()),
            diagnostics: Some(diagnostics.clone()),
            enrichers: std::sync::RwLock::new(ix.enrichers),
            settings: ix.settings,
        });
        let worker = Arc::new(devworks_core::services::indexing::worker::IndexWorker::new(
            env,
            // Unique per boot: a claim of an earlier boot is always another owner.
            format!(
                "index-{}-{}",
                run_owner.as_str(),
                store
                    .info()
                    .boot_id
                    .map_or_else(String::new, |b| b.as_str().to_owned())
            ),
        ));
        IndexSupervisor::new(worker, ix.config)
    });
    let use_cases = match &index_supervisor {
        Some(ix) => use_cases.with_indexing(Arc::new(
            devworks_core::services::indexing::IndexCoordinator::new(ix.env().clone()),
        )),
        None => use_cases,
    };
    let use_cases = use_cases.with_graph(Arc::new(
        devworks_core::services::graph::GraphService::new(
            store.clone(),
            clock.clone(),
            ids.clone(),
            artifacts.clone(),
        )
        .with_indexing(index_supervisor.as_ref().map(|ix| ix.env().clone())),
    ));
    // Native jobs may run only the maintenance commands this composition can execute.
    let jobs = Arc::new(
        JobService::new(NativeCatalog::qualified(&use_cases), config.jobs)
            .with_approval_control(runs.service.clone()),
    );
    let use_cases = use_cases.with_jobs(jobs.clone());
    let instructions = config.instructions.clone().map(|c| {
        Arc::new(
            devworks_core::services::instructions::InstructionService::new(
                store.clone(),
                artifacts.clone(),
                devworks_core::services::instructions::SkillAccess {
                    source: c.source,
                    files: c.files,
                    roots: c.roots,
                },
                ids.clone(),
                Arc::new(devworks_core::authority::HostRoles {
                    store: store.clone(),
                }),
                clock.clone(),
            ),
        )
    });
    let use_cases = match &instructions {
        Some(service) => use_cases.with_instructions(service.clone()),
        None => use_cases,
    };
    let approvals = Arc::new(
        ApprovalService::new(
            ids.clone(),
            clock.clone(),
            Arc::new(devworks_core::authority::HostRoles {
                store: store.clone(),
            }),
            runs.service.clone(),
        )
        .with_catalog(ApprovalCatalog::qualified(&use_cases))
        .with_owners(Arc::new(
            devworks_core::services::approvals::owner_preview::NativeOwnerPreviews::new(
                git.clone(),
                packages.as_ref().map(|p| p.service.clone()),
                Some(jobs.clone()),
            )
            .with_desktop(desktop.clone())
            .with_instructions(instructions.clone())
            .with_containers(containers.clone())
            .with_control(runs.service.clone())
            .with_evidence(store.clone(), artifacts.clone()),
        )),
    );
    let use_cases = use_cases
        .with_approvals(approvals.clone())
        .with_notifications(Arc::new(NotificationService::new(ids.clone())));
    if let Some(service) = &help {
        service.set_composition(&use_cases);
    }
    let providers = match config.providers.clone() {
        Some(provider) => {
            let provider = provider.with_instruction_guard(Arc::new(
                crate::instruction_guard::InstructionGuard::new(
                    store.clone(),
                    index_supervisor.as_ref().map(|ix| ix.env().source.clone()),
                )
                .with_skills(instructions.clone()),
            ));
            let connections = use_cases.connection_service().ok_or_else(|| {
                failed(BootError::Composition(StorageError::unavailable(
                    Unavailable::UnsupportedRuntime,
                    "native provider composition requires current ConnectionService",
                )))
            })?;
            let schemas = Arc::new(crate::providers::schemas::RegisteredSchemas::new(
                store.clone(),
                use_cases.clone(),
                packages.as_ref().map(|p| p.service.clone()),
                artifacts.clone(),
            ));
            Some(crate::providers::driver::NativeProviders::new(
                store.clone(),
                artifacts.clone(),
                connections,
                usage.clone(),
                schemas,
                provider,
            ))
        }
        None => None,
    };
    if let (Some(provider), Some(service), Some(router), Some(_)) = (
        &providers,
        use_cases.mcp_service(),
        use_cases.inference_router(),
        config.mcp.as_ref(),
    ) {
        service
            .install_sampler(Arc::new(crate::mcp_sampling::EngineSampler::new(
                &service,
                provider,
                store.clone(),
                artifacts.clone(),
                router,
                tokio::runtime::Handle::current(),
            )))
            .map_err(|e| BootError::Composition(StorageError::InvalidRecord(e.into())))
            .map_err(failed)?;
    }
    if let (Some(provider), Some(service), Some(worker)) =
        (&providers, &embedding_spaces, &embedding_probes)
    {
        let executor = crate::conventions::embedding::NativeEmbeddings::new(
            store.clone(),
            artifacts.clone(),
            provider.clone(),
            service.clone(),
            clock.clone(),
        );
        conventions
            .indexing()
            .set_embeddings(Some(Arc::new(executor.clone())));
        if !worker.install(executor) {
            return Err(failed(BootError::Composition(StorageError::ScopeDenied)));
        }
        service.enable_native_qualification();
    }
    let hooks: Arc<dyn LifecycleHooks> = Arc::new(ChainedHooks(vec![
        Arc::new(JobLifecycle(jobs.clone())),
        runs.hooks.clone(),
    ]));
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
    // After process recovery has stopped any interrupted CLI tree, reconcile external
    // agent-work containers from the ownership ledger before new effect deliveries begin.
    if let Some(service) = &containers {
        let service = service.clone();
        let bound = config.shutdown_bound;
        let recovered = tokio::task::spawn_blocking(move || service.shutdown_agent_work(bound))
            .await
            .unwrap_or_else(|_| devworks_core::services::containers::ShutdownReport {
                read_error: Some("container boot recovery worker panicked".into()),
                ..Default::default()
            });
        if !recovered.uncertain.is_empty() || recovered.read_error.is_some() {
            let _ = diagnostics.log(devworks_core::services::diagnostics::LogEntry::new(
                "containers",
                "boot-recovery",
                devworks_core::protocol::records::Severity::Warning,
                format!(
                    "{} agent-work containers need shutdown reconciliation; {}",
                    recovered.uncertain.len(),
                    recovered.read_error.unwrap_or_default()
                ),
            ));
        }
    }
    // Browser sessions of earlier boots settle from what process recovery just verified.
    let recovered_browser = match &browser {
        Some(b) => {
            let svc = b.clone();
            Some(
                tokio::task::spawn_blocking(move || svc.recover())
                    .await
                    .unwrap_or_else(|_| Err(panicked("browser recovery")))
                    .map_err(BootError::Browser)
                    .map_err(failed)?,
            )
        }
        None => None,
    };
    let mut run_supervisor = RunSupervisor::new(
        store.clone(),
        clock.clone(),
        runs.service.clone(),
        supervisor.clone(),
        runs.launcher.clone(),
        hooks,
        run_owner,
    );
    // A root stop never outlasts the application's one shutdown bound.
    if let Some(s) = Arc::get_mut(&mut run_supervisor) {
        s.stop_grace = config.shutdown_grace.min(config.shutdown_bound);
        s.stop_bound = config.shutdown_bound;
        s.with_containers(containers.clone());
    }
    let recovered_runs = run_supervisor
        .boot()
        .await
        .map_err(BootError::Runs)
        .map_err(failed)?;
    // Packages (after owned processes are recovered, before any command is admitted): remove
    // interrupted publications and unreferenced install directories, then start every enabled,
    // available package's helper against the committed registry.
    let mut package_guard = PackageBootGuard(None);
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
            let _ = package_tools.set(Arc::downgrade(&host));
            // From here every early return abandons the host (breaks the service<->host cycle
            // that would keep the store and its workspace lock alive, and kills any started
            // helper) until the completed Runtime owns it.
            package_guard.0 = Some(host.clone());
            let booting = host.clone();
            tokio::task::spawn_blocking(move || booting.boot())
                .await
                .unwrap_or_else(|_| Err(panicked("package boot")))
                .map_err(BootError::Packages)
                .map_err(failed)?;
            let reviews = p.service.clone();
            tokio::task::spawn_blocking(move || reviews.sync_pending_reviews())
                .await
                .unwrap_or_else(|_| Err(panicked("package review recovery")))
                .map_err(BootError::Packages)
                .map_err(failed)?;
            Some(host)
        }
        None => None,
    };
    // The last recovery steps that can stop boot run before any task starts, so a failure here
    // leaves nothing delivering or holding the store.
    if let Some(worker) = &purge_worker {
        // Client effects run before UI restoration; held effects retain their original authority.
        let recovered = worker.clone();
        tokio::task::spawn_blocking(move || recovered.step(&mut || false))
            .await
            .unwrap_or_else(|_| Err(panicked("purge recovery")))
            .map_err(BootError::Composition)
            .map_err(failed)?;
    }
    // Earlier submissions remain uncertain even if this boot has no native adapter.
    crate::store::write(&store, |tx| {
        tx.decisions().recover_os_notification_deliveries()
    })
    .await
    .map_err(BootError::Composition)
    .map_err(failed)?;
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
    let index_runs_resumed = match &index_supervisor {
        Some(ix) => ix.recover().await.unwrap_or(0),
        None => 0,
    };
    let report = BootReport {
        queue,
        artifacts: recovered,
        resources: recovered_resources,
        runs: recovered_runs,
        keep_awake: applied,
        browser: recovered_browser,
        index_runs_resumed,
    };
    let (svc, recovered) = (diagnostics.clone(), report.clone());
    let _ = tokio::task::spawn_blocking(move || diag::record_boot(&svc, &recovered)).await;

    // Every background loop is registered here, at the point boot starts it; see `workers` for
    // where shutdown stops and drains each one.
    let mut workers = Workers::default();
    let (run, stop, idle) = (dispatcher.clone(), dispatcher.clone(), dispatcher.clone());
    workers.start(
        StopAt::Work,
        Some(DrainAt::AfterJoin),
        Arc::new(
            Loop::new("delivery", |_| async move { run.run().await })
                .on_stop(move || stop.stop())
                .on_drain(move |deadline| {
                    let idle = idle.clone();
                    async move {
                        let _ = tokio::time::timeout_at(deadline, idle.idle()).await;
                    }
                }),
        ),
    );
    if let Some(h) = &package_host {
        h.start();
    }
    let (status, _) = watch::channel(None);
    let collector = Arc::new(Collector {
        trigger: Notify::new(),
        stopping: AtomicBool::new(false),
        status,
    });
    let stop = collector.clone();
    workers.start(
        StopAt::Work,
        None,
        Arc::new(
            Loop::new("artifact collection", |_| {
                collect(
                    collector.clone(),
                    artifacts.clone(),
                    store.clone(),
                    config.collect_interval,
                )
            })
            .on_stop(move || stop.stop()),
        ),
    );
    let (keeper, stop, every) = (
        housekeeper.clone(),
        housekeeper.clone(),
        config.housekeeping_interval,
    );
    workers.start(
        StopAt::Work,
        None,
        Arc::new(
            Loop::new("resource housekeeping", |_| async move {
                keeper.run(every).await
            })
            .on_stop(move || stop.stop()),
        ),
    );
    run_supervisor.set_dispatcher_wakeup(dispatcher.wakeup());
    let stop = run_supervisor.clone();
    workers.start(
        StopAt::Work,
        None,
        Arc::new(
            Loop::new("run supervision", |_| run_supervisor.clone().run(every))
                .on_stop(move || stop.stop()),
        ),
    );
    let (stop, drain) = (usage.clone(), usage.clone());
    let usage_drain = Arc::new(Mutex::new(None));
    let drained = usage_drain.clone();
    workers.start(
        StopAt::Admission,
        Some(DrainAt::BeforeFence),
        Arc::new(
            Loop::new("usage accounting", |_| {
                usage.clone().run(Duration::from_secs(1))
            })
            .on_stop(move || stop.stop())
            .on_drain(move |deadline| {
                let (usage, drained) = (drain.clone(), drained.clone());
                async move {
                    let report = usage.drain_until(deadline).await;
                    *drained.lock().unwrap_or_else(|p| p.into_inner()) = Some(report);
                }
            }),
        ),
    );
    diag::sources(
        &diagnostics,
        report.clone(),
        booted_at_ms,
        collector.status.subscribe(),
        dispatcher.clone(),
    );
    let browser_supervision = Arc::new(BrowserSupervision::default());
    // Unknown external effects remain held, never retried. Re-observe them while the app is
    // running as well as at boot; an owner can establish a terminal fact without a restart.
    if process_tools.is_some() || containers.is_some() {
        let observing = dispatcher.clone();
        workers.start(
            StopAt::Work,
            None,
            Arc::new(Loop::new(
                "unknown outcome observation",
                |mut stopped| async move {
                    while !stopped.is_set() {
                        let _ = observing.observe_unknown().await;
                        stopped.wait(Duration::from_secs(10)).await;
                    }
                },
            )),
        );
    }
    if let Some(worker) = purge_worker {
        workers.start(
            StopAt::Work,
            None,
            Arc::new(Loop::new("project purge", |mut stopped| async move {
                while !stopped.is_set() {
                    let (worker, flag) = (worker.clone(), stopped.clone());
                    let _ = tokio::task::spawn_blocking(move || worker.step(&mut || flag.is_set()))
                        .await;
                    stopped.wait(Duration::from_millis(250)).await;
                }
            })),
        );
    }

    if let Some(supervisor) = &watchers {
        let supervisor = supervisor.clone();
        let every = config.watch_interval;
        workers.start(
            StopAt::Work,
            None,
            Arc::new(Loop::new("watchers", |mut stopped| async move {
                while !stopped.is_set() {
                    stopped.wait(every).await;
                    if stopped.is_set() {
                        break;
                    }
                    let pass = supervisor.clone();
                    let _ = tokio::task::spawn_blocking(move || pass.tick()).await;
                }
            })),
        );
    }

    let (imports, collection_wakeup, stop) = (
        conventions.imports(),
        collector.clone(),
        conventions.clone(),
    );
    workers.start(
        StopAt::Work,
        None,
        Arc::new(
            Loop::new("convention imports", |mut stopped| async move {
                let mut after = None;
                while !imports.is_stopped() && !stopped.is_set() {
                    let (service, position) = (imports.clone(), after.clone());
                    if let Ok(Ok(pass)) =
                        tokio::task::spawn_blocking(move || service.cleanup_terminal_page(position))
                            .await
                    {
                        after = pass.next;
                        if pass.released {
                            collection_wakeup.trigger.notify_one();
                        }
                    }
                    stopped.wait(Duration::from_secs(1)).await;
                }
            })
            .on_stop(move || stop.stop()),
        ),
    );
    if let Some(service) = &git {
        let stop = service.clone();
        workers.start(
            StopAt::Work,
            None,
            Arc::new(
                Loop::new("git", |stopped| crate::git::run(service.clone(), stopped))
                    .on_stop(move || stop.stop()),
            ),
        );
    }
    if let Some(b) = &browser {
        let svc = b.clone();
        diagnostics.register_source(Arc::new(move |now| svc.health(now)));
        let stop = browser_supervision.clone();
        workers.start(
            StopAt::Work,
            None,
            Arc::new(
                Loop::new("browser supervision", |_| {
                    crate::browser::run(
                        b.clone(),
                        browser_supervision.clone(),
                        config.browser_interval,
                    )
                })
                .on_stop(move || stop.stop()),
            ),
        );
    }
    let monitor = Arc::new(Monitor::default());
    let (watcher, svc, stop, every) = (
        monitor.clone(),
        diagnostics.clone(),
        monitor.clone(),
        config.diagnostics_interval,
    );
    workers.start(
        StopAt::Fence,
        None,
        Arc::new(
            Loop::new("diagnostics monitor", |_| async move {
                watcher.run(svc, every).await
            })
            .on_stop(move || stop.stop()),
        ),
    );
    // Ownership of the package host moves to the Runtime.
    package_guard.0 = None;
    if let Some(ix) = &index_supervisor {
        let (stop, drain) = (ix.clone(), ix.clone());
        workers.start(
            StopAt::Work,
            Some(DrainAt::AfterPackages),
            Arc::new(
                Loop::new("index supervision", |_| ix.clone().run())
                    .on_stop(move || stop.signal_stop())
                    .on_drain(move |deadline| {
                        let ix = drain.clone();
                        async move {
                            ix.stop(deadline).await;
                        }
                    }),
            ),
        );
    }
    let notifications = config
        .native_notifications
        .map(|port| OsNotificationSupervisor::new(store.clone(), ids.clone(), clock.clone(), port));
    if let Some(worker) = &notifications {
        let (stop, drain) = (worker.clone(), worker.clone());
        workers.start(
            StopAt::Admission,
            Some(DrainAt::BeforeFence),
            Arc::new(
                Loop::new("notifications", |_| {
                    worker.clone().run(config.notification_interval)
                })
                .on_stop(move || stop.stop())
                .on_drain(move |_| {
                    let worker = drain.clone();
                    async move { worker.drain().await }
                }),
            ),
        );
    }
    // Recovery has settled prior-boot activations before the first evaluation sweep.
    let evals = EvalSupervisor::new(store.clone(), eval_service, config.automatic_eval_suites);
    let (stop, drain) = (evals.clone(), evals.clone());
    workers.start(
        StopAt::Admission,
        // No blocking evaluator job may outlive the runtime: its local write fence is drained
        // once admission has closed.
        Some(DrainAt::AfterFence),
        Arc::new(
            Loop::new("evaluation", |_| evals.clone().run(config.eval_interval))
                .on_stop(move || stop.stop())
                .on_drain(move |_| {
                    let evals = drain.clone();
                    async move { evals.drain().await }
                }),
        ),
    );
    let admission = Arc::new(std::sync::RwLock::new(false));
    let admitter = Arc::new(Admitter {
        store: store.clone(),
        ids: ids.clone(),
        authority: authority.clone(),
        use_cases: use_cases.clone(),
        admission: admission.clone(),
        receive_waiters: Arc::new(Mutex::new(BTreeMap::new())),
        next_receive_waiter: AtomicU64::new(0),
        dispatcher: dispatcher.clone(),
        runs: run_supervisor.clone(),
        usage: usage.clone(),
        collector: collector.clone(),
        diagnostics: diagnostics.clone(),
        keep_awake: keep_awake.clone(),
        scheduler: OnceLock::new(),
        evals: evals.clone(),
        resumes: OnceLock::new(),
        prices: prices.clone(),
        notifications: notifications.clone(),
        indexing: index_supervisor.clone(),
        changes: watch::channel(0).0,
        handle: tokio::runtime::Handle::current(),
    });
    // Brokered application calls of packages run through this command path, as the principal
    // that invoked the package.
    if let Some(host) = &package_host {
        let gateway: Arc<dyn crate::package_host::broker::GatewayCalls> = admitter.clone();
        host.attach_broker(crate::package_host::broker::BrokerPorts {
            gateway: Arc::downgrade(&gateway),
            artifacts: artifacts.clone(),
        });
    }
    if let Some(service) = &help {
        let (service, stop) = (service.clone(), service.clone());
        let command = admitter.clone();
        workers.start(
            StopAt::Work,
            None,
            Arc::new(
                Loop::new("help maintenance", |mut stopped| async move {
                    while !service.is_stopped() && !stopped.is_set() {
                        let current = service.clone();
                        let target = command.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            let _ = current.cleanup();
                            if let Ok(Some((context, frame))) = current.maintenance_request() {
                                let _ = target.command_with(
                                    &context,
                                    "help.generate_capability_docs",
                                    &frame,
                                    WriteOptions::default(),
                                );
                            }
                        })
                        .await;
                        stopped.wait(Duration::from_secs(1)).await;
                    }
                })
                .on_stop(move || stop.stop()),
            ),
        );
    }
    if let Some(service) = &schemas {
        let (service, stop) = (service.clone(), service.clone());
        workers.start(
            StopAt::Work,
            None,
            Arc::new(
                Loop::new("schema view cleanup", |mut stopped| async move {
                    while !service.is_stopped() && !stopped.is_set() {
                        let current = service.clone();
                        let _ =
                            tokio::task::spawn_blocking(move || current.cleanup_retained_views())
                                .await;
                        stopped.wait(Duration::from_secs(1)).await;
                    }
                })
                .on_stop(move || stop.stop()),
            ),
        );
    }
    if let Some(service) = &containers {
        let service = service.clone();
        workers.start(
            StopAt::Work,
            None,
            Arc::new(Loop::new(
                "container reconciliation",
                |mut stopped| async move {
                    while !stopped.is_set() {
                        let current = service.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            current.reconcile_revoked_agent_work(Duration::from_secs(30))
                        })
                        .await;
                        stopped.wait(Duration::from_secs(1)).await;
                    }
                },
            )),
        );
    }
    if let Some(worker) = &embedding_probes {
        let (worker, stop) = (worker.clone(), worker.clone());
        workers.start(
            StopAt::Work,
            None,
            Arc::new(
                Loop::new("embedding probes", |mut stopped| async move {
                    while !worker.is_stopped() && !stopped.is_set() {
                        let current = worker.clone();
                        let _ = tokio::task::spawn_blocking(move || current.reconcile()).await;
                        stopped.wait(Duration::from_secs(1)).await;
                    }
                })
                .on_stop(move || stop.stop()),
            ),
        );
    }
    let mut scheduler = Scheduler::new(
        store.clone(),
        jobs.clone(),
        admitter.clone(),
        runs.inference.clone(),
        supervisor,
    );
    if let Some(s) = Arc::get_mut(&mut scheduler) {
        s.stop_grace = config.shutdown_grace.min(config.shutdown_bound);
        s.stop_bound = config.shutdown_bound;
    }
    let _ = admitter.scheduler.set(Arc::downgrade(&scheduler));
    let (stop, drain) = (scheduler.clone(), scheduler.clone());
    workers.start(
        StopAt::Schedule,
        // Scheduled runs in flight are awaited after index runs (a cancellation one is
        // executing may be stopping a tree).
        Some(DrainAt::AfterPackages),
        Arc::new(
            Loop::new("scheduler", |_| {
                scheduler.clone().run(config.scheduler_interval)
            })
            .on_stop(move || stop.stop())
            .on_drain(move |deadline| {
                let scheduler = drain.clone();
                async move { scheduler.drain(deadline).await }
            }),
        ),
    );
    // All recovery precedes saved-operation readmission. The core supplies the original
    // identity and immutable reviewed frame; the existing gateway supplies fresh authority.
    let resumes = ResumeDispatcher::new(store.clone(), approvals.clone(), admitter.clone());
    {
        let (store, artifacts) = (store.clone(), artifacts.clone());
        resumes.on_settled(Arc::new(move |approval| {
            crate::tool_turn::publish_held_update(store.as_ref(), &artifacts, approval);
        }));
    }
    let _ = admitter.resumes.set(Arc::downgrade(&resumes));
    let (stop, drain) = (resumes.clone(), resumes.clone());
    workers.start(
        StopAt::Admission,
        Some(DrainAt::Readmission),
        Arc::new(
            Loop::new("approval resumes", |_| {
                resumes.clone().run(config.approval_resume_interval)
            })
            .on_stop(move || stop.stop())
            .on_drain(move |_| {
                let resumes = drain.clone();
                async move { resumes.drain().await }
            }),
        ),
    );
    if let Some(worker) = &prices {
        let (stop, drain) = (worker.clone(), worker.clone());
        workers.start(
            StopAt::Admission,
            Some(DrainAt::BeforeFence),
            Arc::new(
                Loop::new("price refresh", |_| {
                    worker.clone().run(
                        config.price_refresh.as_ref().expect("composed").interval,
                        dispatcher.wakeup(),
                    )
                })
                .on_stop(move || stop.stop())
                .on_drain(move |deadline| {
                    let worker = drain.clone();
                    async move { worker.drain_until(deadline).await }
                }),
            ),
        );
    }
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
        usage,
        providers,
        conventions,
        instruction_boot: instructions.as_ref().map(|service| service.sync()),
        instructions,
        schemas,
        stage: std::sync::atomic::AtomicU8::new(ShutdownStage::Open as u8),
        admission,
        admitter,
        jobs,
        scheduler,
        evals,
        approvals,
        resumes,
        prices,
        price_service,
        notifications,
        keep_awake,
        diagnostics,
        browser,
        process_tools,
        watchers: watchers.clone(),
        containers,
        desktop,
        git,
        shutdown_grace: config.shutdown_grace,
        shutdown_bound: config.shutdown_bound,
        report,
        packages: package_host,
        indexing: index_supervisor,
        workers,
        usage_drain,
    })
}

/// Abandons a package host whose boot did not complete (see `boot`).
struct PackageBootGuard(Option<Arc<PackageHost>>);
impl Drop for PackageBootGuard {
    fn drop(&mut self) {
        if let Some(host) = self.0.take() {
            host.abandon();
        }
    }
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

/// One independently attempted durable provider call.
pub struct ToolDispatch {
    pub call: devworks_core::protocol::provider::ToolCallKey,
    pub outcome: Result<devworks_core::services::sessions::transcript::BatchOutcome, StorageError>,
}

/// One operation-linked tool call whose latest journal state was reconciled independently.
pub struct ToolOperationUpdate {
    pub operation_id: devworks_core::protocol::OperationId,
    pub outcome: Result<Option<devworks_core::protocol::provider::ArtifactRef>, StorageError>,
}

/// One native generation and its independently dispatched tool calls. A completed turn publishes
/// its next context; after `ToolCallsReady`, a missing continuation means at least one call still
/// needs admission or reconciliation.
pub struct NativeTurnProgress {
    pub provider: crate::providers::driver::ProviderInvocation,
    pub tool_dispatches: Vec<ToolDispatch>,
    pub continuation: Option<devworks_core::protocol::provider::ArtifactRef>,
}

impl Runtime {
    pub fn mcp_service(&self) -> Option<Arc<devworks_core::services::mcp::McpClientService>> {
        self.use_cases.mcp_service()
    }
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
    pub fn git(&self) -> Option<&Arc<devworks_core::services::git::GitService>> {
        self.git.as_ref()
    }

    /// The browser service the `browser.*` use cases, its effect consumer and supervision
    /// share (`None` when no browser is composed). Typed completions of accepted browser
    /// commands are read through [`BrowserService::completion`].
    pub fn browser(&self) -> Option<&Arc<BrowserService>> {
        self.browser.as_ref()
    }

    /// The process owner shared by the native UI, agent tools and its durable effect consumer.
    /// Typed completions supply the process ID after an accepted execute settles.
    /// The watcher supervisor, composed with the process tools.
    pub fn watchers(&self) -> Option<&Arc<crate::watch::WatchSupervisor>> {
        self.watchers.as_ref()
    }
    pub fn process_tools(
        &self,
    ) -> Option<&Arc<devworks_core::services::process::tools::ProcessToolService>> {
        self.process_tools.as_ref()
    }

    pub fn containers(
        &self,
    ) -> Option<&Arc<devworks_core::services::containers::service::ContainerService>> {
        self.containers.as_ref()
    }
    /// The desktop service and its scoped typed input completions.
    pub fn desktop(&self) -> Option<&Arc<devworks_core::services::desktop::DesktopService>> {
        self.desktop.as_ref()
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
    /// Host automatic grading supervisor (suite activation is supplied at composition).
    pub fn evals(&self) -> &Arc<EvalSupervisor> {
        &self.evals
    }
    /// How far shutdown has progressed (AD20: the progress surface stays responsive).
    pub fn shutdown_stage(&self) -> ShutdownStage {
        ShutdownStage::from_u8(self.stage.load(Ordering::SeqCst))
    }
    fn enter(&self, stage: ShutdownStage) {
        self.stage.store(stage as u8, Ordering::SeqCst);
    }
    /// Canonical native producers and bounded historical accounting over the same router.
    /// The same owner serves UI/tools, durable body indexing and scheduled maintenance.
    pub fn schemas(&self) -> Option<&Arc<devworks_core::services::schemas::SchemaService>> {
        self.schemas.as_ref()
    }
    pub fn conventions(&self) -> &Arc<devworks_core::services::conventions::ConventionService> {
        &self.conventions
    }
    /// The skill and role instruction owner, when composed.
    pub fn instructions(
        &self,
    ) -> Option<&Arc<devworks_core::services::instructions::InstructionService>> {
        self.instructions.as_ref()
    }
    /// What the boot observation of skill directories and role settings found.
    pub fn instruction_boot_sync(
        &self,
    ) -> Option<&Result<devworks_core::services::instructions::SyncReport, StorageError>> {
        self.instruction_boot.as_ref()
    }
    /// Records the skill set a session adopts at a safe step. When the current effective set
    /// differs from `adopted` (what the session last recorded), a `SkillRevisionsAdopted` event
    /// is appended to the session's history with the selected skills, their exact revisions
    /// and digests, the transitions and any unresolved conflicts; completed steps keep the
    /// revisions they used. Returns the current set and whether it changed.
    pub fn adopt_skills(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        adopted: Option<&devworks_core::services::instructions::EffectiveSkills>,
    ) -> Result<(devworks_core::services::instructions::EffectiveSkills, bool), StorageError> {
        if ctx.session_id() != Some(session_id) {
            return Err(StorageError::ScopeDenied);
        }
        let current = self.effective_skills()?;
        if adopted.is_some_and(|a| a.digest == current.digest) {
            return Ok((current, false));
        }
        self.record_skill_adoption(
            ctx,
            session_id,
            adopted.and_then(|a| a.stamp()).as_ref(),
            current.stamp().as_ref(),
        )?;
        Ok((current, true))
    }
    /// Appends the `SkillRevisionsAdopted` session event of a skill-set change.
    fn record_skill_adoption(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        before: Option<&devworks_core::providers::instructions::SkillStamp>,
        after: Option<&devworks_core::providers::instructions::SkillStamp>,
    ) -> Result<(), StorageError> {
        let payload = skill_adoption_payload(before, after);
        let (session, actor) = (session_id.clone(), ctx.principal().clone());
        devworks_core::ports::storage::run_write(
            self.store.as_ref(),
            devworks_core::ports::storage::WriteOptions::default(),
            move |tx| {
                let stored = tx
                    .sessions()
                    .session(&session)?
                    .ok_or(StorageError::ScopeDenied)?;
                if stored.closed() {
                    return Err(StorageError::ScopeDenied);
                }
                let id = devworks_core::protocol::EventId(
                    tx.new_id(devworks_core::ports::IdKind::Event),
                );
                tx.history()
                    .append(devworks_core::ports::storage::NewEvent {
                        id,
                        actor,
                        project_id: stored.project_id.clone(),
                        root_id: stored.root_id.clone(),
                        operation_id: None,
                        aggregate_kind:
                            devworks_core::services::sessions::transcript::SESSION_AGGREGATE.into(),
                        aggregate_id: stored.id.as_str().to_owned(),
                        aggregate_revision: stored.revision,
                        kind: "SkillRevisionsAdopted".into(),
                        payload,
                    })?;
                Ok(())
            },
        )
    }

    /// The skill set a running agent adopts at its next safe step: sources are observed first,
    /// so a saved, approved or externally changed skill is current; unapproved proposals are
    /// never included. The caller records the returned digest and revisions in its session and
    /// starts a new instruction context epoch when the digest differs from what it adopted.
    pub fn effective_skills(
        &self,
    ) -> Result<devworks_core::services::instructions::EffectiveSkills, StorageError> {
        let service = self.instructions.as_ref().ok_or_else(|| {
            StorageError::unavailable(Unavailable::UnsupportedRuntime, "skills are not composed")
        })?;
        service.sync()?;
        devworks_core::ports::storage::read_snapshot(
            self.store.as_ref(),
            &devworks_core::ports::storage::ReadOptions::default(),
            |read| devworks_core::services::instructions::effective_skills(read.instructions()),
        )
    }
    pub fn providers(&self) -> Option<&Arc<crate::providers::driver::NativeProviders>> {
        self.providers.as_ref()
    }

    /// Drive a host-qualified native generation through durable tool dispatch and publish its
    /// next immutable context, including an ordinary completed assistant turn. Replaying the
    /// same request recovers saved results; it never sends inference or a gateway command twice.
    pub async fn produce_and_dispatch_native_turn(
        &self,
        invocation: crate::providers::driver::NativeInvocation,
        cancellation: Option<devworks_core::ports::storage::Cancellation>,
    ) -> Result<NativeTurnProgress, StorageError> {
        if matches!(
            invocation.request.purpose,
            devworks_core::protocol::provider::InferencePurpose::Compaction
        ) {
            // Compaction output is an artifact, not a conversation turn with a transcript
            // start position or a publishable assistant continuation.
            return Err(StorageError::ScopeDenied);
        }
        let ctx = invocation.context.clone();
        let (session_id, previous) = match &invocation.request.input {
            devworks_core::protocol::provider::InferenceInput::Generation {
                session_id,
                context,
                ..
            } if invocation.logical.session_id.as_ref() == Some(session_id)
                && ctx.session_id() == Some(session_id) =>
            {
                (session_id.clone(), context.clone())
            }
            _ => return Err(StorageError::ScopeDenied),
        };
        let providers = self.providers.as_ref().ok_or_else(|| {
            StorageError::unavailable(
                Unavailable::UnsupportedRuntime,
                "native provider unavailable",
            )
        })?;
        let tool_cancellation = cancellation.clone();
        let provider = providers
            .produce_cancellable(invocation, cancellation)
            .await?;
        let mut tool_dispatches = Vec::new();
        let mut continuation = None;
        if matches!(
            provider.result.outcome,
            Some(
                devworks_core::services::usage::accounting::AttemptEnd::ToolCallsReady
                    | devworks_core::services::usage::accounting::AttemptEnd::Completed
            )
        ) {
            let has_tools = provider.result.outcome
                == Some(devworks_core::services::usage::accounting::AttemptEnd::ToolCallsReady);
            let attempt_id = provider.result.attempt_id.as_ref().ok_or_else(|| {
                StorageError::InvalidRecord("tool turn lacks settled attempt identity".into())
            })?;
            let binding = provider.result.binding.as_ref().ok_or_else(|| {
                StorageError::InvalidRecord("tool turn lacks native binding".into())
            })?;
            let after_sequence = provider.result.turn_start_sequence.ok_or_else(|| {
                StorageError::InvalidRecord("tool turn lacks durable start position".into())
            })?;
            if has_tools {
                tool_dispatches = self.dispatch_pending_tool_calls_cancellable(
                    &ctx,
                    &session_id,
                    tool_cancellation.as_ref(),
                )?;
            }
            let unanswered =
                !crate::tool_turn::pending_calls(self.store.as_ref(), &ctx, &session_id)?
                    .is_empty();
            if !unanswered {
                let material = crate::tool_turn::continuation_context(
                    self.store.as_ref(),
                    &self.artifacts,
                    &ctx,
                    crate::tool_turn::TurnSource {
                        session_id: &session_id,
                        attempt_id,
                        after_sequence,
                        has_tools,
                    },
                    binding,
                    &previous,
                )?;
                continuation = Some(crate::tool_turn::publish_continuation(
                    self.store.as_ref(),
                    &self.artifacts,
                    &ctx,
                    &session_id,
                    attempt_id,
                    binding,
                    material,
                )?);
            }
        }
        Ok(NativeTurnProgress {
            provider,
            tool_dispatches,
            continuation,
        })
    }

    /// Resume one durable provider call through the shared gateway, recording its single
    /// initial response in the owning session. A caller can retry the same call after restart.
    pub fn dispatch_tool_call(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        call: &devworks_core::protocol::provider::ToolCallKey,
    ) -> Result<devworks_core::services::sessions::transcript::BatchOutcome, StorageError> {
        self.dispatch_tool_call_cancellable(ctx, session_id, call, None)
    }

    fn dispatch_tool_call_cancellable(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        call: &devworks_core::protocol::provider::ToolCallKey,
        cancellation: Option<&Cancellation>,
    ) -> Result<devworks_core::services::sessions::transcript::BatchOutcome, StorageError> {
        let providers = self.providers.as_ref().ok_or_else(|| {
            StorageError::unavailable(
                Unavailable::UnsupportedRuntime,
                "native provider unavailable",
            )
        })?;
        crate::tool_turn::dispatch_one(
            self,
            self.store.as_ref(),
            &self.artifacts,
            providers.schemas().as_ref(),
            ctx,
            session_id,
            call,
            cancellation,
        )
    }

    /// Record a final cancellation only if this durable call has no dispatch-start marker.
    /// A call already started requires same-key recovery or effect reconciliation.
    pub fn cancel_tool_call_before_dispatch(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        call: &devworks_core::protocol::provider::ToolCallKey,
    ) -> Result<devworks_core::services::sessions::transcript::BatchOutcome, StorageError> {
        crate::tool_turn::cancel_before_dispatch(self.store.as_ref(), ctx, session_id, call)
    }

    /// Resume the durable unanswered calls of a session. Each call is handled independently:
    /// an approval, status uncertainty or failure leaves the other calls eligible to run.
    pub fn dispatch_pending_tool_calls(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
    ) -> Result<Vec<ToolDispatch>, StorageError> {
        self.dispatch_pending_tool_calls_cancellable(ctx, session_id, None)
    }

    /// A stopped turn cancels calls that have not started. Started calls retain their durable
    /// marker and must reconcile under the same identity when work resumes.
    pub fn dispatch_pending_tool_calls_cancellable(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        cancellation: Option<&Cancellation>,
    ) -> Result<Vec<ToolDispatch>, StorageError> {
        let calls = crate::tool_turn::pending_calls(self.store.as_ref(), ctx, session_id)?;
        if calls.is_empty() {
            return Ok(Vec::new());
        }
        // A bounded worker set allows independent gateway calls to finish out of order. Each
        // writer reads the current sequence in its own serialized transaction, so a slow sibling
        // cannot reserve a transcript position or cancel completed results.
        let next = std::sync::atomic::AtomicUsize::new(0);
        let results = std::sync::Mutex::new(
            std::iter::repeat_with(|| None)
                .take(calls.len())
                .collect::<Vec<Option<ToolDispatch>>>(),
        );
        std::thread::scope(|scope| {
            for _ in 0..calls.len().min(8) {
                let calls = &calls;
                let next = &next;
                let results = &results;
                scope.spawn(move || {
                    loop {
                        let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(call) = calls.get(index) else { break };
                        let outcome = if cancellation.is_some_and(Cancellation::is_cancelled) {
                            self.cancel_tool_call_before_dispatch(ctx, session_id, call)
                        } else {
                            match self.dispatch_tool_call_cancellable(
                                ctx,
                                session_id,
                                call,
                                cancellation,
                            ) {
                                Err(StorageError::Cancelled) => {
                                    self.cancel_tool_call_before_dispatch(ctx, session_id, call)
                                }
                                other => other,
                            }
                        };
                        results.lock().unwrap_or_else(|p| p.into_inner())[index] =
                            Some(ToolDispatch {
                                call: call.clone(),
                                outcome,
                            });
                    }
                });
            }
        });
        Ok(results
            .into_inner()
            .unwrap_or_else(|p| p.into_inner())
            .into_iter()
            .map(|result| result.expect("joined tool worker"))
            .collect())
    }

    /// Read a closed tool turn in native ordinal order after every emitted call has its one
    /// durable initial response. The caller can then encode the next provider request without
    /// depending on the order in which independent workers finished.
    pub fn resolved_tool_turn(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        attempt_id: &devworks_core::protocol::AttemptId,
        after_sequence: i64,
    ) -> Result<Vec<crate::ResolvedToolCall>, StorageError> {
        crate::tool_turn::resolved_turn(
            self.store.as_ref(),
            ctx,
            session_id,
            attempt_id,
            after_sequence,
        )
    }

    /// Record a newer journal state for an operation linked to this session's accepted tool
    /// response. The first response remains unchanged; replaying the same revision adds nothing.
    pub fn record_tool_operation_update(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        operation_id: &devworks_core::protocol::OperationId,
    ) -> Result<Option<devworks_core::protocol::provider::ArtifactRef>, StorageError> {
        crate::tool_turn::record_operation_update(
            self.store.as_ref(),
            &self.artifacts,
            ctx,
            session_id,
            operation_id,
        )
    }

    /// Reconcile every operation linked to an initial tool response without making a failure
    /// on one operation suppress updates for unrelated calls.
    pub fn reconcile_tool_operations(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
    ) -> Result<Vec<ToolOperationUpdate>, StorageError> {
        let linked = crate::tool_turn::linked_operations(self.store.as_ref(), ctx, session_id)?;
        let mut operations: Vec<_> = linked
            .into_iter()
            .filter_map(|call| call.operation_id)
            .collect();
        operations
            .sort_by(|a: &devworks_core::protocol::OperationId, b| a.0.as_str().cmp(b.0.as_str()));
        operations.dedup();
        Ok(operations
            .into_iter()
            .map(|operation_id| ToolOperationUpdate {
                outcome: self.record_tool_operation_update(ctx, session_id, &operation_id),
                operation_id,
            })
            .collect())
    }

    /// On a session wake, record the current states of linked tool operations and append their
    /// new facts to the previous native continuation. A redirect or epoch change requires the
    /// caller to rebuild context before requesting more inference.
    pub fn refresh_native_tool_context(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        previous: &devworks_core::protocol::provider::ArtifactRef,
    ) -> Result<devworks_core::protocol::provider::ArtifactRef, StorageError> {
        for update in self.reconcile_tool_operations(ctx, session_id)? {
            update.outcome?;
        }
        crate::tool_turn::append_operation_updates(
            self.store.as_ref(),
            &self.artifacts,
            ctx,
            session_id,
            previous,
        )
    }

    /// Recover the exact context last published for this session, including frozen native
    /// aliases. The returned cursor tells the caller whether later transcript facts need
    /// reconciliation before another provider request.
    pub fn recover_native_context_head(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
    ) -> Result<
        Option<(
            devworks_core::protocol::provider::ArtifactRef,
            devworks_core::protocol::provider::NativeBinding,
            i64,
        )>,
        StorageError,
    > {
        crate::tool_turn::recover_context_head(
            self.store.as_ref(),
            &self.artifacts,
            ctx,
            session_id,
        )
    }

    /// Fix the first provider prompt and binding for a fresh session before its first send.
    /// A restart recovers this head verbatim; a matching retry does not create another version.
    pub fn publish_initial_native_context(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        binding: &devworks_core::protocol::provider::NativeBinding,
        context: &devworks_core::providers::context::ProviderContext,
    ) -> Result<devworks_core::protocol::provider::ArtifactRef, StorageError> {
        crate::tool_turn::publish_initial_context(
            self.store.as_ref(),
            &self.artifacts,
            ctx,
            session_id,
            binding,
            context,
        )
    }

    /// Build the next provider context from the immutable request context and settled native
    /// exchange. The caller publishes the returned material before a new request.
    pub fn tool_continuation_context(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        attempt_id: &devworks_core::protocol::AttemptId,
        after_sequence: i64,
        binding: &devworks_core::protocol::provider::NativeBinding,
        previous: &devworks_core::protocol::provider::ArtifactRef,
    ) -> Result<devworks_core::providers::context::ProviderContext, StorageError> {
        crate::tool_turn::continuation_context(
            self.store.as_ref(),
            &self.artifacts,
            ctx,
            crate::tool_turn::TurnSource {
                session_id,
                attempt_id,
                after_sequence,
                has_tools: true,
            },
            binding,
            previous,
        )
        .map(|material| material.context)
    }

    /// Record an explicit provider-change context epoch after all emitted calls have their
    /// initial responses. Known tool exchanges survive; opaque provider blocks are recorded as
    /// omitted sources and cannot leak into the next provider's request.
    pub fn change_provider_epoch(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        previous_context: &devworks_core::protocol::provider::ArtifactRef,
        previous_binding: &devworks_core::protocol::provider::NativeBinding,
        next_binding: &devworks_core::protocol::provider::NativeBinding,
    ) -> Result<devworks_core::protocol::provider::ArtifactRef, StorageError> {
        crate::context_epoch::provider_change(
            self.store.as_ref(),
            &self.artifacts,
            ctx,
            session_id,
            previous_context,
            previous_binding,
            next_binding,
        )
    }

    /// Adopt changed repository guidance at an explicit safe context boundary. Unchanged
    /// source stamps reuse the existing snapshot; a changed bundle creates a new immutable
    /// context and records any opaque native state that cannot cross the epoch.
    pub fn adopt_instruction_epoch(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        previous_context: &devworks_core::protocol::provider::ArtifactRef,
        previous_binding: &devworks_core::protocol::provider::NativeBinding,
        targets: &[(Id, Vec<String>)],
    ) -> Result<devworks_core::protocol::provider::ArtifactRef, StorageError> {
        if ctx.session_id() != Some(session_id) {
            return Err(StorageError::ScopeDenied);
        }
        let previous_epoch = previous_binding.context_epoch.get();
        devworks_core::ports::storage::read_snapshot(
            self.store.as_ref(),
            &devworks_core::ports::storage::ReadOptions::default(),
            |read| {
                let session = read
                    .sessions()
                    .session(session_id)?
                    .ok_or(StorageError::ScopeDenied)?;
                if session.closed() || session.context_epoch != previous_epoch {
                    return Err(StorageError::ScopeDenied);
                }
                Ok(())
            },
        )?;
        let source = crate::context_epoch::current_source(
            self.store.as_ref(),
            &self.artifacts,
            ctx,
            session_id,
            previous_context,
            previous_binding,
        )?;
        let mut base = source.context;
        let old = std::mem::take(&mut base.instruction_stamps);
        let old_count: usize = old.iter().map(|stamp| stamp.sources.len()).sum();
        drop_stamped_guidance(&mut base.instructions, old_count)?;
        let old_skills = base.skill_stamp.take();
        if old_skills.is_some() {
            base.instructions.pop();
        }
        // An assignment's context starts with its role's instructions, captured when the work
        // began; a role changed since is adopted here, like changed guidance and skills.
        let mut role_changed = false;
        if let Some(role) = self.current_role_prefix(ctx)?
            && let Some(rebound) = with_role(&base.instructions, role)
        {
            base.instructions = rebound;
            role_changed = true;
        }
        let rebound = self.bind_provider_instructions(ctx, base, targets)?;
        if !role_changed && rebound.instruction_stamps == old && rebound.skill_stamp == old_skills {
            return Ok(source.artifact);
        }
        // Recorded in the transaction that publishes the epoch: a refused or failed transition
        // records no adoption, and a retry records it once.
        let adoption = (rebound.skill_stamp != old_skills)
            .then(|| skill_adoption_payload(old_skills.as_ref(), rebound.skill_stamp.as_ref()));
        let next_binding = devworks_core::protocol::provider::NativeBinding {
            context_epoch: devworks_core::protocol::Counter::new(
                previous_epoch
                    .checked_add(1)
                    .ok_or_else(|| StorageError::InvalidRecord("context epoch overflow".into()))?,
            )
            .map_err(|e| StorageError::InvalidRecord(e.into()))?,
            ..previous_binding.clone()
        };
        let (converted, omitted) = rebound
            .converted_for_new_epoch(previous_binding, &next_binding)
            .map_err(|e| StorageError::InvalidRecord(format!("instruction adoption: {e:?}")))?;
        crate::context_epoch::publish(
            self.store.as_ref(),
            &self.artifacts,
            ctx,
            session_id,
            crate::context_epoch::EpochChange {
                previous_epoch,
                binding: next_binding,
                observed_sequence: source.observed_sequence,
                source: source.artifact,
                reason: devworks_core::protocol::provider::ContextChangeReason::InstructionAdoption,
                context: converted,
                omitted,
                adoption,
            },
        )
    }

    /// The current role instructions of the agent assignment `ctx` runs, as the role prefix of
    /// its provider context (empty when the role has no instructions); `None` outside an agent
    /// assignment.
    fn current_role_prefix(
        &self,
        ctx: &InvocationContext,
    ) -> Result<Option<Vec<devworks_core::providers::context::TextBlock>>, StorageError> {
        let Some(assignment) = ctx.assignment_id() else {
            return Ok(None);
        };
        devworks_core::ports::storage::read_snapshot(
            self.store.as_ref(),
            &devworks_core::ports::storage::ReadOptions::default(),
            |read| {
                let Some(source) = read.runs().agent_assignment_context(assignment)? else {
                    return Ok(None);
                };
                let role =
                    devworks_core::settings::catalog::AgentRole::parse(source.role_id.as_str())
                        .ok_or_else(|| StorageError::InvalidRecord("assignment role".into()))?;
                let text = devworks_core::settings::service::role_instructions(
                    read.configuration(),
                    role,
                    Some(&source.project_id),
                )
                .map_err(|failure| match failure {
                    devworks_core::settings::service::Failure::Storage(e) => e,
                    devworks_core::settings::service::Failure::Refused(_) => {
                        StorageError::InvalidRecord("role instructions".into())
                    }
                })?
                .text;
                Ok(Some(if text.is_empty() {
                    Vec::new()
                } else {
                    vec![devworks_core::providers::context::TextBlock { text, cache: None }]
                }))
            },
        )
    }

    pub fn accounting(&self) -> &Arc<crate::usage::UsageRuntime> {
        &self.usage
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
    /// Index supervision, when indexing is composed.
    pub fn indexing(&self) -> Option<&Arc<IndexSupervisor>> {
        self.indexing.as_ref()
    }

    /// Resolve current path-scoped repository instructions from the checkout the invocation is
    /// authorized to use. A missing/changed source is observed before each new dispatch; the
    /// caller binds the returned revision and sources into its context epoch.
    pub fn effective_instructions(
        &self,
        ctx: &InvocationContext,
        repository_id: &Id,
        target_paths: &[String],
    ) -> Result<devworks_core::providers::instructions::InstructionBundle, StorageError> {
        crate::instruction_guard::InstructionGuard::new(
            self.store.clone(),
            self.indexing.as_ref().map(|ix| ix.env().source.clone()),
        )
        .resolve(ctx, repository_id, target_paths)
    }

    /// The provider dispatch gate for instruction material, as the provider driver applies it:
    /// repository guidance and the skill set bound to `context` must still be current.
    pub fn verify_instruction_context(
        &self,
        ctx: &InvocationContext,
        context: &devworks_core::providers::context::ProviderContext,
        targets: Option<&[(Id, Vec<String>)]>,
    ) -> Result<(), StorageError> {
        crate::instruction_guard::InstructionGuard::new(
            self.store.clone(),
            self.indexing.as_ref().map(|ix| ix.env().source.clone()),
        )
        .with_skills(self.instructions.clone())
        .verify(ctx, context, targets)
    }

    /// Compose the effective instruction prefix and its exact source stamps for one provider
    /// request. The trusted turn caller supplies its authorized repository and target paths;
    /// an empty path set means repository-root guidance only.
    pub fn bind_provider_instructions(
        &self,
        ctx: &InvocationContext,
        base: devworks_core::providers::context::ProviderContext,
        targets: &[(Id, Vec<String>)],
    ) -> Result<devworks_core::providers::context::ProviderContext, StorageError> {
        let selected = if targets.is_empty() {
            if let Some(bound) = ctx.worktree() {
                vec![(bound.repository_id().clone(), Vec::new())]
            } else if let Some(project) = ctx.project_id() {
                devworks_core::ports::storage::read_snapshot(
                    self.store.as_ref(),
                    &devworks_core::ports::storage::ReadOptions::default(),
                    |read| {
                        Ok(read
                            .projects()
                            .members(project)?
                            .into_iter()
                            .map(|id| (id, Vec::new()))
                            .collect())
                    },
                )?
            } else {
                Vec::new()
            }
        } else {
            targets.to_vec()
        };
        if selected.len() > 64 {
            return Err(StorageError::InvalidRecord(
                "too many instruction repositories".into(),
            ));
        }
        let mut bundles = Vec::with_capacity(selected.len());
        for (repository, paths) in &selected {
            let scope = if paths.is_empty() {
                vec![".".into()]
            } else {
                paths.clone()
            };
            bundles.push(self.effective_instructions(ctx, repository, &scope)?);
        }
        let base = match crate::instruction_guard::InstructionGuard::new(self.store.clone(), None)
            .with_skills(self.instructions.clone())
            .current_skills()?
        {
            Some(stamp) => devworks_core::providers::instructions::bind_skills(base, stamp)
                .map_err(|e| StorageError::InvalidRecord(format!("skill binding: {e:?}")))?,
            None => base,
        };
        devworks_core::providers::instructions::bind_context(base, &bundles)
            .map_err(|error| StorageError::InvalidRecord(format!("instruction binding: {error:?}")))
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
        self.query_with(
            ctx,
            operation,
            raw,
            &devworks_core::ports::storage::ReadOptions::default(),
        )
    }

    /// The native caller owns this exact cancellation/deadline. It is passed unchanged to
    /// preparation, the read snapshot and any auxiliary native inference source.
    pub fn query_with(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
        options: &devworks_core::ports::storage::ReadOptions,
    ) -> QueryOutcome {
        let outcome =
            self.use_cases
                .query_with(&self.gateway(), ctx, operation, raw, options.clone());
        if operation == DISCOVERS_REPOSITORIES
            && let Some(ix) = &self.indexing
            && discovery_scans_inline(ctx, &outcome)
        {
            // Only after the gateway admitted the query (authority, store and epoch checks) and
            // for a person or the host: discovery then answers from a fresh scan of the
            // invocation's sections. Other readers answer from the recorded registrations, which
            // the supervisor's background discovery keeps current. Shutdown refuses the scan.
            let open = self.admission.read().unwrap_or_else(|p| p.into_inner());
            if *open {
                return outcome;
            }
            let env = ix.env().clone();
            let cancelled = || {
                options
                    .cancellation
                    .as_ref()
                    .is_some_and(devworks_core::ports::storage::Cancellation::is_cancelled)
            };
            // A scan that fails leaves the recorded registrations to answer.
            let _ = devworks_core::services::indexing::discover::scan(&env, ctx, None, &cancelled);
            drop(open);
            return self.use_cases.query_with(
                &self.gateway(),
                ctx,
                operation,
                raw,
                options.clone(),
            );
        }
        outcome
    }

    pub fn command(&self, ctx: &InvocationContext, operation: &str, raw: &[u8]) -> CommandOutcome {
        self.command_with(ctx, operation, raw, WriteOptions::default())
    }

    /// Dedicated trusted native-human interaction. The ordinary command/tool path cannot
    /// mint this private exact-content witness, even for a context whose kind is Human.
    pub fn record_human_decision(&self, ctx: &InvocationContext, frame: &[u8]) -> CommandOutcome {
        let outcome = self.decide_as_human(ctx, frame);
        // A rejection settles an approval staged for an agent's refused call; tell that agent.
        if let Ok(CommandReply::Recorded { receipt, .. }) = &outcome
            && let DomainDecision::Applied {
                result:
                    devworks_core::protocol::results::OperationResult::ApprovalsRecordHumanDecision {
                        value,
                    },
            } = &receipt.decision
        {
            crate::tool_turn::publish_held_update(self.store.as_ref(), &self.artifacts, &value.id);
        }
        outcome
    }

    fn decide_as_human(&self, ctx: &InvocationContext, frame: &[u8]) -> CommandOutcome {
        const OPERATION: &str = "approvals.record_human_decision";
        if self
            .desktop
            .as_ref()
            .is_some_and(|desktop| desktop.automation_may_be_active())
        {
            return self.command(ctx, OPERATION, frame);
        }
        let decoded = devworks_core::admission::decode(
            devworks_core::admission::Selection {
                operation: OPERATION,
                mode: devworks_core::registry::Mode::Command,
            },
            frame,
        );
        if let Ok(decoded) = decoded
            && let devworks_core::operations::OperationInput::Approvals(
                devworks_core::operations::approvals::Input::RecordHumanDecision(input),
            ) = decoded.input
            && let Some(bound) = devworks_core::context::HostIngress::new(&*self.store)
                .human_decision_context(ctx, &input)
        {
            return self.command(&bound, OPERATION, frame);
        }
        // Preserve the normal typed admission/authority refusal. No fallback mints a witness.
        self.command(ctx, OPERATION, frame)
    }

    /// Dedicated native confirmation. Generic command/MCP dispatch never mints
    /// this proof; current authority and manifest freshness remain required.
    pub fn confirm_project_purge(&self, ctx: &InvocationContext, frame: &[u8]) -> CommandOutcome {
        const OPERATION: &str = "projects.purge";
        if self
            .desktop
            .as_ref()
            .is_some_and(|desktop| desktop.automation_may_be_active())
        {
            return self.command(ctx, OPERATION, frame);
        }
        let decoded = devworks_core::admission::decode(
            devworks_core::admission::Selection {
                operation: OPERATION,
                mode: devworks_core::registry::Mode::Command,
            },
            frame,
        );
        if let Ok(decoded) = decoded
            && let devworks_core::operations::OperationInput::Projects(
                devworks_core::operations::projects::Input::Purge(input),
            ) = decoded.input
            && let Some(bound) = devworks_core::context::HostIngress::new(&*self.store)
                .project_purge_context(ctx, &input)
        {
            return self.command(&bound, OPERATION, frame);
        }
        self.command(ctx, OPERATION, frame)
    }

    pub fn approvals(&self) -> &Arc<ApprovalService> {
        &self.approvals
    }
    /// Host hint after binding the canonical local user or producing persistent inbox rows.
    pub fn notification_wakeup(&self) {
        self.resumes.notify();
        if let Some(worker) = &self.notifications {
            worker.notify();
        }
    }
    /// Unknown focus is conservative; only explicit unfocused state permits OS delivery.
    pub fn set_notification_focus(&self, focused: Option<bool>) {
        if let Some(worker) = &self.notifications {
            worker.set_focus(focused);
        }
    }
    pub fn notification_permission(&self) -> OsNotificationPermission {
        self.notifications.as_ref().map_or_else(
            || {
                OsNotificationPermission::Unavailable(
                    "no native notification adapter configured".into(),
                )
            },
            |worker| worker.permission(),
        )
    }
    /// Run through the host worker, never on the GUI thread.
    pub async fn request_notification_permission(&self) -> OsNotificationPermission {
        match &self.notifications {
            Some(worker) => worker.request_permission().await,
            None => self.notification_permission(),
        }
    }
    pub fn price_service(&self) -> &Arc<devworks_core::services::prices::PriceService> {
        &self.price_service
    }
    pub fn price_refreshes(&self) -> Option<&Arc<crate::prices::PriceSupervisor>> {
        self.prices.as_ref()
    }
    pub fn approval_resumes(&self) -> &Arc<ResumeDispatcher> {
        &self.resumes
    }
    /// Changes each time an admitted command may have changed domain state: a new receipt
    /// that is not a refusal, or an unknown outcome. Approval decisions and their readmissions
    /// are admitted commands, so a caller waiting on one wakes here instead of polling.
    pub fn state_changes(&self) -> watch::Receiver<u64> {
        self.admitter.changes.subscribe()
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
        self.admitter.command_with(ctx, operation, raw, options)
    }

    /// The jobs service the `jobs.*` use cases, the scheduler and the lifecycle hooks share.
    pub fn jobs(&self) -> &Arc<JobService> {
        &self.jobs
    }
    /// The schedule runner (ticks are paced by [`RuntimeConfig::scheduler_interval`]).
    pub fn scheduler(&self) -> &Arc<Scheduler> {
        &self.scheduler
    }

    /// Stops admission of new deliveries, collection, housekeeping and the diagnostics monitor,
    /// waits for the delivery loop, the collector, the housekeeper, the monitor and every
    /// in-flight delivery to finish (an
    /// asynchronous handler gets its consumer's stop grace), then stops every owned process tree
    /// within [`RuntimeConfig::shutdown_bound`] (recording the `Shutdown` reconciliation of
    /// resources) and releases the keep-awake request. Every tree is stopped natively even when
    /// the store refuses writes; the report carries each tree's outcome and any write failure.
    pub async fn shutdown(&self) -> ShutdownReport {
        // One bound for the whole shutdown (AD20): every later step gets what is left of it.
        let deadline = tokio::time::Instant::now() + self.shutdown_bound;
        // Schedule admission freezes first: no occurrence is claimed and no run started from
        // here on (runs already dispatching finish or are reconciled at the next boot).
        self.workers.stop_at(StopAt::Schedule);
        let refresh_shutdown = self
            .use_cases
            .connection_service()
            .map(|connections| connections.close())
            .transpose();
        // Usage, evaluation, approval resumes, price refresh and notifications stop admitting.
        self.workers.stop_at(StopAt::Admission);
        // Preparation/admission/settlement are one drained unit: never orphan a blocking
        // readmission or let its outbox CAS write after shutdown has returned. Usage, price
        // refresh and notifications drain next.
        self.workers.drain(DrainAt::Readmission, deadline).await;
        self.workers.drain(DrainAt::BeforeFence, deadline).await;
        let inference_shutdown = self
            .usage_drain
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .unwrap_or_else(|| {
                Ok(crate::usage::UsageDrain {
                    unclosed: Vec::new(),
                    pending_preparations: 0,
                })
            });
        let left = || deadline.saturating_duration_since(tokio::time::Instant::now());
        // The diagnostics monitor is only *signalled* here, and joined after the run-control
        // transition below: a pass in progress (a full integrity check of a large store, say)
        // must never use up the bound before the live work is held for Continue. Once signalled
        // it starts no further step or probe, so no retention, probe or sweep write starts after
        // the admission fence closes; none of it is a command, so none of it changes run control.
        self.workers.stop_at(StopAt::Fence);
        // AD20 step 1: fence admission in memory and cancel retained inbox waits, then commit the
        // run-control transition: live feature/issue work is held for manual Continue and asked
        // to stop; launches get the cooperative grace. A storage failure here is not fatal: the
        // next boot holds the same work from its boot epoch. Close admission first: wait (off the
        // async thread) for every
        // command in admission or commit, so none commits after the quiescence snapshot.
        self.enter(ShutdownStage::DrainingAdmission);
        let admission = self.admission.clone();
        let receive_waiters = self.admitter.receive_waiters.clone();
        let _ = tokio::task::spawn_blocking(move || {
            *admission.write().unwrap_or_else(|p| p.into_inner()) = true;
            for token in receive_waiters
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .values()
            {
                token.cancel();
            }
        })
        .await;
        // No blocking evaluator job may outlive the runtime: drain its local write fence.
        self.workers.drain(DrainAt::AfterFence, deadline).await;
        self.enter(ShutdownStage::Quiescing);
        let _ = self.runs.quiesce(self.shutdown_grace.min(left())).await;
        self.enter(ShutdownStage::StoppingWork);
        self.stop();
        // Every boot-started task (delivery, collection, housekeeping, the monitor pass in
        // progress, run supervision, the schedule loop), the deliveries and the scheduled runs in flight are awaited only within what
        // is left of the one bound (CONTROL-09); past it each is abandoned, and the next boot
        // recovers what it left: a unit of work it already admitted commits or rolls back
        // alone, a claim's lease expires into recovery, an interrupted operation is classified.
        // Supervision admits no further root stop once stopping; one it is executing ends
        // within its own bound, and the owned-tree stop below covers its trees.
        let _ = self.workers.join(deadline).await;
        // In-flight deliveries.
        self.workers.drain(DrainAt::AfterJoin, deadline).await;
        // Browser sessions close through the process owner (their records settle and profiles
        // are removed) before the remaining owned trees are stopped. Like every wait above, it
        // starts only after the run-control hold committed and is abandoned at the one deadline:
        // a close that does not return in time leaves its trees to the owned-tree stop below.
        if let Some(b) = self.browser.clone() {
            let closing = tokio::task::spawn_blocking(move || b.shutdown());
            let _ = dispatch::first(tokio::time::sleep_until(deadline), closing).await;
        }
        // Package helpers get their cooperative shutdown (cancel in-flight invocations,
        // host.shutdown) after the hold above and within what is left of the same bound: the
        // host stops every helper by that deadline, and its thread is not awaited past it; the
        // process owner below stops whatever remains.
        if let Some(host) = self.packages.clone() {
            let bound = left();
            let stopping = tokio::task::spawn_blocking(move || host.shutdown(bound));
            let _ = dispatch::first(tokio::time::sleep_until(deadline), stopping).await;
        }
        // Index runs, then scheduled runs, are waited for within the same deadline; their trees
        // are owned trees, so the stop below kills any that remain.
        self.workers.drain(DrainAt::AfterPackages, deadline).await;
        let container_shutdown = if let Some(service) = self.containers.clone() {
            let bound = left();
            let mut task = tokio::task::spawn_blocking(move || service.shutdown_agent_work(bound));
            Some(
                match dispatch::first(tokio::time::sleep_until(deadline), &mut task).await {
                    dispatch::Either::Right(result) => result.unwrap_or_else(|_| {
                        devworks_core::services::containers::ShutdownReport {
                            read_error: Some("container shutdown worker panicked".into()),
                            ..Default::default()
                        }
                    }),
                    dispatch::Either::Left(()) => {
                        task.abort();
                        devworks_core::services::containers::ShutdownReport {
                            read_error: Some("container shutdown deadline elapsed".into()),
                            ..Default::default()
                        }
                    }
                },
            )
        } else {
            None
        };
        let bound = left();
        let mut stopped = self
            .housekeeper
            .shutdown(self.shutdown_grace.min(bound), bound)
            .await;
        stopped.containers = container_shutdown;
        // Watchers whose processes the stop above ended are interrupted, and a probe's stopped
        // run is forgotten so it runs again on reopen (WATCH-08). A failure leaves them to the
        // next boot, which observes the stopped trees instead.
        if let Some(watchers) = &self.watchers {
            let watchers = watchers.clone();
            let stopped: std::collections::HashSet<_> =
                stopped.stopped.iter().map(|(id, _)| id.clone()).collect();
            let budget = left();
            let deadline = std::time::Instant::now() + budget;
            let pass =
                tokio::task::spawn_blocking(move || watchers.close_at_shutdown(&stopped, deadline));
            let _ = tokio::time::timeout(budget, pass).await;
        }
        match refresh_shutdown {
            Err(error) if stopped.reconciliation.is_ok() => stopped.reconciliation = Err(error),
            Ok(Some(report)) if !report.unknown_operations.is_empty() => {
                stopped.reconciliation = Err(
                    devworks_core::ports::storage::StorageError::unavailable(
                        devworks_core::ports::storage::Unavailable::UnsupportedRuntime,
                        format!(
                            "{} token refreshes have unknown outcomes; {} external secret handles remain unpublished",
                            report.unknown_operations.len(),
                            report.unpublished_handles.len()
                        ),
                    ),
                );
            }
            _ => {}
        }
        match inference_shutdown {
            Ok(drain) => {
                stopped.unclosed_inference_attempts = drain.unclosed;
                if !stopped.unclosed_inference_attempts.is_empty() || drain.pending_preparations > 0
                {
                    let service = self.diagnostics.clone();
                    let ids = stopped.unclosed_inference_attempts.clone();
                    let _=tokio::task::spawn_blocking(move||{
                        use devworks_core::services::diagnostics::LogEntry;
                        use devworks_core::protocol::records::{Severity,DiagnosticOutcome};
                        let mut entry=LogEntry::new("router.usage","unclosed-inference",Severity::Warning,
                            format!("{} inference transports were not verified closed before shutdown ({} preparations still settling); custody and reservations remain held.",ids.len(),drain.pending_preparations));
                        entry.operation=Some("runtime.shutdown".into());entry.stage=Some("shutdown".into());entry.outcome=Some(DiagnosticOutcome::Partial);
                        entry.target_kind=Some("inference_attempt".into());entry.target_id=ids.first().cloned();
                        let _=service.log(entry);
                    }).await;
                }
            }
            Err(error) => {
                if stopped.reconciliation.is_ok() {
                    stopped.reconciliation = Err(error);
                }
            }
        }
        // AD20 step 4: final known activation state.
        let _ = self.runs.finish().await;
        self.keep_awake.apply(false);
        self.enter(ShutdownStage::Done);
        stopped
    }

    /// Signals every registered worker (each loop then ends, so shutdown can join it) and
    /// closes outgoing connections.
    fn stop(&self) {
        if let Some(connections) = self.use_cases.connection_service() {
            let _ = connections.close();
        }
        self.workers.stop_all();
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

/// Whether an `indexing.discover` query scans before it answers: only once the gateway admitted
/// it (a result, not a refusal or failure), and only for a person or the host. Agents, packages,
/// MCP clients and schedules read the recorded registrations.
pub(crate) fn discovery_scans_inline(ctx: &InvocationContext, outcome: &QueryOutcome) -> bool {
    matches!(
        outcome,
        Ok(devworks_core::protocol::results::DomainQueryReply::Result { .. })
    ) && matches!(
        ctx.principal_kind(),
        devworks_core::context::PrincipalKind::Human | devworks_core::context::PrincipalKind::Host
    )
}

/// The run an `indexing.cancel` frame names.
fn cancelled_run(raw: &[u8]) -> Option<Id> {
    let v = serde_json::from_slice::<serde_json::Value>(raw).ok()?;
    let id = v.get("command")?.get("operationId")?.as_str()?;
    Id::new(id).ok()
}

/// The project a `projects.archive` frame names.
fn archived_project(raw: &[u8]) -> Option<devworks_core::protocol::ProjectId> {
    let v = serde_json::from_slice::<serde_json::Value>(raw).ok()?;
    let id = v.get("body")?.get("projectId")?.as_str()?;
    Id::new(id).ok().map(devworks_core::protocol::ProjectId)
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
        // Every registered worker is signalled; its loop is left to end on its own.
        self.stop();
        // The write fences of evaluation, approval resumes, price refresh and notifications
        // close at once, also fencing jobs that have not begun yet.
        self.evals.close();
        self.resumes.close();
        if let Some(worker) = &self.prices {
            worker.close();
        }
        if let Some(worker) = &self.notifications {
            worker.close();
        }
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

/// The one command path: the admission fence shutdown closes, the gateway over the registered use
/// cases, and the post-commit wakeups (reply delivery, run supervision, collection, the live log,
/// keep-awake, the scheduler). UI, agent tools and the scheduler's native runs all go through it.
struct Admitter {
    store: Arc<dyn ApplicationStore>,
    ids: Arc<dyn IdSource>,
    authority: Arc<dyn AuthorityPort>,
    use_cases: UseCases,
    admission: Arc<std::sync::RwLock<bool>>,
    receive_waiters: Arc<Mutex<BTreeMap<u64, Cancellation>>>,
    next_receive_waiter: AtomicU64,
    dispatcher: Dispatcher,
    runs: Arc<RunSupervisor>,
    usage: Arc<crate::usage::UsageRuntime>,
    collector: Arc<Collector>,
    diagnostics: Arc<DiagnosticService>,
    keep_awake: Arc<KeepAwake>,
    scheduler: OnceLock<Weak<Scheduler>>,
    evals: Arc<EvalSupervisor>,
    resumes: OnceLock<Weak<ResumeDispatcher>>,
    prices: Option<Arc<crate::prices::PriceSupervisor>>,
    notifications: Option<Arc<OsNotificationSupervisor>>,
    indexing: Option<Arc<IndexSupervisor>>,
    /// Counts admissions that may have changed domain state (see [`Runtime::state_changes`]).
    changes: watch::Sender<u64>,
    /// The Tokio runtime [`boot`] ran on (a project archival's immediate stop is spawned there).
    handle: tokio::runtime::Handle,
}

struct ReceiveWaitGuard {
    id: u64,
    waiters: Arc<Mutex<BTreeMap<u64, Cancellation>>>,
}
impl Drop for ReceiveWaitGuard {
    fn drop(&mut self) {
        self.waiters
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.id);
    }
}
impl Admitter {
    fn gateway(&self) -> Gateway<'_> {
        Gateway {
            authority: &*self.authority,
            store: &*self.store,
            ids: &*self.ids,
        }
    }

    /// Admits a command. A command that may have committed (`Recorded`, or a lost
    /// acknowledgment) wakes delivery for the reply intent its receipt committed, an applied
    /// `artifacts.delete` starts a collection pass for the blobs it released, an applied
    /// `settings.save_revision` re-applies keep-awake and a `jobs.*` command wakes the scheduler.
    fn command_with(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
        options: WriteOptions,
    ) -> CommandOutcome {
        self.command_with_wakeup(ctx, operation, raw, options, true)
    }

    fn command_with_wakeup(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
        options: WriteOptions,
        wake_resumes: bool,
    ) -> CommandOutcome {
        // A retained receive may wait 55 seconds. Its read-only preparation happens without
        // holding the shutdown fence; only the final transactional decision holds that fence.
        let outcome = if operation == "agents.receive" {
            let mut options = options;
            let token = options
                .cancellation
                .get_or_insert_with(Cancellation::new)
                .clone();
            let wait_guard = {
                let open = self.admission.read().unwrap_or_else(|p| p.into_inner());
                if *open {
                    return closing_reply(
                        raw,
                        self.ids.next_id(devworks_core::ports::IdKind::Correlation),
                    );
                }
                let id = self.next_receive_waiter.fetch_add(1, Ordering::Relaxed);
                self.receive_waiters
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(id, token);
                ReceiveWaitGuard {
                    id,
                    waiters: self.receive_waiters.clone(),
                }
            };
            let prepared = self
                .use_cases
                .prepare_receive(&self.gateway(), ctx, raw, options);
            drop(wait_guard);
            let open = self.admission.read().unwrap_or_else(|p| p.into_inner());
            if *open {
                return closing_reply(
                    raw,
                    self.ids.next_id(devworks_core::ports::IdKind::Correlation),
                );
            }
            let outcome = match prepared {
                Ok(prepared) => self
                    .use_cases
                    .finish_receive(&self.gateway(), ctx, prepared),
                Err(outcome) => outcome,
            };
            drop(open);
            outcome
        } else {
            // Ordinary commands keep the fence through preparation and commit.
            let open = self.admission.read().unwrap_or_else(|p| p.into_inner());
            if *open {
                return closing_reply(
                    raw,
                    self.ids.next_id(devworks_core::ports::IdKind::Correlation),
                );
            }
            let outcome =
                self.use_cases
                    .command_with(&self.gateway(), ctx, operation, raw, options);
            drop(open);
            outcome
        };
        let (committed, released) = match &outcome {
            Ok(CommandReply::Recorded { receipt, .. }) => (
                true,
                matches!(receipt.decision, DomainDecision::Applied { .. }),
            ),
            Ok(CommandReply::CommandStatusUnknown { .. }) => (true, true),
            _ => (false, false),
        };
        // A replayed receipt or a refusal records nothing new in domain state, so it cannot
        // unblock a waiter; counting it would wake the caller that submitted it.
        let changed = match &outcome {
            Ok(CommandReply::Recorded {
                receipt, replayed, ..
            }) => !replayed && !matches!(receipt.decision, DomainDecision::Refused { .. }),
            Ok(CommandReply::CommandStatusUnknown { .. }) => true,
            _ => false,
        };
        if changed {
            self.changes.send_modify(|n| *n = n.wrapping_add(1));
        }
        if committed {
            self.dispatcher.wakeup().wake();
            self.usage.notify();
            if operation.starts_with("router.")
                && let Some(worker) = &self.prices
            {
                worker.notify();
            }
            if let Some(worker) = &self.notifications {
                worker.notify();
            }
            if wake_resumes && let Some(resumes) = self.resumes.get().and_then(Weak::upgrade) {
                resumes.notify();
            }
            if operation == "sessions.end" || operation.starts_with("runs.") {
                self.evals.notify();
            }
            if operation.starts_with("runs.") {
                // A stop to execute, a stop to signal or a new activation to claim.
                self.runs.notify();
            }
            if (operation.starts_with("jobs.") || operation == ARCHIVES_PROJECT)
                && let Some(s) = self.scheduler.get().and_then(Weak::upgrade)
            {
                // A run to start, a cancellation to execute or a changed next occurrence.
                s.notify();
            }
        }
        if released && operation == RELEASES_BLOBS {
            self.collector.trigger.notify_one();
        }
        if committed && operation == RECORDS_DIAGNOSTIC {
            self.diagnostics.committed();
        }
        if committed && operation == ARCHIVES_PROJECT {
            // A recorded archival (new or replayed) or an unknown outcome: while the project is
            // still closed to work, its trees are killed at once and its root stops executed.
            // A crash before this runs leaves the stops journaled: the next boot reaps the
            // earlier boot's trees and run supervision executes them.
            if let Some(project) = archived_project(raw) {
                let runs = self.runs.clone();
                let wakeup = self.dispatcher.wakeup();
                self.handle.spawn(async move {
                    let _ = runs.stop_archived_project(&project).await;
                    wakeup.wake();
                });
            }
        }
        if committed
            && let Some(ix) = &self.indexing
            && (operation.starts_with("indexing.") || operation == "repositories.remove_index")
        {
            if operation == CANCELS_INDEX_RUN
                && let Some(run) = cancelled_run(raw)
            {
                ix.cancelled(&run);
            }
            if operation == "indexing.configure_sources" {
                ix.request_discovery();
            }
            ix.notify();
        }
        if released
            && operation == SAVES_SETTINGS
            && let Some(ix) = &self.indexing
        {
            // A section's base directory may have changed: rescan it.
            ix.request_discovery();
        }
        if released && operation == SAVES_SETTINGS {
            // A read failure leaves the previous request in place; the next save or explicit
            // refresh applies the setting again.
            let ka = &self.keep_awake;
            let _ = devworks_core::ports::storage::read_snapshot(
                &*self.store,
                &Default::default(),
                |s| ka.refresh(s.configuration()),
            );
        }
        outcome
    }
}
impl crate::package_host::broker::GatewayCalls for Admitter {
    fn query(&self, ctx: &InvocationContext, operation: &str, raw: &[u8]) -> QueryOutcome {
        self.use_cases.query_with(
            &self.gateway(),
            ctx,
            operation,
            raw,
            devworks_core::ports::storage::ReadOptions::default(),
        )
    }
    fn command(&self, ctx: &InvocationContext, operation: &str, raw: &[u8]) -> CommandOutcome {
        self.command_with(ctx, operation, raw, WriteOptions::default())
    }
}
impl NativeJobDispatch for Admitter {
    fn command(&self, ctx: &InvocationContext, operation: &str, raw: &[u8]) -> CommandOutcome {
        self.command_with(ctx, operation, raw, WriteOptions::default())
    }
}

impl ResumeAdmission for Admitter {
    fn readmit(
        &self,
        context: &InvocationContext,
        operation: &str,
        frame: &[u8],
        options: WriteOptions,
    ) -> CommandOutcome {
        // Unknown outcomes wait for the periodic sweep, never a self-notification hot loop.
        self.command_with_wakeup(context, operation, frame, options, false)
    }
}

/// Removes the trailing `count` guidance blocks named by the stored stamps. A stored context
/// whose stamps name more blocks than it holds is inconsistent: refuse it, never underflow.
fn drop_stamped_guidance(
    instructions: &mut Vec<devworks_core::providers::context::TextBlock>,
    count: usize,
) -> Result<(), StorageError> {
    let kept = instructions.len().checked_sub(count).ok_or_else(|| {
        StorageError::InvalidRecord("instruction stamps exceed the stored instructions".into())
    })?;
    instructions.truncate(kept);
    Ok(())
}

/// The `SkillRevisionsAdopted` payload of a skill-set change: the selected skills with exact
/// revisions and digests, the transitions and any unresolved conflicts.
/// The role prefix replaced by `role` when its text differs, keeping the host-authored blocks
/// that follow it (they are not part of the role); `None` when the role is unchanged.
fn with_role(
    instructions: &[devworks_core::providers::context::TextBlock],
    role: Vec<devworks_core::providers::context::TextBlock>,
) -> Option<Vec<devworks_core::providers::context::TextBlock>> {
    let host = |b: &devworks_core::providers::context::TextBlock| {
        b.text == crate::agent_turns::TOOL_WINDOW_NOTICE
    };
    let current: Vec<&str> = instructions
        .iter()
        .filter(|b| !host(b))
        .map(|b| b.text.as_str())
        .collect();
    if current == role.iter().map(|b| b.text.as_str()).collect::<Vec<_>>() {
        return None;
    }
    Some(
        role.into_iter()
            .chain(instructions.iter().filter(|b| host(b)).cloned())
            .collect(),
    )
}

fn skill_adoption_payload(
    before: Option<&devworks_core::providers::instructions::SkillStamp>,
    after: Option<&devworks_core::providers::instructions::SkillStamp>,
) -> serde_json::Value {
    let names = |s: Option<&devworks_core::providers::instructions::SkillStamp>| {
        s.map(|s| {
            s.skills
                .iter()
                .map(|e| (e.name.clone(), (e.instruction_id.clone(), e.revision)))
                .collect::<std::collections::BTreeMap<_, _>>()
        })
        .unwrap_or_default()
    };
    let (old, new) = (names(before), names(after));
    let transitions: Vec<String> = old
        .keys()
        .chain(new.keys())
        .filter(|n| old.get(*n) != new.get(*n))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    serde_json::json!({
        "digest": after.map(|s| s.digest.clone()),
        "previousDigest": before.map(|s| s.digest.clone()),
        "skills": after.map(|s| s.skills.iter().map(|e| serde_json::json!({
            "name": e.name, "instructionId": e.instruction_id.as_str(), "rootId": e.root_id,
            "revision": e.revision, "contentDigest": e.content_digest,
        })).collect::<Vec<_>>()).unwrap_or_default(),
        "transitions": transitions,
        "conflicts": after.map(|s| s.conflicts.clone()).unwrap_or_default(),
    })
}

#[cfg(test)]
mod role_adoption_tests {
    use super::{drop_stamped_guidance, with_role};
    use devworks_core::providers::context::TextBlock;

    fn block(text: &str) -> TextBlock {
        TextBlock {
            text: text.into(),
            cache: None,
        }
    }

    #[test]
    fn a_role_change_keeps_the_host_tool_window_notice() {
        let notice = crate::agent_turns::TOOL_WINDOW_NOTICE;
        let current = [block("old role"), block(notice)];
        // An unchanged role is not a change, though the notice follows it.
        assert!(with_role(&current, vec![block("old role")]).is_none());
        let rebound = with_role(&current, vec![block("new role")]).expect("changed");
        let texts: Vec<&str> = rebound.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, ["new role", notice]);
        // A role cleared to nothing keeps the notice alone; one set later leads again.
        let cleared = with_role(&current, Vec::new()).expect("cleared");
        assert_eq!(cleared.len(), 1);
        assert_eq!(cleared[0].text, notice);
        let set = with_role(&cleared, vec![block("role")]).expect("set");
        assert_eq!(set[0].text, "role");
        assert_eq!(set[1].text, notice);
    }

    #[test]
    fn stamps_naming_more_blocks_than_stored_are_refused_not_underflowed() {
        let mut instructions = vec![block("role"), block("guidance")];
        drop_stamped_guidance(&mut instructions, 1).expect("consistent");
        assert_eq!(instructions.len(), 1);
        let err = drop_stamped_guidance(&mut instructions, 2).expect_err("inconsistent");
        assert!(matches!(
            err,
            devworks_core::ports::storage::StorageError::InvalidRecord(_)
        ));
        assert_eq!(instructions.len(), 1);
    }
}
