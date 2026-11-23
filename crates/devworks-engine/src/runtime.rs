//! The engine's one boot entry point: [`boot`] recovers durable state in a fixed order (queue,
//! artifacts, owned processes and resource leases, run roots), applies keep-awake, records what
//! recovery established as a diagnostic, starts every required consumer, artifact collection,
//! resource housekeeping, run supervision and the diagnostics monitor, and only then returns the
//! [`Runtime`] through which commands and queries are admitted. See the crate documentation ("Boot
//! sequence") for the order and why each step sits where it does.
use crate::approvals::{ResumeAdmission, ResumeDispatcher};
use crate::browser::{BrowserComposition, Supervision as BrowserSupervision};
pub use crate::core_services::RunComposition;
use crate::core_services::{
    ApplicationComposition, ArtifactComposition, CorePorts, CoreServices, DiagnosticComposition,
    KnowledgeComposition, PackageServicePorts, WebPortsFactory, owned_operation,
};
use crate::diagnostics::{self as diag, Monitor};
use crate::dispatch::{self, Consumer, Dispatcher, DispatcherConfig, RecoveryReport};
use crate::evals::EvalSupervisor;
use crate::indexing::{IndexSupervisor, IndexingConfig};
use crate::jobs::Scheduler;
use crate::notifications::OsNotificationSupervisor;
use crate::package_host::{HostConfig, LaunchPorts, PackageHost};
use crate::replies::reply_consumer;
use crate::resources::{BootReport as ResourceBootReport, Housekeeper};
use crate::runs::{RunGate, RunSettings, RunSupervisor};
use crate::workers::{Commits, DrainAt, Loop, LoopState, Next, StopAt, Ticker, Workers};
use devworks_core::context::InvocationContext;
use devworks_core::gateway::{CommandOutcome, Gateway, PreparedReceive, QueryOutcome};
use devworks_core::locks::{LockExt, RwLockExt};
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
use devworks_core::services::desktop::ControlEpoch;
use devworks_core::services::diagnostics::DiagnosticService;
#[cfg(any(test, feature = "test-support"))]
use devworks_core::services::diagnostics::{ExportFailure, LogQuery};
use devworks_core::services::evals::EvalService;
use devworks_core::services::jobs::runner::{JobLifecycle, NativeJobDispatch};
use devworks_core::services::jobs::{JobConfig, JobService, NativeCatalog};
use devworks_core::services::notifications::NotificationService;
use devworks_core::services::power::{KeepAwake, KeepAwakeStatus};
use devworks_core::services::process::{ProcessSupervisor, ShutdownReport};
use devworks_core::services::projects::{ProjectService, RepositoryService};
use devworks_core::services::resources::{MachineResource, ResourceService, desktop};
use devworks_core::services::runs::ControlService;
use devworks_core::services::runs::recovery::{BootRecovery, LifecycleHooks};
use devworks_core::services::work::WorkService;
use devworks_core::settings::view::Health;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;
use tokio::sync::watch;

/// The operation whose applied decision may leave blobs pending physical removal.
const RELEASES_BLOBS: &str = "artifacts.delete";
/// How long the container loop waits before retrying a stop that stayed uncertain.
const CONTAINER_RETRY: Duration = Duration::from_secs(5);

/// The operation through which the `power.keepAwake` setting changes.
const SAVES_SETTINGS: &str = "settings.save_revision";
/// The operation that records a reported diagnostic (its commit raises the live-log hint).
const RECORDS_DIAGNOSTIC: &str = "workspace.record_diagnostic";
/// The operation whose commit fences a project and journals its root stops; the engine then
/// kills the project's owned trees at once and executes those stops.
const ARCHIVES_PROJECT: &str = "projects.archive";
/// The operation whose commit cancels an index run (the run it names is signalled at once).
const CANCELS_INDEX_RUN: &str = "indexing.cancel";
/// Registry families whose commits wake a background worker. Each must name a registered
/// family (`commit_wake_families_are_registered` checks it).
const ROUTER_FAMILY: &str = "router";
const RUNS_FAMILY: &str = "runs";
const JOBS_FAMILY: &str = "jobs";
const INDEXING_FAMILY: &str = "indexing";
/// The family the operation registry records for `operation`; an operation the registry does
/// not know belongs to none.
fn family_of(operation: &str) -> Option<&'static str> {
    devworks_core::registry::find(operation).map(|descriptor| descriptor.family)
}
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

/// The package lifecycle as composed: the ports of the one package service (built with the other
/// core services and registered for the `addons.*` use cases) and what its host needs to launch
/// helpers.
pub struct PackageComposition {
    pub service: PackageServicePorts,
    pub launch: LaunchPorts,
    pub config: HostConfig,
    /// Runtime probing (the cache must be the service's runtime probe, `service.runtimes`).
    pub runtimes: Option<crate::package_host::RuntimePorts>,
    /// Signed publisher index sources (fetch and verification), built over the web tools' ports
    /// (none when no web tools are composed) so the downloads share their resolver, pinned
    /// transport and egress policy.
    pub index: Option<IndexPortsFactory>,
}

/// Builds the package index ports from the web tools' network ports.
/// A factory that needs the web tools refuses the composition (`Err` carries the reason) when
/// they are not composed.
pub type IndexPortsFactory = Arc<
    dyn Fn(
            Option<&devworks_core::services::web::WebPorts>,
        ) -> Result<crate::package_host::IndexPorts, String>
        + Send
        + Sync,
>;

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

/// Everything the runtime is composed from. Every part is a core port; the one `store` backs the
/// gateway, the dispatcher, the reply publisher and artifact recovery and collection. The core
/// services (artifacts, resources, run control, diagnostics, knowledge, agents, web, packages,
/// settings, provider connections, router, sessions, history, MCP client) are built from their
/// ports by [`CoreServices::build`] inside [`boot`]; a use case family of one of them in
/// [`Self::use_cases`] is refused as a composition error.
pub struct Composition {
    pub store: Arc<dyn ApplicationStore>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdSource>,
    pub authority: Arc<dyn AuthorityPort>,
    /// The ports of the one artifact service: [`boot`] registers the artifact use cases with it,
    /// publishes receipt replies through it and collects with it, so its readers, stream grants
    /// and deferred staging resolutions are shared by all three.
    pub artifacts: ArtifactComposition,
    /// The native process launcher every owned tree is spawned through.
    pub process_host: Arc<dyn ProcessHost>,
    /// Run root control: [`boot`] builds its service, registers the `runs.*` use cases and gates
    /// every consumer's root-scoped deliveries with it. It also registers the `projects.*` and
    /// repository registration use cases, whose archival stops roots through this same service.
    pub runs: RunComposition,
    /// The OS power facility the keep-awake setting is applied with.
    pub power: Arc<dyn PowerPort>,
    /// The ports of the one diagnostics service: [`boot`] registers the `workspace.diagnostics`,
    /// `.logs` and `.record_diagnostic` use cases with it, records boot recovery through it,
    /// feeds it the boot report, collection passes and consumers, and runs its retention and
    /// monitor passes.
    pub diagnostics: DiagnosticComposition,
    /// The ports of the one knowledge service ([`boot`] registers its use cases and its source
    /// deletion consumer).
    pub knowledge: KnowledgeComposition,
    /// The ports of the settings, provider connection, router, session, history and MCP client
    /// services ([`boot`] builds the ones composed and registers their use cases).
    pub application: ApplicationComposition,
    /// The web tools' network ports, built over the provider connection service's secret
    /// resolver. When present, [`boot`] builds the web service over the artifact and
    /// diagnostics services and registers `web.search` and `web.fetch`; the package host's
    /// brokered HTTP calls use the same ports.
    pub web: Option<WebPortsFactory>,
    /// Owned browser sessions (`browser.*`): when present, [`boot`] builds the one
    /// [`BrowserService`] over the composition's store, artifact service and process owner,
    /// registers its use cases and effect consumer, recovers earlier boots' sessions after the
    /// owned processes, supervises it and closes its sessions at shutdown. When absent, no
    /// browser operation is registered (nothing unqualified is advertised).
    pub browser: Option<BrowserComposition>,
    /// Native Git owner; missing executable/filesystem is not advertised.
    pub git: Option<crate::git::GitComposition>,
    /// The package lifecycle: [`boot`] registers the `addons.*` use cases with its service, starts
    /// the package host after process recovery and stops its helpers cooperatively at shutdown.
    /// `None`: no package host (packages are not offered).
    pub packages: Option<PackageComposition>,
    /// The use cases of no engine-owned family (the queue, ...). One operation of an owned
    /// family is a composition error; a test scripts one through [`Self::test_overrides`].
    pub use_cases: UseCases,
    /// Scripted operations that replace the engine's own handlers. Only tests can fill it: the
    /// constructor exists under the `test-support` feature, which only dev-dependencies enable.
    pub test_overrides: TestOverrides,
    /// Consumers besides the required ones [`boot`] registers itself (package handlers, ...).
    pub consumers: Vec<Consumer>,
    /// Repository indexing (none: the `indexing.*` operations have no use case).
    pub indexing: Option<IndexComposition>,
    pub config: RuntimeConfig,
}

/// Operations a test scripts over the engine's own handlers. Production code can only name the
/// empty value, so a production composition cannot replace an engine-owned operation.
#[derive(Default)]
pub struct TestOverrides(UseCases);
#[cfg(feature = "test-support")]
impl TestOverrides {
    pub fn new(use_cases: UseCases) -> Self {
        Self(use_cases)
    }
}

/// Builds project purge file access over the engine's read fence.
pub type PurgeFilesFactory = Arc<
    dyn Fn(
            Arc<dyn devworks_core::ports::project_purge::ProjectPurgeReadFence>,
        ) -> Arc<dyn devworks_core::ports::project_purge::ProjectPurgeFiles>
        + Send
        + Sync,
>;
#[derive(Clone)]
pub struct PurgeComposition {
    /// Builds the purge file access over the artifact service, which fences purge reads
    /// ([`devworks_core::ports::project_purge::ProjectPurgeReadFence`]).
    pub files: PurgeFilesFactory,
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

#[derive(Clone)]
pub struct WorkspaceArchiveComposition {
    pub storage: Arc<dyn devworks_core::ports::storage::WorkspaceArchiveStorage>,
    pub files: Arc<dyn devworks_core::ports::workspace_archive::WorkspaceArchiveFiles>,
    pub app_build: String,
}
impl std::fmt::Debug for WorkspaceArchiveComposition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceArchiveComposition")
            .finish_non_exhaustive()
    }
}

/// A native startup owner can hold all runtime effects until an already-open store and
/// the composed engine have been admitted. Normal runtime boots proceed immediately.
#[derive(Clone)]
pub struct StartupAdmission(Arc<dyn Fn() -> Result<(), String> + Send + Sync>);

impl StartupAdmission {
    pub fn new(admit: impl Fn() -> Result<(), String> + Send + Sync + 'static) -> Self {
        Self(Arc::new(admit))
    }
}

impl std::fmt::Debug for StartupAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StartupAdmission")
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub startup_admission: Option<StartupAdmission>,
    /// The native desktop adapter. It remains registered when an OS permission is missing so
    /// inspect can report the actual capability state.
    pub desktop: Option<DesktopComposition>,
    pub purge: Option<PurgeComposition>,
    pub workspace_archives: Option<WorkspaceArchiveComposition>,
    /// Process tools are offered only when a host supplies owned executable/root and secret
    /// bindings. Missing qualification leaves the family unregistered.
    pub process_tools: Option<ProcessToolComposition>,
    /// Native outgoing MCP connection owner, installed after the process supervisor exists.
    pub mcp: Option<McpComposition>,
    /// Optional selected container backend. Without a qualified host binding the family is absent.
    pub containers: Option<ContainerComposition>,
    pub schemas: Option<crate::schemas::SchemaComposition>,
    pub work_import_sources: Option<devworks_core::services::imports::ImportSources>,
    /// The qualified native document extractor. An absent port leaves the feature unregistered.
    pub documents: Option<crate::documents::indexing::DocumentComposition>,
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
    /// Already authorized immutable suites supplied by the host. Empty by default: no automatic
    /// grading or transcript scans. Agents cannot activate suites. Automatic grading (EVAL-02) is
    /// not yet active in the app: no host supplies suites until suite versioning and user-approved
    /// activation exist (DW-72); Help says so.
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
    /// Fallback between store passes of the loops that otherwise wake on a commit signal (git
    /// cancellation, help, schema views, convention imports, project purge, container
    /// reconciliation). An idle runtime does one pass per interval, which repairs a missed
    /// signal and covers work that becomes due by time alone (an expired schema pin).
    pub polling_interval: Duration,
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
            workspace_archives: None,
            startup_admission: None,
            process_tools: None,
            mcp: None,
            containers: None,
            price_refresh: None,
            schemas: None,
            work_import_sources: None,
            documents: None,
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
            polling_interval: Duration::from_secs(30),
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
    ticker: Ticker,
    status: watch::Sender<Option<CollectionPass>>,
}
impl Collector {
    fn stop(&self) {
        self.ticker.stop();
    }
}

/// The booted runtime. Commands and queries block the calling (non-UI) thread on the store, as
/// the gateway does; delivery and collection run on the Tokio runtime [`boot`] was awaited on.
/// Dropping it stops delivery and collection (as [`Runtime::shutdown`] does, without waiting).
pub struct Runtime {
    store: Arc<dyn ApplicationStore>,
    keep_working: Arc<crate::keep_working::KeepWorking>,
    artifacts: Arc<ArtifactService>,
    control: Arc<ControlService>,
    knowledge: Arc<devworks_core::services::knowledge::KnowledgeService>,
    agents: Arc<devworks_core::services::agents::AgentService>,
    web: Option<Arc<devworks_core::services::web::WebService>>,
    connections: Option<Arc<devworks_core::services::router::connections::ConnectionService>>,
    use_cases: UseCases,
    /// The narrow handle set agent turns and durable tool dispatch run on.
    turn: Arc<crate::turn_engine::TurnEngine>,
    dispatcher: Dispatcher,
    collector: Arc<Collector>,
    resources: Arc<ResourceService>,
    housekeeper: Arc<Housekeeper>,
    runs: Arc<RunSupervisor>,
    usage: Arc<crate::usage::UsageRuntime>,
    providers: Option<Arc<crate::providers::driver::NativeProviders>>,
    conventions: Arc<devworks_core::services::conventions::ConventionService>,
    instructions: Option<Arc<devworks_core::services::instructions::InstructionService>>,
    #[cfg(any(test, feature = "test-support"))]
    instruction_boot:
        Option<Result<devworks_core::services::instructions::SyncReport, StorageError>>,
    schemas: Option<Arc<devworks_core::services::schemas::SchemaService>>,
    /// The shutdown stage ([`ShutdownStage`] as a byte) for the progress surface; the fence
    /// itself is `admission_closed`.
    stage: std::sync::atomic::AtomicU8,
    archive_shutdown_verified: std::sync::atomic::AtomicBool,
    /// The admission fence: ordinary commands hold it through admission and commit. A retained
    /// receive waits outside it with a registered shutdown token, then holds it for its final
    /// decision. Shutdown closes it exclusively and cancels waiting receives; no command commits
    /// after the run-control quiescence snapshot. The value is `true` once the fence is closed.
    admission_closed: Arc<std::sync::RwLock<bool>>,
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
    workspace_archives:
        Option<Arc<devworks_core::services::workspace_archive::WorkspaceArchiveService>>,

    shutdown_grace: Duration,
    shutdown_bound: Duration,
    report: BootReport,
    /// Index supervision (none when indexing is not composed).
    indexing: Option<Arc<IndexSupervisor>>,
    /// Every background loop boot started, in registration order. Stop, shutdown and drop
    /// iterate it; each loop is awaited only within the shutdown bound (a root stop or a
    /// cancellation one is executing may be blocked on a tree).
    workers: Workers,
    /// Store passes the commit-woken polling loops have started (diagnostics and tests).
    polling_passes: Arc<AtomicU64>,
    /// The loops built on [`Commits`], by name (tests check every polling loop is among them).
    polling_loops: Arc<Mutex<Vec<&'static str>>>,
    /// What each polling loop has seen, with the commit and delivery counts to compare it to.
    #[cfg(any(test, feature = "test-support"))]
    polling_states: PollingStates,
    /// What the usage worker's drain established, for the shutdown report.
    usage_drain: Arc<Mutex<Option<Result<crate::usage::UsageDrain, StorageError>>>>,
}

/// Every engine-owned service, constructed and wired by [`build`] before any recovery step.
/// [`boot`] runs recovery and then starts the background loops over this finished set.
struct Engine {
    store: Arc<dyn ApplicationStore>,
    clock: Arc<dyn Clock>,
    core: CoreServices,
    config: RuntimeConfig,
    use_cases: UseCases,
    usage: Arc<crate::usage::UsageRuntime>,
    dispatcher: Dispatcher,
    workspace_replies: Option<Arc<crate::workspace_reply::WorkspaceReplies>>,
    auto_decisions: Option<Arc<devworks_core::services::router::decisions::DecisionService>>,
    process_tools: Option<Arc<devworks_core::services::process::tools::ProcessToolService>>,
    containers: Option<Arc<devworks_core::services::containers::service::ContainerService>>,
    git: Option<Arc<devworks_core::services::git::GitService>>,
    github: Option<Arc<devworks_core::services::github::GitHubService>>,
    github_triage: Option<Arc<crate::github_triage::TriageWorker>>,
    browser: Option<Arc<BrowserService>>,
    desktop: Option<Arc<devworks_core::services::desktop::DesktopService>>,
    watchers: Option<Arc<crate::watch::WatchSupervisor>>,
    prices: Option<Arc<crate::prices::PriceSupervisor>>,
    price_service: Arc<devworks_core::services::prices::PriceService>,
    purge_worker: Option<Arc<devworks_core::services::projects::purge::PurgeWorker>>,
    workspace_archives:
        Option<Arc<devworks_core::services::workspace_archive::WorkspaceArchiveService>>,

    conventions: Arc<devworks_core::services::conventions::ConventionService>,
    schemas: Option<Arc<devworks_core::services::schemas::SchemaService>>,
    help: Option<Arc<devworks_core::services::help::HelpService>>,
    embedding_probes: Option<Arc<crate::conventions::probes::ProbeRuntime>>,
    index_supervisor: Option<Arc<IndexSupervisor>>,
    jobs: Arc<JobService>,
    instructions: Option<Arc<devworks_core::services::instructions::InstructionService>>,
    approvals: Arc<ApprovalService>,
    providers: Option<Arc<crate::providers::driver::NativeProviders>>,
    housekeeper: Arc<Housekeeper>,
    run_supervisor: Arc<RunSupervisor>,
    keep_awake: Arc<KeepAwake>,
    collector: Arc<Collector>,
    browser_supervision: Arc<BrowserSupervision>,
    monitor: Arc<Monitor>,
    notifications: Option<Arc<OsNotificationSupervisor>>,
    evals: Arc<EvalSupervisor>,
    admission_closed: Arc<std::sync::RwLock<bool>>,
    admitter: Arc<Admitter>,
    scheduler: Arc<Scheduler>,
    resumes: Arc<ResumeDispatcher>,
    /// The package host, abandoned if boot stops before the Runtime owns it.
    package_guard: PackageBootGuard,
}

/// Builds every other engine-owned service from the composition over the finished core
/// services. Nothing here recovers, deletes or starts anything: the only effect is the Git
/// qualification probe, which decides whether the Git service (and everything composed with it)
/// exists. Errors are recorded by [`boot`].
async fn build(composition: Composition, core: CoreServices) -> Result<Engine, BootError> {
    let Composition {
        store,
        clock,
        ids,
        authority,
        artifacts: _,
        process_host,
        runs,
        power,
        diagnostics: _,
        knowledge: _,
        application,
        web: _,
        browser,
        git,
        packages,
        use_cases,
        test_overrides,
        mut consumers,
        indexing,
        config,
    } = composition;
    let CoreServices {
        artifacts,
        resources,
        control,
        diagnostics,
        knowledge,
        agents: _,
        web: _,
        packages: package_service,
        ..
    } = core.clone();
    consumers.push(crate::knowledge::source_deletion_consumer(
        knowledge.clone(),
    ));
    // The engine's handlers first, then the composition's operations (none of an owned family:
    // boot refused those) and a test's scripted operations over them.
    let use_cases = core
        .register(UseCases::new())
        .overlay(use_cases)
        .overlay(test_overrides.0);
    let purge_files = config.purge.as_ref().map(|p| (p.files)(artifacts.clone()));
    let usage = crate::usage::UsageRuntime::with_process_host(
        store.clone(),
        control.clone(),
        use_cases.inference_router(),
        process_host.clone(),
    );
    let use_cases = use_cases.with_usage(Arc::new(
        devworks_core::services::usage::UsageService::new(ids.clone(), clock.clone()),
    ));
    for suite in &config.automatic_eval_suites {
        devworks_core::protocol::Check::check(suite)
            .map_err(|e| BootError::Composition(StorageError::InvalidRecord(e)))?;
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
            .map_err(|e| BootError::Composition(StorageError::InvalidRecord(e.into())))?;
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
        // Committed event intents run on the same host once it exists.
        consumers.push(crate::package_host::intents::consumer(
            store.clone(),
            package_tools.clone(),
        ));
        // Package events: the announcement is fanned out to the handlers that declare it, and
        // each delivery runs its handler as the person who approved that version.
        if let Some(service) = &package_service {
            service.add_availability_consumer(crate::package_host::events::ROUTER);
        }
        consumers.push(crate::package_host::events::router());
        consumers.push(crate::package_host::events::consumer(
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
    // GitHub calls resolve their credential through the provider connection service built with
    // the core set.
    let git = match git {
        Some(mut composition) => {
            if let Some(http) = composition.github_http.take() {
                let Some(connections) = &core.connections else {
                    return Err(BootError::Composition(StorageError::InvalidRecord(
                        "GitHub calls resolve credentials through the provider connection \
                         service"
                            .to_owned(),
                    )));
                };
                composition.config.github = Some(Arc::new(
                    devworks_core::services::git::github::GitHubClient {
                        http,
                        secrets: connections.clone(),
                    },
                ));
            }
            Some(composition)
        }
        None => None,
    };
    let github = git
        .as_ref()
        .and_then(|composition| composition.config.github.clone())
        .map(|client| {
            Arc::new(devworks_core::services::github::GitHubService::new(
                store.clone(),
                client,
                ids.clone(),
                clock.clone(),
            ))
        });
    let git = git.and_then(|composition| {
        let fs = composition.config.filesystem.clone()?;
        let helpers = Arc::new(devworks_core::services::git::helper::SupervisedHelpers {
            supervisor: supervisor.clone(),
            store: store.clone(),
            fs,
        });
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
                .with_root_control(control.clone())
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
    if let Some(service) = &github {
        use_cases = use_cases.with_github(service.clone());
        consumers.push(crate::github::consumer(service.clone()));
    }
    if let Some(b) = &browser {
        use_cases = use_cases.with_browser(b.clone());
        consumers.push(crate::browser::consumer(b.clone()));
    }
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
        WorkService::new(ids.clone(), control.clone())
            .with_staged_plans(artifacts.clone())
            .with_plan_documents(artifacts.clone())
            .with_native_plan_review()
            .with_clock(clock.clone()),
    );
    let mut projects = ProjectService::new(ids.clone(), control.clone()).with_work(work.clone());
    if let Some(p) = &config.purge {
        projects = projects.with_purge(Arc::new(
            devworks_core::services::projects::purge::PurgeCoordinator::new(
                store.clone(),
                ids.clone(),
                purge_files.clone().expect("composed with the purge"),
                p.client.clone(),
            ),
        ));
    }
    let purge_worker = config.purge.as_ref().map(|p| {
        Arc::new(devworks_core::services::projects::purge::PurgeWorker::new(
            store.clone(),
            ids.clone(),
            purge_files.clone().expect("composed with the purge"),
            p.client.clone(),
        ))
    });
    let eval_service = Arc::new(EvalService::new(ids.clone(), clock.clone()));
    let use_cases = use_cases
        .with_evals(eval_service.clone())
        .with_projects(Arc::new(projects))
        .with_work(work.clone())
        .with_repositories(Arc::new(RepositoryService::new(ids.clone())));
    if github.is_some() {
        consumers.push(crate::github::notice_consumer_with_services(
            store.clone(),
            artifacts.clone(),
            work.clone(),
        ));
    }
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
    let use_cases = if let Some(sources) = &config.work_import_sources {
        let service = Arc::new(devworks_core::services::imports::ImportService::new(
            store.clone(),
            sources.clone(),
            artifacts.clone(),
        ));
        consumers.push(crate::effect_consumer::effect_consumer(
            devworks_core::services::imports::EFFECT_CONSUMER,
            service.clone(),
            crate::effect_consumer::PanicReasons {
                execute: "Work import stopped unexpectedly after dispatch",
                observe: "Work import checkpoint observation stopped unexpectedly",
            },
            crate::effect_consumer::STANDARD_RETRY,
            Some(1),
        ));
        use_cases.with_work_imports(service)
    } else {
        use_cases
    };
    let maintenance =
        Arc::new(devworks_core::services::maintenance::MaintenanceService::new(store.clone()));
    consumers.push(crate::effect_consumer::effect_consumer(
        devworks_core::services::maintenance::EFFECT_CONSUMER,
        maintenance.clone(),
        crate::effect_consumer::PanicReasons {
            execute: "Operational retention stopped unexpectedly after dispatch",
            observe: "Operational retention observation stopped unexpectedly",
        },
        crate::effect_consumer::STANDARD_RETRY,
        Some(1),
    ));
    let use_cases = use_cases.with_maintenance(maintenance);
    use devworks_core::services::workspace_archive as archive;
    let workspace_archives = match (&config.workspace_archives, package_service.as_ref()) {
        (Some(ports), Some(packages)) => Some(Arc::new(archive::WorkspaceArchiveService::new(
            store.clone(),
            archive::ArchivePorts {
                storage: ports.storage.clone(),
                files: ports.files.clone(),
                app_build: ports.app_build.clone(),
            },
            artifacts.clone(),
            packages.clone(),
            ids.clone(),
            clock.clone(),
        ))),
        (None, _) => None,
        _ => return Err(BootError::Composition(StorageError::ViewUnavailable)),
    };
    let use_cases = if let Some(service) = &workspace_archives {
        consumers.push(crate::effect_consumer::effect_consumer(
            archive::EFFECT_CONSUMER,
            service.clone(),
            crate::effect_consumer::PanicReasons {
                execute: "Workspace archive copy stopped unexpectedly",
                observe: "Workspace archive observation stopped unexpectedly",
            },
            crate::effect_consumer::STANDARD_RETRY,
            Some(1),
        ));
        use_cases.with_workspace_archives(service.clone())
    } else {
        use_cases
    };
    let schemas = config.schemas.as_ref().map(|ports| {
        Arc::new(
            devworks_core::services::schemas::SchemaService::new(ids.clone(), clock.clone())
                .with_credentials(store.clone(), ports.secrets.clone())
                .with_runtime(store.clone(), ports.reader.clone(), artifacts.clone()),
        )
    });
    let use_cases = if let Some(service) = &schemas {
        consumers.push(crate::schemas::consumer(store.clone(), service.clone()));
        use_cases.with_schemas(service.clone())
    } else {
        use_cases
    };
    use devworks_core::services::documents::{DocumentLibrary, indexing::DocumentIndexer};
    let document_indexer = config.documents.as_ref().map(|ports| {
        Arc::new(
            DocumentIndexer::new(store.clone(), artifacts.clone(), ports.processor.clone())
                .with_embeddings(conventions.indexing()),
        )
    });
    let use_cases = if let Some(indexer) = &document_indexer {
        consumers.push(crate::documents::indexing::consumer(indexer.clone()));
        use_cases.with_document_library(Arc::new(
            DocumentLibrary::new(artifacts.clone(), ids.clone(), clock.clone())
                .with_indexer(indexer.clone()),
        ))
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
            package_service.clone(),
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
            return Err(BootError::Composition(StorageError::ScopeDenied));
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
    let systemone = match (
        use_cases.inference_router(),
        use_cases.connection_service(),
        config.providers.as_ref(),
    ) {
        (Some(router), Some(connections), Some(provider)) => {
            provider.schema_validator.clone().map(|validator| {
                Arc::new(crate::providers::systemone::SystemOne {
                    store: store.clone(),
                    artifacts: artifacts.clone(),
                    decisions: Arc::new(
                        devworks_core::services::router::decisions::DecisionService::new(
                            router.clone(),
                        ),
                    ),
                    router,
                    connections,
                    web: core.web.clone(),
                    authority: authority.clone(),
                    http: provider.http.clone(),
                    validator,
                    usage: usage.clone(),
                    schemas: Default::default(),
                    runtime: tokio::runtime::Handle::current(),
                })
            })
        }
        _ => None,
    };
    let use_cases = if let Some(systemone) = &systemone {
        consumers.push(systemone.consumer());
        use_cases.with_systemone(systemone.decisions.clone())
    } else {
        use_cases
    };
    let dreaming = config
        .providers
        .as_ref()
        .and_then(|provider| provider.schema_validator.clone())
        .zip(use_cases.inference_router());
    let dreaming_worker = Arc::new(crate::dreaming::DeferredSweep::default());
    let use_cases = if dreaming.is_some() {
        devworks_core::services::dreaming::DreamingService::ensure_base(
            store.as_ref(),
            artifacts.clone(),
        )
        .map_err(BootError::Composition)?;
        consumers.push(dreaming_worker.consumer());
        use_cases.with_dreaming(Arc::new(
            devworks_core::services::dreaming::DreamingService::new(ids.clone()),
        ))
    } else {
        use_cases
    };
    let workspace_replies = (config.providers.is_some()
        && application
            .sessions
            .as_ref()
            .is_some_and(|sessions| sessions.coordinator.is_some()))
    .then(|| Arc::new(crate::workspace_reply::WorkspaceReplies::default()));
    if let Some(worker) = &workspace_replies {
        consumers.push(worker.consumer());
    }
    let run_owner = config.dispatcher.owner.clone();
    // Indexing runs its Git helpers through this same process owner.
    let index_supervisor = indexing.map(|ix| {
        let env = Arc::new(devworks_core::services::indexing::IndexEnv {
            store: store.clone(),
            parser: ix.parser,
            source: ix.source,
            helpers: Arc::new(devworks_core::services::git::helper::SupervisedHelpers {
                supervisor: supervisor.clone(),
                store: store.clone(),
                fs: ix.fs,
            }),
            clock: clock.clone(),
            ids: ids.clone(),
            artifacts: Some(artifacts.clone()),
            diagnostics: Some(diagnostics.clone()),
            package_semantics: Default::default(),
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
    let git_helpers_for_triage = index_supervisor.as_ref().map(|ix| {
        (
            ix.env().helpers.clone(),
            ix.env().settings.git_program.clone(),
        )
    });
    let github_triage = github
        .as_ref()
        .zip(config.providers.as_ref())
        .map(|(github, _)| {
            crate::github_triage::TriageWorker::new(
                github.clone(),
                work.clone(),
                git_helpers_for_triage,
            )
        });
    if let Some(worker) = &github_triage {
        consumers.push(worker.consumer());
    }
    if let (Some(gh), Some(index)) = (&github, &index_supervisor) {
        consumers.push(crate::github::index_consumer(index.env().clone()));
        if github_triage.is_some() {
            consumers.push(crate::github_verify::Verifier::consumer(
                gh.clone(),
                store.clone(),
                index.env().helpers.clone(),
                control.clone(),
                index.env().settings.git_program.clone(),
            ));
        }
    }
    // No entry path bypasses root controls: every consumer's root-scoped work passes the
    // run-control gate (and its own port, if it has one).
    for c in &mut consumers {
        c.eligibility = Some(Arc::new(RunGate::new(
            control.clone(),
            c.eligibility.take(),
        )));
    }
    let mut replies = reply_consumer(
        Arc::new(ReceiptReplies::new(artifacts.clone())),
        store.clone(),
    );
    replies.max_in_flight = config.reply_max_in_flight;
    consumers.insert(0, replies);
    diagnostics.set_consumers(consumers.iter().map(|c| c.name.clone()).collect());
    let dispatcher = Dispatcher::new(
        store.clone(),
        clock.clone(),
        consumers,
        config.dispatcher.clone(),
    )
    .map_err(BootError::Composition)?;
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
            .with_approval_control(control.clone()),
    );
    jobs.install_nightly_dreaming(store.as_ref())
        .map_err(BootError::Composition)?;
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
    // Each approvable operation has one owner, registered here.
    let mut owners =
        devworks_core::services::approvals::owner_preview::NativeOwnerPreviews::default()
            .register(Arc::new(
                devworks_core::services::projects::ProjectApprovals,
            ))
            .register(jobs.clone());
    if let Some(git) = &git {
        git.set_approval_control(control.clone());
        owners = owners.register(git.clone());
    }
    if let Some(packages) = &package_service {
        packages.set_approval_control(control.clone());
        owners = owners.register(packages.clone());
    }
    if let Some(desktop) = &desktop {
        owners = owners.register(desktop.clone());
    }
    if let Some(instructions) = &instructions {
        owners = owners.register(instructions.clone());
    }
    if let Some(containers) = &containers {
        owners = owners.register(containers.clone());
    }
    let owners = owners.with_evidence(store.clone(), artifacts.clone());
    let approvals = Arc::new(
        ApprovalService::new(
            ids.clone(),
            clock.clone(),
            Arc::new(devworks_core::authority::HostRoles {
                store: store.clone(),
            }),
            control.clone(),
        )
        .with_catalog(ApprovalCatalog::qualified(&use_cases))
        .with_owners(Arc::new(owners)),
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
                BootError::Composition(StorageError::unavailable(
                    Unavailable::UnsupportedRuntime,
                    "native provider composition requires current ConnectionService",
                ))
            })?;
            let schemas = Arc::new(crate::providers::schemas::RegisteredSchemas::new(
                store.clone(),
                use_cases.clone(),
                package_service.clone(),
                artifacts.clone(),
            ));
            if let Some(systemone) = &systemone {
                let _ = systemone.schemas.set(schemas.clone());
            }
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
            .map_err(|e| BootError::Composition(StorageError::InvalidRecord(e.into())))?;
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
            return Err(BootError::Composition(StorageError::ScopeDenied));
        }
        service.enable_native_qualification();
    }
    if let (Some(indexer), Some(provider), Some(router)) =
        (&document_indexer, &providers, use_cases.inference_router())
    {
        indexer.set_vision(Arc::new(crate::documents::vision::NativeVision::new(
            store.clone(),
            artifacts.clone(),
            router,
            provider,
            tokio::runtime::Handle::current(),
        )));
    }
    let hooks: Arc<dyn LifecycleHooks> = Arc::new(ChainedHooks(vec![
        Arc::new(JobLifecycle(jobs.clone())),
        runs.hooks.clone(),
    ]));
    let housekeeper = Arc::new(Housekeeper::new(
        store.clone(),
        clock.clone(),
        resources.clone(),
        supervisor.clone(),
    ));
    // A root stop never outlasts the application's one shutdown bound.
    let run_supervisor = RunSupervisor::configured(
        store.clone(),
        clock.clone(),
        control.clone(),
        supervisor.clone(),
        runs.launcher.clone(),
        hooks,
        run_owner,
        RunSettings {
            stop_grace: config.shutdown_grace.min(config.shutdown_bound),
            stop_bound: config.shutdown_bound,
            containers: containers.clone(),
            dispatcher: Some(dispatcher.wakeup()),
        },
    );
    let keep_awake = Arc::new(KeepAwake::new(power));
    let (status, _) = watch::channel(None);
    let collector = Arc::new(Collector {
        ticker: Ticker::new(),
        status,
    });
    let browser_supervision = Arc::new(BrowserSupervision::default());
    let monitor = Arc::new(Monitor::default());
    let notifications = config
        .native_notifications
        .clone()
        .map(|port| OsNotificationSupervisor::new(store.clone(), ids.clone(), clock.clone(), port));
    let evals = EvalSupervisor::new(
        store.clone(),
        eval_service,
        config.automatic_eval_suites.clone(),
    );
    let admission_closed = Arc::new(std::sync::RwLock::new(false));
    let admitter = Arc::new(Admitter {
        store: store.clone(),
        ids: ids.clone(),
        authority: authority.clone(),
        use_cases: use_cases.clone(),
        admission_closed: admission_closed.clone(),
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
    if let Some(((validator, router), providers)) = dreaming.zip(providers.as_ref()) {
        dreaming_worker.install(crate::dreaming::NativeDreaming::new(
            store.clone(),
            artifacts.clone(),
            router,
            providers,
            validator,
            &admitter,
            index_supervisor.as_ref().map(|ix| ix.env().source.clone()),
        ));
    }
    let scheduler = Scheduler::with_stop(
        store.clone(),
        jobs.clone(),
        admitter.clone(),
        runs.inference.clone(),
        supervisor.clone(),
        config.shutdown_grace.min(config.shutdown_bound),
        config.shutdown_bound,
    );
    let _ = admitter.scheduler.set(Arc::downgrade(&scheduler));
    let settled: Arc<dyn Fn(&Id) + Send + Sync> = {
        let (store, artifacts) = (store.clone(), artifacts.clone());
        Arc::new(move |approval| {
            crate::tool_turn::publish_held_update(store.as_ref(), &artifacts, approval);
        })
    };
    let resumes =
        ResumeDispatcher::with_settled(store.clone(), approvals.clone(), admitter.clone(), settled);
    let _ = admitter.resumes.set(Arc::downgrade(&resumes));
    // Built last: from here every early return abandons the host (breaks the service<->host
    // cycle that would keep the store and its workspace lock alive, and kills any started
    // helper) until the completed Runtime owns it.
    let mut package_guard = PackageBootGuard(None);
    if let Some(p) = packages {
        let host = PackageHost::new(
            store.clone(),
            package_service
                .clone()
                .expect("built with the package composition"),
            supervisor,
            ids.clone(),
            p.launch,
            p.config,
            p.runtimes,
            core.package_index
                .as_ref()
                .map(|i| crate::package_host::IndexPorts {
                    transport: i.transport.clone(),
                    verifier: i.verifier.clone(),
                }),
        );
        host.service().set_activity(host.clone());
        let _ = package_tools.set(Arc::downgrade(&host));
        // Brokered application calls of packages run through this command path, as the
        // principal that invoked the package.
        let gateway: Arc<dyn crate::package_host::broker::GatewayCalls> = admitter.clone();
        // Brokered HTTP calls use the web tools' resolver, transport and secret resolver.
        if let Some(web) = &core.web_ports {
            host.attach_network(crate::package_host::http::NetworkPorts {
                resolver: web.resolver.clone(),
                transport: web.transport.clone(),
                secrets: web.secrets.clone(),
            });
        }
        host.attach_broker(crate::package_host::broker::BrokerPorts {
            gateway: Arc::downgrade(&gateway),
            artifacts: artifacts.clone(),
        });
        // Brokered inference runs as routed text inference of the invoking agent's session.
        if let (Some(provider), Some(router)) = (&providers, use_cases.inference_router()) {
            host.attach_inference(crate::package_host::inference::InferencePorts {
                providers: Arc::downgrade(provider),
                router,
                runtime: tokio::runtime::Handle::current(),
            });
        }
        package_guard.0 = Some(host);
    }
    Ok(Engine {
        store,
        clock,
        core,
        config,
        use_cases,
        usage,
        dispatcher,
        workspace_replies,
        auto_decisions: systemone.as_ref().map(|worker| worker.decisions.clone()),
        process_tools,
        containers,
        git,
        github,
        github_triage,
        browser,
        desktop,
        watchers,
        prices,
        price_service,
        purge_worker,
        workspace_archives,
        conventions,
        schemas,
        help,
        embedding_probes,
        index_supervisor,
        jobs,
        instructions,
        approvals,
        providers,
        housekeeper,
        run_supervisor,
        keep_awake,
        collector,
        browser_supervision,
        monitor,
        notifications,
        evals,
        admission_closed,
        admitter,
        scheduler,
        resumes,
        package_guard,
    })
}

/// Boots the runtime (see the crate documentation, "Boot sequence"):
///
/// 1. builds every engine-owned service ([`build`]), including the dispatcher composed with the
///    required receipt reply consumer and the caller's consumers (nothing is claimed yet);
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
    let workspace_coordinator = composition
        .application
        .sessions
        .as_ref()
        .and_then(|sessions| sessions.coordinator.clone());
    // The one construction of the core services; everything below receives the finished set.
    let core = CoreServices::build(CorePorts {
        store: &composition.store,
        clock: &composition.clock,
        ids: &composition.ids,
        artifacts: &composition.artifacts,
        diagnostics: &composition.diagnostics,
        runs: &composition.runs,
        knowledge: &composition.knowledge,
        application: &composition.application,
        web: composition.web.as_ref(),
        packages: composition.packages.as_ref(),
    });
    // Every error that stops boot passes through this, so each leaves a failed boot diagnostic.
    // The diagnostics service is built with the core set, so no step runs without one; a store
    // that cannot take the record is ignored by `record_boot_failure`.
    let diagnostics = core.diagnostics.clone();
    let failed = |e: BootError| {
        diag::record_boot_failure(&diagnostics, &e);
        e
    };
    if let Some(reason) = core.refusals.first() {
        return Err(failed(BootError::Composition(StorageError::InvalidRecord(
            reason.clone(),
        ))));
    }
    if let Some((family, operation)) = owned_operation(&composition.use_cases) {
        return Err(failed(BootError::Composition(StorageError::InvalidRecord(
            format!(
                "{operation} belongs to the {family} use cases, which the engine builds and \
                 registers; a composition must not register any of their operations"
            ),
        ))));
    }
    // Every engine-owned service exists before the first recovery step below.
    let Engine {
        store,
        clock,
        core,
        config,
        use_cases,
        usage,
        dispatcher,
        workspace_replies,
        auto_decisions,
        process_tools,
        containers,
        git,
        github,
        github_triage,
        browser,
        desktop,
        watchers,
        prices,
        price_service,
        purge_worker,
        workspace_archives,
        conventions,
        schemas,
        help,
        embedding_probes,
        index_supervisor,
        jobs,
        instructions,
        approvals,
        providers,
        housekeeper,
        run_supervisor,
        keep_awake,
        collector,
        browser_supervision,
        monitor,
        notifications,
        evals,
        admission_closed,
        admitter,
        scheduler,
        resumes,
        mut package_guard,
    } = build(composition, core).await.map_err(failed)?;
    let (artifacts, resources, control, diagnostics, knowledge, agents, web, connections) = (
        core.artifacts.clone(),
        core.resources.clone(),
        core.control.clone(),
        core.diagnostics.clone(),
        core.knowledge.clone(),
        core.agents.clone(),
        core.web.clone(),
        core.connections.clone(),
    );
    if let Some(admission) = config.startup_admission.clone() {
        tokio::task::spawn_blocking(move || (admission.0)())
            .await
            .unwrap_or_else(|_| Err("Native startup admission failed.".into()))
            .map_err(|error| failed(BootError::Composition(StorageError::InvalidRecord(error))))?;
    }
    if let Some(coordinator) = workspace_coordinator {
        let info = store.info();
        let host = devworks_core::context::HostIngress::new(store.as_ref()).context(
            info.host_actor,
            devworks_core::context::PrincipalKind::Host,
            admitter.ids.next_id(devworks_core::ports::IdKind::Command),
            Id::new("workspace-coordinator").expect("fixed ingress"),
            devworks_core::protocol::Counter::new(info.restore_epoch)
                .map_err(|_| failed(BootError::Composition(StorageError::ScopeDenied)))?,
            devworks_core::protocol::Revision::new(1).expect("positive revision"),
        );
        crate::store::write(&store, WriteOptions::recovery(), move |tx| {
            devworks_core::services::agents::bind_workspace_coordinator(tx, &host, &coordinator)
        })
        .await
        .map_err(BootError::Composition)
        .map_err(failed)?;
    }
    if let Some(service) = &schemas {
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
    }
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
    if let Some(service) = workspace_archives.clone() {
        tokio::task::spawn_blocking(move || service.complete_restored_workspace())
            .await
            .unwrap_or_else(|_| Err(panicked("workspace restore completion")))
            .map_err(BootError::Composition)
            .map_err(failed)?;
    }
    let booted_at_ms = clock.now_ms().get();
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
    let recovered_runs = run_supervisor
        .boot()
        .await
        .map_err(BootError::Runs)
        .map_err(failed)?;
    // Packages (after owned processes are recovered, before any command is admitted): remove
    // interrupted publications and unreferenced install directories, then start every enabled,
    // available package's helper against the committed registry.
    let package_host = package_guard.0.clone();
    if let Some(host) = &package_host {
        let booting = host.clone();
        tokio::task::spawn_blocking(move || booting.boot())
            .await
            .unwrap_or_else(|_| Err(panicked("package boot")))
            .map_err(BootError::Packages)
            .map_err(failed)?;
        let reviews = host.service().clone();
        tokio::task::spawn_blocking(move || reviews.sync_pending_reviews())
            .await
            .unwrap_or_else(|_| Err(panicked("package review recovery")))
            .map_err(BootError::Packages)
            .map_err(failed)?;
    }
    if let (Some(host), Some(providers)) = (&package_host, &providers) {
        let _ = providers.package_codecs.set(Arc::downgrade(host));
    }
    if let (Some(host), Some(indexing)) = (&package_host, &index_supervisor) {
        let _ = indexing.env().package_semantics.set(Arc::new(
            crate::package_host::semantic::PackageSemantics(Arc::downgrade(host)),
        ));
    }
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
    crate::store::write(&store, WriteOptions::recovery(), |tx| {
        tx.decisions().recover_os_notification_deliveries()
    })
    .await
    .map_err(BootError::Composition)
    .map_err(failed)?;
    // Applying may start an OS helper (and wait briefly to see it hold), so off the async thread.
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
    // The polling loops wake on store commits, not a timer: each gets its own receiver. A store
    // without a commit signal falls back to admitted commands (and the interval).
    let polling_passes = Arc::new(AtomicU64::new(0));
    let polling_loops = Arc::new(Mutex::new(Vec::new()));
    #[cfg(any(test, feature = "test-support"))]
    let loop_states = Arc::new(Mutex::new(Vec::new()));
    let (stored, delivered) = {
        let stored = match store.commit_signal() {
            Some(signal) => {
                let (tx, rx) = watch::channel(signal.generation());
                signal.subscribe(move |generation| {
                    tx.send_replace(generation);
                });
                rx
            }
            None => admitter.changes.subscribe(),
        };
        (stored, dispatcher.subscribe())
    };
    #[cfg(any(test, feature = "test-support"))]
    let polling_marks = (stored.clone(), delivered.clone());
    let turn_commits = stored.clone();
    let turn = Arc::new(crate::turn_engine::TurnEngine {
        store: store.clone(),
        clock: clock.clone(),
        artifacts: artifacts.clone(),
        providers: providers.clone(),
        inference_router: use_cases.inference_router(),
        auto_decisions,
        mcp: use_cases.mcp_service(),
        packages: package_host.clone(),
        approvals: approvals.clone(),
        runs: run_supervisor.clone(),
        instructions: instructions.clone(),
        indexing: index_supervisor.clone(),
        admitter: admitter.clone(),
        commits: turn_commits,
    });
    if let Some(worker) = &workspace_replies {
        worker.bind(&turn);
    }
    if let Some(worker) = &github_triage {
        worker.bind(&turn);
    }
    let commits = {
        let (passes, fallback) = (polling_passes.clone(), config.polling_interval);
        let registered = polling_loops.clone();
        #[cfg(any(test, feature = "test-support"))]
        let states = loop_states.clone();
        move |name: &'static str| {
            registered.locked().push(name);
            let state = LoopState::new();
            #[cfg(any(test, feature = "test-support"))]
            states.locked().push(state.clone());
            Commits::new(
                stored.clone(),
                delivered.clone(),
                fallback,
                passes.clone(),
                state,
            )
        }
    };
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
    let stop = run_supervisor.clone();
    workers.start(
        StopAt::Work,
        None,
        Arc::new(
            Loop::new("run supervision", |_| run_supervisor.clone().run(every))
                .on_stop(move || stop.stop()),
        ),
    );
    let keep_working = Arc::new(crate::keep_working::KeepWorking::new(
        store.clone(),
        admitter.ids.clone(),
        admitter.clone(),
    ));
    let (project_work, stop) = (keep_working.clone(), keep_working.clone());
    workers.start(
        StopAt::Schedule,
        None,
        Arc::new(
            Loop::new("project keep-working", |_| project_work.clone().run(every))
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
                    *drained.locked() = Some(report);
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
    // Unknown external effects remain held, never retried. Re-observe them while the app is
    // running as well as at boot; an owner can establish a terminal fact without a restart.
    if process_tools.is_some()
        || containers.is_some()
        || workspace_replies.is_some()
        || github.is_some()
    {
        let observing = dispatcher.clone();
        let stopping = workspace_replies.clone();
        workers.start(
            StopAt::Work,
            None,
            Arc::new(
                Loop::new("unknown outcome observation", |mut stopped| async move {
                    while !stopped.is_set() {
                        let _ = observing.observe_unknown().await;
                        stopped.wait(Duration::from_secs(10)).await;
                    }
                })
                .on_stop(move || {
                    if let Some(worker) = &stopping {
                        worker.stop();
                    }
                }),
            ),
        );
    }
    if let Some(worker) = purge_worker {
        workers.start(
            StopAt::Work,
            None,
            Arc::new(Loop::new("project purge", |stopped| {
                let flag = stopped.clone();
                commits("project purge").run(
                    stopped,
                    || true,
                    move || {
                        let (worker, flag) = (worker.clone(), flag.clone());
                        async move {
                            // A pass that settled effects looks for more at once; a held
                            // effect (an owner still in use, which commits nothing when it
                            // frees) is retried when its window ends.
                            match tokio::task::spawn_blocking(move || {
                                worker.step_pass(&mut || flag.is_set())
                            })
                            .await
                            {
                                Ok(Ok(pass)) if pass.settled > 0 && pass.retry_after.is_none() => {
                                    Next::Now
                                }
                                Ok(Ok(pass)) => pass.retry_after.map_or(Next::Idle, Next::Within),
                                _ => Next::Idle,
                            }
                        }
                    },
                )
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
            Loop::new("convention imports", |stopped| {
                let live = imports.clone();
                let after = Arc::new(Mutex::new(None));
                commits("convention imports").run(
                    stopped,
                    move || !live.is_stopped(),
                    move || {
                        let (service, after) = (imports.clone(), after.clone());
                        let collection_wakeup = collection_wakeup.clone();
                        async move {
                            let position = after.locked().clone();
                            if let Ok(Ok(pass)) = tokio::task::spawn_blocking(move || {
                                service.cleanup_terminal_page(position)
                            })
                            .await
                            {
                                if pass.released {
                                    collection_wakeup.ticker.notify();
                                }
                                // A full page continues with the next one at once.
                                let more = pass.next.is_some();
                                *after.locked() = pass.next;
                                return more;
                            }
                            false
                        }
                    },
                )
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
                Loop::new("git", |stopped| {
                    crate::git::run(service.clone(), stopped, commits("git"))
                })
                .on_stop(move || stop.stop()),
            ),
        );
    }
    if let Some(worker) = github_triage {
        let stop = worker.clone();
        workers.start(
            StopAt::Work,
            None,
            Arc::new(
                Loop::new("GitHub issue triage", |stopped| {
                    crate::github_triage::run(
                        worker.clone(),
                        stopped,
                        commits("GitHub issue triage"),
                    )
                })
                .on_stop(move || stop.stop()),
            ),
        );
    }
    if let Some(service) = &github {
        let stop = service.clone();
        let clock = clock.clone();
        workers.start(
            StopAt::Work,
            None,
            Arc::new(
                Loop::new("github", |stopped| {
                    crate::github::run(service.clone(), clock.clone(), stopped, commits("github"))
                })
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
    if let Some(service) = &help {
        let (service, stop) = (service.clone(), service.clone());
        let command = admitter.clone();
        workers.start(
            StopAt::Work,
            None,
            Arc::new(
                Loop::new("help maintenance", |stopped| {
                    let live = service.clone();
                    commits("help maintenance").run(
                        stopped,
                        move || !live.is_stopped(),
                        move || {
                            let current = service.clone();
                            let target = command.clone();
                            async move {
                                // A cleanup pass is bounded: one that removed drafts runs again.
                                matches!(
                                    tokio::task::spawn_blocking(move || {
                                        let removed = current.cleanup();
                                        if let Ok(Some((context, frame))) =
                                            current.maintenance_request()
                                        {
                                            let _ = target.command_with(
                                                &context,
                                                "help.generate_capability_docs",
                                                &frame,
                                                WriteOptions::command(),
                                            );
                                        }
                                        removed
                                    })
                                    .await,
                                    Ok(Ok(removed)) if removed > 0
                                )
                            }
                        },
                    )
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
                Loop::new("schema view cleanup", |stopped| {
                    let live = service.clone();
                    commits("schema view cleanup").run(
                        stopped,
                        move || !live.is_stopped(),
                        move || {
                            let current = service.clone();
                            async move {
                                // Each pass is bounded: one that removed rows runs again;
                                // otherwise the next pin expiry is the next time work is due.
                                match tokio::task::spawn_blocking(move || current.retention_pass())
                                    .await
                                {
                                    Ok(Ok(pass)) if pass.removed > 0 => Next::Now,
                                    Ok(Ok(pass)) => {
                                        pass.next_expiry.map_or(Next::Idle, Next::Within)
                                    }
                                    _ => Next::Idle,
                                }
                            }
                        },
                    )
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
            Arc::new(Loop::new("container reconciliation", |stopped| {
                commits("container reconciliation").run(
                    stopped,
                    || true,
                    move || {
                        let current = service.clone();
                        async move {
                            // An uncertain stop is retried by a later pass: committing the retry
                            // wakes nothing by itself, so ask for one.
                            match tokio::task::spawn_blocking(move || {
                                current.reconcile_revoked_agent_work(Duration::from_secs(30))
                            })
                            .await
                            {
                                Ok(report)
                                    if !report.uncertain.is_empty()
                                        || report.read_error.is_some() =>
                                {
                                    Next::Within(CONTAINER_RETRY)
                                }
                                _ => Next::Idle,
                            }
                        }
                    },
                )
            })),
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
        keep_working,
        artifacts,
        control,
        knowledge,
        agents,
        web,
        connections,
        use_cases,
        turn,
        dispatcher,
        collector,
        resources,
        housekeeper,
        runs: run_supervisor,
        usage,
        providers,
        conventions,
        #[cfg(any(test, feature = "test-support"))]
        instruction_boot: instructions.as_ref().map(|service| service.sync()),
        instructions,
        schemas,
        stage: std::sync::atomic::AtomicU8::new(ShutdownStage::Open as u8),
        archive_shutdown_verified: std::sync::atomic::AtomicBool::new(false),
        admission_closed,
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
        workspace_archives,
        indexing: index_supervisor,
        workers,
        usage_drain,
        polling_passes,
        polling_loops,
        #[cfg(any(test, feature = "test-support"))]
        polling_states: PollingStates {
            states: loop_states,
            marks: polling_marks,
        },
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
    let collector = &*collector;
    collector
        .ticker
        .run(interval, || {
            pass += 1;
            let (svc, st, n) = (artifacts.clone(), store.clone(), pass);
            async move {
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
                    pass: n,
                    blobs,
                    derived,
                }));
            }
        })
        .await;
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

#[cfg(any(test, feature = "test-support"))]
/// The commit-woken loops' progress and the commit and delivery counts it is compared to.
struct PollingStates {
    states: Arc<Mutex<Vec<Arc<LoopState>>>>,
    marks: (watch::Receiver<u64>, watch::Receiver<u64>),
}

impl Runtime {
    /// The handles agent turns run on.
    pub fn turn_engine(&self) -> &crate::turn_engine::TurnEngine {
        &self.turn
    }
    pub fn mcp_service(&self) -> Option<Arc<devworks_core::services::mcp::McpClientService>> {
        self.use_cases.mcp_service()
    }
    /// Store passes started by the loops that wake on a commit signal, with their fallback
    /// interval ([`RuntimeConfig::polling_interval`]); constant while the runtime is idle.
    pub fn polling_passes(&self) -> u64 {
        self.polling_passes.load(Ordering::SeqCst)
    }
    /// Whether every commit-woken loop is waiting and has seen every store commit and finished
    /// delivery so far, so none has a wake or a pass pending. A burst of commits (boot's Help
    /// ingestion) leaves loops with a coalesced pass up to five seconds out; an idle check
    /// starts only after this holds.
    #[cfg(any(test, feature = "test-support"))]
    pub fn polling_settled(&self) -> bool {
        let (stored, delivered) = &self.polling_states.marks;
        let (stored, delivered) = (*stored.borrow(), *delivered.borrow());
        self.polling_states
            .states
            .locked()
            .iter()
            .all(|state| state.caught_up(stored, delivered))
    }
    /// Names of the background loops that wait on store commits instead of a private timer.
    pub fn polling_loops(&self) -> Vec<&'static str> {
        self.polling_loops.locked().clone()
    }
    /// What boot recovery established.
    pub fn report(&self) -> &BootReport {
        &self.report
    }
    pub fn store(&self) -> &Arc<dyn ApplicationStore> {
        &self.store
    }
    pub fn workspace_reply_status(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
    ) -> Result<Option<crate::WorkspaceReplyStatus>, StorageError> {
        crate::workspace_reply::status(self.store.as_ref(), ctx, session_id)
    }

    /// The artifact service the use cases, reply publication and collection share (artifact
    /// streams are opened through it).
    pub fn artifacts(&self) -> &Arc<ArtifactService> {
        &self.artifacts
    }
    pub fn workspace_archives(
        &self,
    ) -> Option<&Arc<devworks_core::services::workspace_archive::WorkspaceArchiveService>> {
        self.workspace_archives.as_ref()
    }
    /// The resource service the `resources.*` use cases and dispatch-time checks share.
    /// The run root control service the engine built and registered for the `runs.*` use cases.
    pub fn control(&self) -> &Arc<ControlService> {
        &self.control
    }
    /// The knowledge service the engine built and registered.
    pub fn knowledge(&self) -> &Arc<devworks_core::services::knowledge::KnowledgeService> {
        &self.knowledge
    }
    /// The agent coordination service the engine built and registered.
    pub fn agents(&self) -> &Arc<devworks_core::services::agents::AgentService> {
        &self.agents
    }
    /// The web tools service (none when no web ports were composed).
    pub fn web(&self) -> Option<&Arc<devworks_core::services::web::WebService>> {
        self.web.as_ref()
    }
    /// The provider connection service the engine built and registered (none when no
    /// connection ports were composed).
    pub fn connections(
        &self,
    ) -> Option<&Arc<devworks_core::services::router::connections::ConnectionService>> {
        self.connections.as_ref()
    }
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
    #[cfg(any(test, feature = "test-support"))]
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
    #[cfg(any(test, feature = "test-support"))]
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
    #[cfg(any(test, feature = "test-support"))]
    pub fn adopt_skills(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        adopted: Option<&devworks_core::services::instructions::EffectiveSkills>,
    ) -> Result<(devworks_core::services::instructions::EffectiveSkills, bool), StorageError> {
        if ctx.session_id() != Some(session_id) {
            return Err(StorageError::ScopeDenied);
        }
        // The same project resolution as every other skill path decides what it is offered.
        let project = self.turn.skill_project(ctx)?;
        let current = self.effective_skills_in(project.as_ref())?;
        if adopted.is_some_and(|a| a.digest == current.digest) {
            return Ok((current, false));
        }
        crate::tool_turn::record_skill_adoption(
            self.store.as_ref(),
            ctx,
            session_id,
            skill_adoption_payload(
                adopted.and_then(|a| a.stamp()).as_ref(),
                current.stamp().as_ref(),
            ),
        )?;
        Ok((current, true))
    }
    /// The skill set a running agent adopts at its next safe step: sources are observed first,
    /// so a saved, approved or externally changed skill is current; unapproved proposals are
    /// never included. The caller records the returned digest and revisions in its session and
    /// starts a new instruction context epoch when the digest differs from what it adopted.
    pub fn effective_skills(
        &self,
    ) -> Result<devworks_core::services::instructions::EffectiveSkills, StorageError> {
        self.effective_skills_in(None)
    }
    /// The skill set an agent working in `project` adopts: the workspace's skills plus that
    /// project's own. No other project's skill is ever offered.
    pub fn effective_skills_in(
        &self,
        project: Option<&devworks_core::protocol::ProjectId>,
    ) -> Result<devworks_core::services::instructions::EffectiveSkills, StorageError> {
        let service = self.instructions.as_ref().ok_or_else(|| {
            StorageError::unavailable(Unavailable::UnsupportedRuntime, "skills are not composed")
        })?;
        service.sync()?;
        devworks_core::ports::storage::read_snapshot(
            self.store.as_ref(),
            &devworks_core::ports::storage::ReadOptions::default(),
            |read| {
                devworks_core::services::instructions::effective_skills_in(
                    read.instructions(),
                    project,
                )
            },
        )
    }
    pub fn providers(&self) -> Option<&Arc<crate::providers::driver::NativeProviders>> {
        self.providers.as_ref()
    }

    /// See [`TurnEngine::produce_and_dispatch_native_turn`].
    pub async fn produce_and_dispatch_native_turn(
        &self,
        invocation: crate::providers::driver::NativeInvocation,
        cancellation: Option<devworks_core::ports::storage::Cancellation>,
    ) -> Result<NativeTurnProgress, StorageError> {
        self.turn
            .produce_and_dispatch_native_turn(invocation, cancellation)
            .await
    }
    /// See [`TurnEngine::dispatch_tool_call`].
    pub fn dispatch_tool_call(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        call: &devworks_core::protocol::provider::ToolCallKey,
    ) -> Result<devworks_core::services::sessions::transcript::BatchOutcome, StorageError> {
        self.turn.dispatch_tool_call(ctx, session_id, call)
    }
    /// See [`TurnEngine::cancel_tool_call_before_dispatch`].
    pub fn cancel_tool_call_before_dispatch(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        call: &devworks_core::protocol::provider::ToolCallKey,
    ) -> Result<devworks_core::services::sessions::transcript::BatchOutcome, StorageError> {
        self.turn
            .cancel_tool_call_before_dispatch(ctx, session_id, call)
    }
    /// See [`TurnEngine::dispatch_pending_tool_calls`].
    pub fn dispatch_pending_tool_calls(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
    ) -> Result<Vec<ToolDispatch>, StorageError> {
        self.turn.dispatch_pending_tool_calls(ctx, session_id)
    }
    /// See [`TurnEngine::dispatch_pending_tool_calls_cancellable`].
    pub fn dispatch_pending_tool_calls_cancellable(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        cancellation: Option<&Cancellation>,
    ) -> Result<Vec<ToolDispatch>, StorageError> {
        self.turn
            .dispatch_pending_tool_calls_cancellable(ctx, session_id, cancellation)
    }
    /// See [`TurnEngine::resolved_tool_turn`].
    pub fn resolved_tool_turn(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        attempt_id: &devworks_core::protocol::AttemptId,
        after_sequence: i64,
    ) -> Result<Vec<crate::ResolvedToolCall>, StorageError> {
        self.turn
            .resolved_tool_turn(ctx, session_id, attempt_id, after_sequence)
    }
    /// See [`TurnEngine::record_tool_operation_update`].
    pub fn record_tool_operation_update(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        operation_id: &devworks_core::protocol::OperationId,
    ) -> Result<Option<devworks_core::protocol::provider::ArtifactRef>, StorageError> {
        self.turn
            .record_tool_operation_update(ctx, session_id, operation_id)
    }
    /// See [`TurnEngine::reconcile_tool_operations`].
    pub fn reconcile_tool_operations(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
    ) -> Result<Vec<ToolOperationUpdate>, StorageError> {
        self.turn.reconcile_tool_operations(ctx, session_id)
    }
    /// See [`TurnEngine::refresh_native_tool_context`].
    pub fn refresh_native_tool_context(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        previous: &devworks_core::protocol::provider::ArtifactRef,
    ) -> Result<devworks_core::protocol::provider::ArtifactRef, StorageError> {
        self.turn
            .refresh_native_tool_context(ctx, session_id, previous)
    }
    /// See [`TurnEngine::recover_native_context_head`].
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
        self.turn.recover_native_context_head(ctx, session_id)
    }
    /// See [`TurnEngine::publish_initial_native_context`].
    pub fn publish_initial_native_context(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        binding: &devworks_core::protocol::provider::NativeBinding,
        context: &devworks_core::providers::context::ProviderContext,
    ) -> Result<devworks_core::protocol::provider::ArtifactRef, StorageError> {
        self.turn
            .publish_initial_native_context(ctx, session_id, binding, context)
    }
    /// See [`TurnEngine::tool_continuation_context`].
    pub fn tool_continuation_context(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        attempt_id: &devworks_core::protocol::AttemptId,
        after_sequence: i64,
        binding: &devworks_core::protocol::provider::NativeBinding,
        previous: &devworks_core::protocol::provider::ArtifactRef,
    ) -> Result<devworks_core::providers::context::ProviderContext, StorageError> {
        self.turn.tool_continuation_context(
            ctx,
            session_id,
            attempt_id,
            after_sequence,
            binding,
            previous,
        )
    }
    /// See [`TurnEngine::change_provider_epoch`].
    pub fn change_provider_epoch(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        previous_context: &devworks_core::protocol::provider::ArtifactRef,
        previous_binding: &devworks_core::protocol::provider::NativeBinding,
        next_binding: &devworks_core::protocol::provider::NativeBinding,
    ) -> Result<devworks_core::protocol::provider::ArtifactRef, StorageError> {
        self.turn.change_provider_epoch(
            ctx,
            session_id,
            previous_context,
            previous_binding,
            next_binding,
        )
    }
    /// See [`TurnEngine::adopt_instruction_epoch`].
    pub fn adopt_instruction_epoch(
        &self,
        ctx: &InvocationContext,
        session_id: &Id,
        previous_context: &devworks_core::protocol::provider::ArtifactRef,
        previous_binding: &devworks_core::protocol::provider::NativeBinding,
        targets: &[(Id, Vec<String>)],
    ) -> Result<devworks_core::protocol::provider::ArtifactRef, StorageError> {
        self.turn.adopt_instruction_epoch(
            ctx,
            session_id,
            previous_context,
            previous_binding,
            targets,
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

    /// See [`TurnEngine::effective_instructions`].
    pub fn effective_instructions(
        &self,
        ctx: &InvocationContext,
        repository_id: &Id,
        target_paths: &[String],
    ) -> Result<devworks_core::providers::instructions::InstructionBundle, StorageError> {
        self.turn
            .effective_instructions(ctx, repository_id, target_paths)
    }
    /// See [`TurnEngine::verify_instruction_context`].
    pub fn verify_instruction_context(
        &self,
        ctx: &InvocationContext,
        context: &devworks_core::providers::context::ProviderContext,
        targets: Option<&[(Id, Vec<String>)]>,
    ) -> Result<(), StorageError> {
        self.turn.verify_instruction_context(ctx, context, targets)
    }
    /// See [`TurnEngine::bind_provider_instructions`].
    pub fn bind_provider_instructions(
        &self,
        ctx: &InvocationContext,
        base: devworks_core::providers::context::ProviderContext,
        targets: &[(Id, Vec<String>)],
    ) -> Result<devworks_core::providers::context::ProviderContext, StorageError> {
        self.turn.bind_provider_instructions(ctx, base, targets)
    }
    /// The keep-awake request as last applied (re-checking a held request).
    pub fn keep_awake(&self) -> KeepAwakeStatus {
        self.keep_awake.status()
    }
    /// Keep-awake runtime health for the Settings view (distinct from the configured value).
    pub fn keep_awake_health(&self) -> Health {
        self.keep_awake.health()
    }

    /// The running dispatcher (status hints, wakeups, diagnostics).
    pub fn dispatcher(&self) -> &Dispatcher {
        &self.dispatcher
    }
    /// The latest artifact collection pass (`None` until the boot pass finished).
    pub fn collections(&self) -> watch::Receiver<Option<CollectionPass>> {
        self.collector.status.subscribe()
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
        self.admitter.query_with(ctx, operation, raw, options)
    }

    /// The desktop uses the live use cases when deciding what incoming MCP may discover.
    pub fn has_mcp_tool(&self, tool: &devworks_core::services::mcp_inbound::PublishedTool) -> bool {
        match tool.binding.mode {
            devworks_core::registry::Mode::Query => self.use_cases.has_query(tool.operation.name),
            devworks_core::registry::Mode::Command => {
                self.use_cases.has_command(tool.operation.name)
            }
        }
    }

    pub fn command(&self, ctx: &InvocationContext, operation: &str, raw: &[u8]) -> CommandOutcome {
        if operation == devworks_core::services::workspace_archive::RESTORE_OPERATION
            && let Some(archives) = &self.workspace_archives
            && let Ok(Some(reply)) = archives.replay_prior_restore(ctx, raw)
        {
            return Ok(reply);
        }
        self.command_with(ctx, operation, raw, WriteOptions::command())
    }

    pub fn systemone_completion(
        &self,
        context: &InvocationContext,
        id: &devworks_core::protocol::OperationId,
    ) -> Result<
        Option<devworks_core::protocol::results::OperationCompletion>,
        Box<devworks_core::protocol::DomainFault>,
    > {
        crate::providers::systemone::completion(&*self.store, context, id)
    }

    /// Dedicated trusted native-human interaction. The ordinary command/tool path cannot
    /// mint this private exact-content witness, even for a context whose kind is Human.
    pub fn record_human_decision(&self, ctx: &InvocationContext, frame: &[u8]) -> CommandOutcome {
        self.record_human_decision_received(ctx, frame, self.desktop_control_epoch())
    }

    /// [`Self::record_human_decision`] for a click the UI received at `received`: refused as a
    /// trusted decision unless desktop control stayed quiet from then until now.
    pub fn record_human_decision_received(
        &self,
        ctx: &InvocationContext,
        frame: &[u8],
        received: ControlEpoch,
    ) -> CommandOutcome {
        let outcome = self.decide_as_human(ctx, frame, received);
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

    fn decide_as_human(
        &self,
        ctx: &InvocationContext,
        frame: &[u8],
        received: ControlEpoch,
    ) -> CommandOutcome {
        const OPERATION: &str = "approvals.record_human_decision";
        if !self.control_quiet_since(received) {
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
        self.confirm_project_purge_received(ctx, frame, self.desktop_control_epoch())
    }

    /// [`Self::confirm_project_purge`] for a click the UI received at `received`: refused as a
    /// trusted confirmation unless desktop control stayed quiet from then until now.
    pub fn confirm_project_purge_received(
        &self,
        ctx: &InvocationContext,
        frame: &[u8],
        received: ControlEpoch,
    ) -> CommandOutcome {
        const OPERATION: &str = "projects.purge";
        if !self.control_quiet_since(received) {
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

    /// Dedicated native input for whole-workspace archive operations. Receipt replay remains
    /// available through normal command ingress without recreating trusted consent.
    pub fn workspace_maintenance_received(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        frame: &[u8],
        received: ControlEpoch,
    ) -> CommandOutcome {
        if self.control_quiet_since(received)
            && let Ok(decoded) = devworks_core::admission::decode(
                devworks_core::admission::Selection {
                    operation,
                    mode: devworks_core::registry::Mode::Command,
                },
                frame,
            )
            && let Some(bound) = devworks_core::context::HostIngress::new(&*self.store)
                .workspace_maintenance_context(ctx, &decoded.input)
        {
            return self.command(&bound, operation, frame);
        }
        self.command(ctx, operation, frame)
    }

    /// The desktop control state now: a human-only confirmation captures it when the UI
    /// receives the click ([`ControlEpoch`]).
    pub fn desktop_control_epoch(&self) -> ControlEpoch {
        self.desktop
            .as_ref()
            .map_or(ControlEpoch::QUIET, |desktop| desktop.control_epoch())
    }

    /// Whether a human-only confirmation the UI received at `received` may act now: desktop
    /// automation could not have produced it (nobody held control then or now, and none was
    /// granted in between). Trusted native confirmation ingress stays closed otherwise.
    pub fn control_quiet_since(&self, received: ControlEpoch) -> bool {
        received.quiet_through(&self.desktop_control_epoch())
    }

    pub fn keep_working_status(
        &self,
        project: &devworks_core::protocol::ProjectId,
    ) -> Option<crate::KeepWorkingStatus> {
        self.keep_working.status(project)
    }

    /// Only an explicit native click can arm automatic allocation. All resulting commands
    /// still run under the human's current authority through the ordinary gateway.
    pub fn set_keep_working(
        &self,
        context: &InvocationContext,
        project: &devworks_core::protocol::ProjectId,
        enabled: bool,
        received: ControlEpoch,
    ) -> Result<crate::KeepWorkingStatus, StorageError> {
        if !self.control_quiet_since(received) {
            return Err(StorageError::ScopeDenied);
        }
        self.keep_working.set(context, project, enabled)
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
    #[cfg(any(test, feature = "test-support"))]
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

    /// [`Self::command_with`] for a caller that can await. An `agents.receive` with nothing to
    /// read yet waits as a suspended task: it holds no thread, ends at once on cancellation and
    /// looks again when a command commits or every five seconds. Any other operation runs as
    /// [`Self::command_with`] does. Callers that cannot await keep [`Self::command_with`], which
    /// parks the calling thread for the wait.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn command_awaiting(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
        options: WriteOptions,
    ) -> CommandOutcome {
        if operation != "agents.receive" {
            return self.command_with(ctx, operation, raw, options);
        }
        let (admitter, call_ctx, bytes) = (self.admitter.clone(), ctx.clone(), raw.to_vec());
        let started = blocking(move || admitter.begin_receive(&call_ctx, &bytes, options)).await;
        match started {
            ReceiveStart::Answered(outcome) => outcome,
            ReceiveStart::Waiting(wait) => self.admitter.clone().finish_receive(wait).await,
        }
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
        let inference_shutdown = self.usage_drain.locked().take().unwrap_or_else(|| {
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
        let admission_closed = self.admission_closed.clone();
        let receive_waiters = self.admitter.receive_waiters.clone();
        let _ = tokio::task::spawn_blocking(move || {
            *admission_closed.write_locked() = true;
            for token in receive_waiters.locked().values() {
                token.cancel();
            }
        })
        .await;
        // No blocking evaluator job may outlive the runtime: drain its local write fence.
        self.workers.drain(DrainAt::AfterFence, deadline).await;
        self.enter(ShutdownStage::Quiescing);
        let quiesced = self
            .runs
            .quiesce(self.shutdown_grace.min(left()))
            .await
            .is_ok();
        self.enter(ShutdownStage::StoppingWork);
        self.stop();
        // Every boot-started task (delivery, collection, housekeeping, the monitor pass in
        // progress, run supervision, the schedule loop), the deliveries and the scheduled runs in flight are awaited only within what
        // is left of the one bound (CONTROL-09); past it each is abandoned, and the next boot
        // recovers what it left: a unit of work it already admitted commits or rolls back
        // alone, a claim's lease expires into recovery, an interrupted operation is classified.
        // Supervision admits no further root stop once stopping; one it is executing ends
        // within its own bound, and the owned-tree stop below covers its trees.
        let loops_stopped = self.workers.join(deadline).await.is_empty();
        // In-flight deliveries.
        self.workers.drain(DrainAt::AfterJoin, deadline).await;
        // Browser sessions close through the process owner (their records settle and profiles
        // are removed) before the remaining owned trees are stopped. Like every wait above, it
        // starts only after the run-control hold committed and is abandoned at the one deadline:
        // a close that does not return in time leaves its trees to the owned-tree stop below.
        let mut owners_closed = true;
        if let Some(b) = self.browser.clone() {
            let closing = tokio::task::spawn_blocking(move || b.shutdown());
            owners_closed &= matches!(
                dispatch::first(tokio::time::sleep_until(deadline), closing).await,
                dispatch::Either::Right(Ok(_))
            );
        }
        // Package helpers get their cooperative shutdown (cancel in-flight invocations,
        // host.shutdown) after the hold above and within what is left of the same bound: the
        // host stops every helper by that deadline, and its thread is not awaited past it; the
        // process owner below stops whatever remains.
        if let Some(host) = self.packages.clone() {
            let bound = left();
            let stopping = tokio::task::spawn_blocking(move || host.shutdown(bound));
            owners_closed &= matches!(
                dispatch::first(tokio::time::sleep_until(deadline), stopping).await,
                dispatch::Either::Right(Ok(_))
            );
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
        let mut inference_closed = false;
        match inference_shutdown {
            Ok(drain) => {
                inference_closed = drain.unclosed.is_empty() && drain.pending_preparations == 0;
                stopped.unclosed_inference_attempts = drain.unclosed;
                if !stopped.unclosed_inference_attempts.is_empty() || drain.pending_preparations > 0
                {
                    let service = self.diagnostics.clone();
                    let ids = stopped.unclosed_inference_attempts.clone();
                    let _=tokio::task::spawn_blocking(move||{
                        use devworks_core::services::diagnostics::LogEntry;
                        use devworks_core::protocol::records::{Severity,DiagnosticOutcome};
                        let mut entry=LogEntry::new("router.usage","unclosed-inference",Severity::Warning,
                            format!("{} inference transports were not verified closed before shutdown ({} \
                                preparations still settling); custody and reservations remain held.",ids.len(),drain.pending_preparations));
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
        let runs_closed = self.runs.finish().await.is_ok();
        self.keep_awake.apply(false);
        let resources_closed = stopped
            .reconciliation
            .as_ref()
            .is_ok_and(|r| r.uncertain == 0 && r.unreachable == 0);
        let local_processes_closed = stopped.stopped.iter().all(|(_, result)| {
            matches!(
                result,
                Ok(devworks_core::services::process::StopOutcome::Reaped { .. })
            )
        });
        let containers_closed = stopped
            .containers
            .as_ref()
            .is_none_or(|r| r.read_error.is_none() && r.uncertain.is_empty());
        self.archive_shutdown_verified.store(
            self.workspace_archives.is_some()
                && quiesced
                && loops_stopped
                && owners_closed
                && inference_closed
                && runs_closed
                && resources_closed
                && local_processes_closed
                && containers_closed
                && !left().is_zero(),
            std::sync::atomic::Ordering::Release,
        );
        self.enter(ShutdownStage::Done);
        stopped
    }

    /// The native archive boundary requires a completed, verified shutdown before arming.
    pub fn archive_shutdown_verified(&self) -> bool {
        self.archive_shutdown_verified
            .load(std::sync::atomic::Ordering::Acquire)
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
pub(crate) struct Admitter {
    store: Arc<dyn ApplicationStore>,
    ids: Arc<dyn IdSource>,
    authority: Arc<dyn AuthorityPort>,
    use_cases: UseCases,
    admission_closed: Arc<std::sync::RwLock<bool>>,
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
    pub(crate) changes: watch::Sender<u64>,
    /// The Tokio runtime [`boot`] ran on (a project archival's immediate stop is spawned there).
    handle: tokio::runtime::Handle,
}

/// One pending tool call after the synchronous part of dispatch.
pub(crate) enum PendingCall {
    Done(ToolDispatch),
    /// A waiting `agents.receive`, parked without a thread.
    Parked(Box<crate::tool_turn::ParkedReceive>),
}

/// Runs store work on a blocking thread and passes a panic on to the caller.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(work).await {
        Ok(value) => value,
        Err(error) => std::panic::resume_unwind(error.into_panic()),
    }
}

/// Drives every future to completion on the calling task and returns the outputs in order.
pub(crate) async fn join_all<'a, T>(
    mut futures: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>>,
) -> Vec<T> {
    let mut outputs: Vec<Option<T>> = futures.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (future, output) in futures.iter_mut().zip(outputs.iter_mut()) {
            if output.is_none() {
                match future.as_mut().poll(cx) {
                    std::task::Poll::Ready(value) => *output = Some(value),
                    std::task::Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    outputs.into_iter().flatten().collect()
}

/// Where a receive stands after its first, non-blocking look.
#[allow(clippy::large_enum_variant)] // the reply is returned at once; boxing it buys nothing
pub(crate) enum ReceiveStart {
    Answered(CommandOutcome),
    Waiting(Box<ReceiveWait>),
}

/// A receive waiting for a message. It holds no thread; dropping it abandons the wait.
pub(crate) struct ReceiveWait {
    ctx: InvocationContext,
    raw: Vec<u8>,
    prepared: PreparedReceive,
    /// Keeps the wait registered for shutdown to cancel.
    guard: ReceiveWaitGuard,
}

struct ReceiveWaitGuard {
    id: u64,
    waiters: Arc<Mutex<BTreeMap<u64, Cancellation>>>,
}
impl Drop for ReceiveWaitGuard {
    fn drop(&mut self) {
        self.waiters.locked().remove(&self.id);
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

    /// Reads through the gateway; a person's or the host's repository discovery answers from a
    /// fresh scan (see [`Runtime::query_with`]).
    pub(crate) fn query_with(
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
            let closed = self.admission_closed.read_locked();
            if *closed {
                return outcome;
            }
            let env = ix.env().clone();
            let cancelled = || {
                options
                    .cancellation
                    .as_ref()
                    .is_some_and(devworks_core::ports::storage::Cancellation::is_cancelled)
            };
            // A scan that fails leaves the recorded registrations to answer; what it could not
            // do is recorded on the indexing channel.
            let scan =
                devworks_core::services::indexing::discover::scan(&env, ctx, None, &cancelled);
            devworks_core::services::indexing::discover::log_scan(&env, &scan);
            drop(closed);
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

    /// Admits a command. A command that may have committed (`Recorded`, or a lost
    /// acknowledgment) wakes delivery for the reply intent its receipt committed, an applied
    /// `artifacts.delete` starts a collection pass for the blobs it released, an applied
    /// `settings.save_revision` re-applies keep-awake and a `jobs.*` command wakes the scheduler.
    pub(crate) fn command_with(
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
        self.fenced_command(ctx, operation, raw, options, wake_resumes, &|decide| {
            Some(decide())
        })
        .expect("an open fence always decides")
    }

    /// [`Self::command_with_wakeup`] whose final transactional decision runs inside the caller's
    /// `fence`; `None` when the fence refused it (nothing was decided). A retained receive's
    /// preparation runs outside the fence.
    fn fenced_command(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
        options: WriteOptions,
        wake_resumes: bool,
        fence: crate::package_host::broker::Fence<'_>,
    ) -> Option<CommandOutcome> {
        // A retained receive may wait 55 seconds. Its read-only preparation happens without
        // holding the shutdown fence or the caller's; only the final transactional decision
        // holds them.
        let outcome = if operation == "agents.receive" {
            let mut options = options;
            let token = options
                .cancellation
                .get_or_insert_with(Cancellation::new)
                .clone();
            let wait_guard = {
                let closed = self.admission_closed.read_locked();
                if *closed {
                    return Some(closing_reply(
                        raw,
                        self.ids.next_id(devworks_core::ports::IdKind::Correlation),
                    ));
                }
                let id = self.next_receive_waiter.fetch_add(1, Ordering::Relaxed);
                self.receive_waiters.locked().insert(id, token);
                ReceiveWaitGuard {
                    id,
                    waiters: self.receive_waiters.clone(),
                }
            };
            let prepared = self
                .use_cases
                .prepare_receive(&self.gateway(), ctx, raw, options);
            drop(wait_guard);
            let mut prepared = Some(prepared);
            fence(&mut || {
                let closed = self.admission_closed.read_locked();
                if *closed {
                    return closing_reply(
                        raw,
                        self.ids.next_id(devworks_core::ports::IdKind::Correlation),
                    );
                }
                match prepared.take().expect("a fence decides once") {
                    Ok(prepared) => self
                        .use_cases
                        .finish_receive(&self.gateway(), ctx, prepared),
                    Err(outcome) => outcome,
                }
            })?
        } else {
            // Ordinary commands keep the fences through preparation and commit.
            fence(&mut || {
                let closed = self.admission_closed.read_locked();
                if *closed {
                    return closing_reply(
                        raw,
                        self.ids.next_id(devworks_core::ports::IdKind::Correlation),
                    );
                }
                self.use_cases
                    .command_with(&self.gateway(), ctx, operation, raw, options.clone())
            })?
        };
        self.after_command(operation, raw, &outcome, wake_resumes);
        Some(outcome)
    }

    /// Phase one of an `agents.receive` that holds no thread: admits it, looks once and either
    /// answers or returns the wait for the caller to await ([`Self::finish_receive`]).
    pub(crate) fn begin_receive(
        &self,
        ctx: &InvocationContext,
        raw: &[u8],
        mut options: WriteOptions,
    ) -> ReceiveStart {
        let token = options
            .cancellation
            .get_or_insert_with(Cancellation::new)
            .clone();
        let guard = {
            let closed = self.admission_closed.read_locked();
            if *closed {
                return ReceiveStart::Answered(closing_reply(
                    raw,
                    self.ids.next_id(devworks_core::ports::IdKind::Correlation),
                ));
            }
            let id = self.next_receive_waiter.fetch_add(1, Ordering::Relaxed);
            self.receive_waiters.locked().insert(id, token);
            ReceiveWaitGuard {
                id,
                waiters: self.receive_waiters.clone(),
            }
        };
        match self
            .use_cases
            .start_receive(&self.gateway(), ctx, raw, options)
        {
            Ok(prepared) if prepared.ticket().is_some() => {
                ReceiveStart::Waiting(Box::new(ReceiveWait {
                    ctx: ctx.clone(),
                    raw: raw.to_vec(),
                    prepared,
                    guard,
                }))
            }
            prepared => {
                drop(guard);
                ReceiveStart::Answered(self.decide_receive(ctx, raw, prepared))
            }
        }
    }

    /// Phase two: waits without holding a thread for a signal, a cancellation or the recheck
    /// interval, looks again, and decides once nothing is left to wait for.
    pub(crate) async fn finish_receive(self: Arc<Self>, wait: Box<ReceiveWait>) -> CommandOutcome {
        let ReceiveWait {
            ctx,
            raw,
            mut prepared,
            guard,
        } = *wait;
        while let Some(ticket) = prepared.ticket() {
            let (wake, generation, pause) =
                (ticket.wake.clone(), ticket.generation, ticket.pause());
            // A timeout is the recheck interval or the deadline; either ends in a new look.
            let _ = tokio::time::timeout(pause, wake.changed_after(generation)).await;
            // Each look reads the store, so it runs on a blocking thread.
            let (admitter, look_ctx) = (self.clone(), ctx.clone());
            let (back, result) = blocking(move || {
                let result = admitter.use_cases.recheck_receive(
                    &admitter.gateway(),
                    &look_ctx,
                    &mut prepared,
                );
                (prepared, result)
            })
            .await;
            if let Err(outcome) = result {
                return outcome;
            }
            prepared = back;
        }
        drop(guard);
        blocking(move || self.decide_receive(&ctx, &raw, Ok(prepared))).await
    }

    /// The short decision of a receive under the admission fence, then its commit wakeups.
    fn decide_receive(
        &self,
        ctx: &InvocationContext,
        raw: &[u8],
        prepared: Result<PreparedReceive, CommandOutcome>,
    ) -> CommandOutcome {
        let closed = self.admission_closed.read_locked();
        let outcome = if *closed {
            closing_reply(
                raw,
                self.ids.next_id(devworks_core::ports::IdKind::Correlation),
            )
        } else {
            match prepared {
                Ok(prepared) => self
                    .use_cases
                    .finish_receive(&self.gateway(), ctx, prepared),
                Err(outcome) => outcome,
            }
        };
        drop(closed);
        self.after_command("agents.receive", raw, &outcome, true);
        outcome
    }

    /// What a command's outcome sets in motion once it is known: the state-change count, the
    /// workers a commit wakes and the follow-up work some operations start.
    fn after_command(
        &self,
        operation: &str,
        raw: &[u8],
        outcome: &CommandOutcome,
        wake_resumes: bool,
    ) {
        let (committed, released) = match outcome {
            Ok(CommandReply::Recorded { receipt, .. }) => (
                true,
                matches!(receipt.decision, DomainDecision::Applied { .. }),
            ),
            Ok(CommandReply::CommandStatusUnknown { .. }) => (true, true),
            _ => (false, false),
        };
        // A replayed receipt or a refusal records nothing new in domain state, so it cannot
        // unblock a waiter; counting it would wake the caller that submitted it.
        let changed = match outcome {
            Ok(CommandReply::Recorded {
                receipt, replayed, ..
            }) => !replayed && !matches!(receipt.decision, DomainDecision::Refused { .. }),
            Ok(CommandReply::CommandStatusUnknown { .. }) => true,
            _ => false,
        };
        if changed {
            self.changes.send_modify(|n| *n = n.wrapping_add(1));
        }
        let family = family_of(operation);
        if committed {
            self.dispatcher.wakeup().wake();
            self.usage.notify();
            if family == Some(ROUTER_FAMILY)
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
            if operation == "sessions.end" || family == Some(RUNS_FAMILY) {
                self.evals.notify();
            }
            if family == Some(RUNS_FAMILY) {
                // A stop to execute, a stop to signal or a new activation to claim.
                self.runs.notify();
            }
            if (family == Some(JOBS_FAMILY) || operation == ARCHIVES_PROJECT)
                && let Some(s) = self.scheduler.get().and_then(Weak::upgrade)
            {
                // A run to start, a cancellation to execute or a changed next occurrence.
                s.notify();
            }
        }
        if released && operation == RELEASES_BLOBS {
            self.collector.ticker.notify();
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
            && (family == Some(INDEXING_FAMILY) || operation == "repositories.remove_index")
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
    fn command(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
        cancellation: Cancellation,
        fence: crate::package_host::broker::Fence<'_>,
    ) -> Option<CommandOutcome> {
        let options = WriteOptions {
            cancellation: Some(cancellation),
            ..WriteOptions::command()
        };
        self.fenced_command(ctx, operation, raw, options, true, fence)
    }
}
impl NativeJobDispatch for Admitter {
    fn command(&self, ctx: &InvocationContext, operation: &str, raw: &[u8]) -> CommandOutcome {
        self.command_with(ctx, operation, raw, WriteOptions::command())
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
pub(crate) fn drop_stamped_guidance(
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
pub(crate) fn with_role(
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

pub(crate) fn skill_adoption_payload(
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
mod commit_wake_tests {
    use super::{INDEXING_FAMILY, JOBS_FAMILY, ROUTER_FAMILY, RUNS_FAMILY, family_of};
    use devworks_core::registry;

    #[test]
    fn commit_wake_families_are_registered() {
        for family in [ROUTER_FAMILY, RUNS_FAMILY, JOBS_FAMILY, INDEXING_FAMILY] {
            assert!(registry::families().contains(&family), "{family}");
        }
    }

    /// The family check replaced a name-prefix check, so every operation's name must start with
    /// its family; otherwise the two would select different operations.
    #[test]
    fn every_operation_name_starts_with_its_family() {
        for descriptor in registry::all() {
            let prefix = descriptor.name.split('.').next();
            assert_eq!(prefix, Some(descriptor.family), "{}", descriptor.name);
            assert_eq!(family_of(descriptor.name), Some(descriptor.family));
        }
    }
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
