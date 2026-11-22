//! The desktop composition root (A3): opens the one SQLite store, composes every core service
//! over it and starts the engine **only** through [`devworks_engine::boot`], so queue, artifact,
//! owned-process and lease recovery and every required consumer run before the first UI command
//! is admitted. It owns the engine thread (a Tokio runtime with timers on which delivery,
//! collection and housekeeping run) and its bounded shutdown on window close (CONTROL-08: there
//! is no tray or background mode).
//!
//! The shell never sees the store: it receives [`DesktopBackend`], which admits queries and
//! commands through [`Runtime::query`] / [`Runtime::command`] with the host-bound human
//! [`InvocationContext`], exactly as an agent tool reaches the same use cases (ARCH-02).
use crate::backend::{Backend, Waker};
use crate::platform::{HostIds, StoreAuthority, SystemClock, user_display_name};
use devworks_adapters::browser::NativeBrowserPlatform;
use devworks_adapters::diagnostics::DiagnosticExports;
use devworks_adapters::fs::NativeFilesystem;
use devworks_adapters::fs::artifacts::NativeArtifactFiles;
use devworks_adapters::fs::sources::{HostArtifactSources, IssueRefusal};
use devworks_adapters::fs::workspace_archive::NativeWorkspaceArchives;
use devworks_adapters::index::NativeSourceTree;
use devworks_adapters::index::treesitter::TreeSitterParser;
use devworks_adapters::mcp::NativeMcpClients;
use devworks_adapters::packages::{
    BundleIndexVerifier, BundleInspector, JsonSchemaValidator, NativeIndexTransport,
    NativePackageFiles, host_target_triple,
};
use devworks_adapters::paths::FsPathProbe;
use devworks_adapters::power::NativePower;
use devworks_adapters::process::NativeProcessHost;
use devworks_adapters::storage::sqlite::{SqliteConfig, SqliteStore};
use devworks_adapters::web::native_ports;
use devworks_core::authority::HostRoles;
use devworks_core::authority::destructive::ProtectedRoots;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::gateway::{CommandOutcome, QueryOutcome};
use devworks_core::locks::LockExt;
use devworks_core::ports::diagnostics::StorageHealthProbe;
use devworks_core::ports::http::HttpTransport;
use devworks_core::ports::packages::{HostPlatform, RuntimeCache};
use devworks_core::ports::router::ConnectionDirectory;
use devworks_core::ports::secrets::{Secret, SecretStore, SwitchableSecrets};
use devworks_core::ports::storage::{
    ApplicationStore, ReadOptions, StorageError, WriteOptions, read_snapshot, run_write,
};
use devworks_core::ports::{Clock, IdKind, IdSource};
use devworks_core::protocol::{ActorId, Counter, Id, Revision};
use devworks_core::services::UseCases;
use devworks_core::services::desktop::ControlEpoch;
use devworks_core::services::indexing::IndexSettings;
use devworks_core::services::process::ShutdownReport;
use devworks_core::services::projects::managed_files::ManagedFileCustody;
use devworks_core::services::router::connections::{ConnectionService, IntakeError};
use devworks_core::services::web::{WebService, read_policy};
use devworks_core::settings::view::Health;
use devworks_engine::package_host::{HostConfig, IndexPorts, LaunchPorts, RuntimePorts};
use devworks_engine::runtime::IndexPortsFactory;
use devworks_engine::{
    ApplicationComposition, ArtifactComposition, BootError, BootReport, Composition,
    ConnectionPorts, DiagnosticComposition, IndexComposition, IndexingConfig, KnowledgeComposition,
    PackageComposition, PackageServicePorts, RouterPorts, RunComposition, Runtime, RuntimeConfig,
    SessionPorts, SettingsPorts, ShutdownStage, boot,
};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::{oneshot, watch};

/// The host principal that attributes store bookkeeping.
pub const HOST_ACTOR: &str = "devworks-host";
/// The local user, registered as the human principal at every boot.
pub const USER_ACTOR: &str = "local-user";
/// The durable rootless agent for native Home conversations.
const WORKSPACE_COORDINATOR: &str = "devworks-workspace-coordinator";
/// Retry-key namespace of the desktop UI surface.
pub const UI_INGRESS: &str = "desktop-ui";
/// Lifetime of an upload ticket minted for an attachment (it is spent at once).
use devworks_core::ports::update::{InstallApproval, ReleaseSource, RunningApplication};
use devworks_core::protocol::records::UpdateChannel;
use devworks_core::services::update::{APPROVE, InstallOutcome, UpdateService, UpdateStatus};

pub const UPLOAD_TICKET_MS: i64 = 10 * 60 * 1000;

#[cfg(target_os = "macos")]
fn macos_major_version() -> u32 {
    std::process::Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|version| version.split('.').next()?.parse().ok())
        .unwrap_or(0)
}

#[cfg(not(target_os = "macos"))]
fn macos_major_version() -> u32 {
    0
}

/// Builds the web tools' ports over the secret resolver the host composed.
pub type WebPortsFactory = devworks_engine::WebPortsFactory;

/// What the composition root is configured with.
#[derive(Clone)]
pub struct DesktopConfig {
    pub data_root: PathBuf,
    /// The user's display name for attribution.
    pub user_name: String,
    /// The application build recorded with applied migrations.
    pub app_build: String,
    /// A candidate opened by the one-shot, verified installed-app update handshake.
    pub app_update: Option<InstallApproval>,
    pub startup_admission: Option<devworks_engine::runtime::StartupAdmission>,
    #[cfg(feature = "test-support")]
    pub update_fixture: Option<(Arc<dyn ReleaseSource>, RunningApplication)>,
    /// Shared bound of the owned-process stop at shutdown (AD20 default 30 s).
    pub shutdown_bound: Duration,
    /// An explicitly configured Chromium-family browser executable (`DEVWORKS_BROWSER`), offered
    /// as the `configured` browser asset beside the discovered Chrome, Chromium and Edge.
    pub browser_path: Option<PathBuf>,
    /// Explicit host grants for process environment credential handles. An unregistered
    /// credentialRef is refused; the generic provider connection handles are not exported.
    pub process_credentials: Vec<devworks_core::services::process::tools::RegisteredCredential>,
    pub process_working_roots: Vec<devworks_core::services::process::tools::RegisteredWorkingRoot>,
    /// Explicit grants for container workload environment and registry credentials. Process
    /// grants do not imply access to these handles.
    pub container_credentials: Vec<devworks_core::services::process::tools::RegisteredCredential>,
    /// Native computer-control adapter; tests may inject a scripted desktop with no OS input.
    pub desktop_platform: Option<Arc<dyn devworks_core::ports::desktop::DesktopPlatform>>,
    /// Replaces the connection directory the router reads (tests script connection states).
    /// `None` (production) routes through the provider connection service's durable rows.
    pub connections: Option<Arc<dyn ConnectionDirectory>>,
    /// The secret facility. `None` (production) is the OS facility (macOS Keychain, Windows
    /// Credential Manager, Linux Secret Service), switchable to session-only memory by the user.
    pub secrets: Option<Arc<dyn SecretStore>>,
    /// Outbound HTTP for sign-in, discovery and connection tests. `None` (production) is
    /// native HTTPS; tests use a fake or none, never the real network.
    pub http: Option<Arc<dyn HttpTransport>>,
    /// Inference-only streaming transport and explicit native model/space qualification.
    /// None composes the real native HTTP client; tests inject controlled streaming ports.
    pub providers: Option<devworks_engine::providers::driver::ProviderComposition>,
    /// The interval of the connection service's background tick (device-code polling, flow
    /// expiry, periodic connection checks, secret reconciliation); `None` disables it.
    pub connection_tick: Option<Duration>,
    /// The web tools' network ports (the system resolver, the rustls transport and the search
    /// providers), built over the secret resolver the host composes: the provider connection
    /// service over [`Self::secrets`], so `web.searchCredential` resolves from the same OS
    /// secret store as the provider credentials. Production is [`native_ports`]; tests keep
    /// every request on this machine.
    pub web: WebPortsFactory,
    /// Index supervision bounds (run concurrency, discovery and housekeeping intervals).
    pub indexing: IndexingConfig,
    /// The user's shared skill folder read in place (`~/.agents/skills` by default); `None`
    /// reads no shared skills. Harness skills always live under `<data_root>/skills`.
    pub shared_skills: Option<PathBuf>,
    /// The search path the container CLIs (`docker`, `podman`, `container`) resolve against.
    /// `None` (production) is the process `PATH`. Tests pass an empty path so opening Settings
    /// never runs a host container CLI; process tools keep resolving from `PATH` either way.
    pub container_search_path: Option<devworks_core::services::process::executables::SearchPath>,
}
impl std::fmt::Debug for DesktopConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopConfig")
            .field("data_root", &self.data_root)
            .field("user_name", &self.user_name)
            .field("app_build", &self.app_build)
            .field("shutdown_bound", &self.shutdown_bound)
            .finish_non_exhaustive()
    }
}
impl DesktopConfig {
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
            user_name: user_display_name(),
            app_build: concat!("devworks-desktop/", env!("CARGO_PKG_VERSION")).to_owned(),
            app_update: None,
            startup_admission: None,
            #[cfg(feature = "test-support")]
            update_fixture: None,
            shutdown_bound: Duration::from_secs(30),
            browser_path: std::env::var_os("DEVWORKS_BROWSER")
                .filter(|p| !p.is_empty())
                .map(PathBuf::from),
            process_credentials: Vec::new(),
            process_working_roots: Vec::new(),
            container_credentials: Vec::new(),
            desktop_platform: None,
            connections: None,
            secrets: None,
            http: None,
            providers: None,
            connection_tick: Some(CONNECTION_TICK),
            web: Arc::new(native_ports),
            indexing: IndexingConfig::default(),
            shared_skills: std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|home| PathBuf::from(home).join(".agents").join("skills")),
            container_search_path: None,
        }
    }
}

/// Container backends select only their own CLI among the configured executables.
const CONTAINER_EXECUTABLES: &[&str] = &["docker", "podman", "container"];

use devworks_adapters::fs::skills::PROJECT_SKILLS_DIR as PROJECT_SKILLS;

/// What every command launch is checked against: the application data folder and the skill
/// folders, so an agent's command can never write a skill change that sync would observe.
pub(crate) fn protected_roots(config: &DesktopConfig) -> ProtectedRoots {
    ProtectedRoots::new(
        Vec::new(),
        Vec::new(),
        Some(config.data_root.display().to_string()),
    )
    .with_skill_roots(
        [
            config.data_root.join("skills"),
            config.data_root.join(PROJECT_SKILLS),
        ]
        .into_iter()
        .chain(config.shared_skills.clone())
        .map(|p| p.display().to_string()),
    )
}

fn container_bindings(
    config: &DesktopConfig,
    executables: Arc<devworks_core::services::process::executables::PathExecutables>,
) -> Result<devworks_core::services::process::tools::ToolBindings, &'static str> {
    devworks_core::services::process::tools::ToolBindings::new(
        std::iter::empty::<devworks_core::services::process::tools::RegisteredExecutable>(),
        std::iter::empty(),
    )?
    .with_path_executables(executables, Some(CONTAINER_EXECUTABLES))
    .with_credentials(config.container_credentials.clone())
}

/// How often the connection service's background step runs (device codes are polled at their
/// own interval, never faster).
pub const CONNECTION_TICK: Duration = Duration::from_secs(1);

/// Why the runtime did not start. The shell stays usable for local inspection and shows the
/// affected screens offline with this reason (PLAT-02/03).
#[derive(Clone, Debug)]
pub enum StartError {
    /// The store could not be opened (another DevWorks owns the data root, an incompatible
    /// schema, a filesystem failure ...).
    Store(StorageError),
    Boot(BootError),
    /// The local user could not be registered as the human principal.
    Principal(StorageError),
    /// The engine thread or its Tokio runtime could not start.
    Thread(String),
}
impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "the workspace store could not be opened: {e:?}"),
            Self::Boot(e) => write!(f, "runtime recovery did not complete: {e:?}"),
            Self::Principal(e) => write!(f, "the local user could not be registered: {e:?}"),
            Self::Thread(e) => write!(f, "the engine could not start: {e}"),
        }
    }
}

/// The booted host: the runtime handle and the host-bound user identity.
pub struct Host {
    runtime: Arc<Runtime>,
    navigation: Arc<devworks_core::services::navigation::NavigationContextService>,
    async_runtime: tokio::runtime::Handle,
    ctx: InvocationContext,
    sources: Arc<HostArtifactSources>,
    purge_client: Arc<crate::project_purge::NativePurgeClient>,
    data_root: PathBuf,
    /// The folders a repository folder deletion never removes (application data, skills).
    deletion_folders: devworks_core::authority::deletion::HostFolders,
    ids: Arc<HostIds>,
    waker: Arc<Mutex<Option<Waker>>>,
    /// The same authority the runtime admits with (stream handles are re-authorized by it).
    authority: Arc<StoreAuthority>,
    /// The provider connection service: its host intake (API keys, pasted sign-in codes) is
    /// reached only from the user's native UI; everything else goes through the gateway.
    connections: Arc<ConnectionService>,
    /// The OS secret facility, when the production one is used (switchable to session-only).
    switchable: Option<Arc<SwitchableSecrets>>,
    /// The web tools, stopped first at shutdown so no fetch holds a request thread.
    web: Arc<WebService>,
    /// The native outgoing MCP client the runtime installed at boot.
    mcp_clients: Arc<std::sync::OnceLock<Arc<NativeMcpClients>>>,
    restart: Mutex<Option<Restart>>,
    updates: Arc<UpdateService>,
}
enum Restart {
    Restore(Id),
    Update(Box<InstallApproval>),
    CancelledUpdate(Box<InstallApproval>),
}

impl Host {
    pub fn updates(&self) -> &Arc<UpdateService> {
        &self.updates
    }

    pub fn runtime(&self) -> &Arc<Runtime> {
        &self.runtime
    }
    /// The native outgoing MCP client, once the runtime installed it.
    pub fn mcp_clients(&self) -> Option<Arc<NativeMcpClients>> {
        self.mcp_clients.get().cloned()
    }
    pub fn report(&self) -> &BootReport {
        self.runtime.report()
    }
    /// The human invocation context every UI request is admitted with.
    pub fn user(&self) -> &InvocationContext {
        &self.ctx
    }
}

/// Composes the services over `store` and boots (A3). Must be awaited on a Tokio runtime with
/// timers; the tasks boot starts run there.
async fn compose(
    config: &DesktopConfig,
    sqlite: Arc<SqliteStore>,
    archive_files: Arc<NativeWorkspaceArchives>,
    clock: Arc<dyn Clock>,
    ids: Arc<HostIds>,
) -> Result<Host, StartError> {
    let store: Arc<dyn ApplicationStore> = sqlite.clone();
    let id_source: Arc<dyn IdSource> = ids.clone();
    let custody = Arc::new(ManagedFileCustody::new(store.clone()));
    let sources = Arc::new(
        HostArtifactSources::new(clock.clone(), id_source.clone())
            .with_project_fence(custody.clone()),
    );
    let artifact_files = Arc::new(NativeArtifactFiles::new(
        config.data_root.clone(),
        config.data_root.join("cache"),
    ));
    let artifact_ports = ArtifactComposition {
        project_previews: Some(custody),
        ..ArtifactComposition::new(
            artifact_files.clone(),
            sources.clone(),
            Arc::new(NativeFilesystem::new()),
        )
    };
    // Provider connections (DW-18): the OS secret facility (or the configured one), native
    // HTTPS (or the configured transport) and the OS random generator.
    let (secrets, switchable): (Arc<dyn SecretStore>, Option<Arc<SwitchableSecrets>>) =
        match &config.secrets {
            Some(s) => (s.clone(), None),
            None => {
                let s = Arc::new(SwitchableSecrets::new(devworks_adapters::secrets::native()));
                (s.clone(), Some(s))
            }
        };
    let http: Arc<dyn HttpTransport> = config
        .http
        .clone()
        .unwrap_or_else(|| Arc::new(devworks_adapters::http::NativeHttp::new()));
    // Unified Knowledge retrieval is lexical until an embedding route exists (vector search is
    // never reported as run). The engine builds every core service from these ports.
    let application = ApplicationComposition {
        settings: Some(SettingsPorts {
            roles: Arc::new(HostRoles {
                store: store.clone(),
            }),
            paths: Arc::new(FsPathProbe),
        }),
        connections: Some(ConnectionPorts {
            http: http.clone(),
            secrets: secrets.clone(),
            entropy: Arc::new(devworks_adapters::OsEntropy),
            endpoints: None,
        }),
        // The router resolves tiers, named models, effort and availability over the provider
        // connections' durable rows (the same unit of work as each route), unless a test
        // scripts the directory.
        router: Some(RouterPorts {
            directory: config.connections.clone(),
        }),
        sessions: Some(SessionPorts {
            coordinator: Some(ActorId(
                Id::new(WORKSPACE_COORDINATOR).expect("fixed actor ID"),
            )),
        }),
        history: true,
        mcp: true,
    };
    let navigation =
        Arc::new(devworks_core::services::navigation::NavigationContextService::default());
    let release = || -> (Arc<dyn ReleaseSource>, RunningApplication) {
        let mut source = crate::app_updates::source(&config.data_root);
        if config.http.is_some() {
            source = source.with_feed(Arc::new(devworks_core::ports::http::NoNetwork));
        }
        (Arc::new(source), crate::app_updates::running())
    };
    #[cfg(feature = "test-support")]
    let (release_source, running_app) = config.update_fixture.clone().unwrap_or_else(release);
    #[cfg(not(feature = "test-support"))]
    let (release_source, running_app) = release();
    let updates = Arc::new(UpdateService::new(
        store.clone(),
        release_source,
        running_app,
        clock.clone(),
    ));
    if store.info().boot_id.is_some() {
        updates.bind_profile().map_err(StartError::Store)?;
    }
    let mut use_cases = UseCases::new().with_queue().with_updates(updates.clone());
    use_cases.register("ui.current_screen", Some(navigation.clone()), None);
    let authority = Arc::new(StoreAuthority {
        store: store.clone(),
        clock: clock.clone(),
    });
    // Diagnostics read the store's own health probes; exports are staged as the user's uploads.
    let diagnostic_ports = DiagnosticComposition {
        storage: Some(sqlite.clone() as Arc<dyn StorageHealthProbe>),
        export_staging: Some(Arc::new(
            DiagnosticExports::new(
                config.data_root.join("diagnostics").join("exports"),
                sources.clone(),
            )
            .with_store(store.clone()),
        )),
        secrets: Vec::new(),
    };
    // Managed executable packages (ADDON-01..04, EXT-01..07): verified bundles install into
    // content-addressed directories under the data root; helpers launch through the process
    // owner. No optional runtime is assumed present until a runtime probe is qualified.
    let runtimes = Arc::new(RuntimeCache::new());
    let package_ports = PackageServicePorts {
        inspector: Arc::new(BundleInspector),
        files: Arc::new(NativePackageFiles::new(config.data_root.join("packages"))),
        runtimes: runtimes.clone(),
        platform: HostPlatform {
            target_triple: host_target_triple(),
            host_version: env!("CARGO_PKG_VERSION").into(),
            protocol_major: 1,
            protocol_minor: 0,
        },
        validator: Arc::new(JsonSchemaValidator),
        quiesce_wait: None,
        availability_consumers: Vec::new(),
        log_ids: None,
    };
    // The shared web tools' ports are built by the engine over the one connection service (the
    // search credential resolves from the same OS secret store, one secret-store abstraction)
    // and handed to the package index and the package host's brokered HTTP calls. Package index
    // and bundle downloads use the same resolver, pinned-address transport and egress policy as
    // web.fetch, read fresh for each download (refused on a read failure).
    let package_index: IndexPortsFactory = {
        let store = store.clone();
        Arc::new(move |web| {
            let web = web.ok_or_else(|| {
                "the package index needs the web tools' ports (a web composition)".to_owned()
            })?;
            let store = store.clone();
            Ok(IndexPorts {
                transport: Arc::new(NativeIndexTransport::new(
                    web.resolver.clone(),
                    web.transport.clone(),
                    Arc::new(move || {
                        read_snapshot(&*store, &ReadOptions::default(), |s| {
                            Ok(read_policy(s.configuration()))
                        })
                        .ok()
                        .and_then(Result::ok)
                        .is_some_and(|p| p.allow_private)
                    }),
                )),
                verifier: Arc::new(BundleIndexVerifier),
            })
        })
    };
    let purge_client = Arc::new(
        crate::project_purge::NativePurgeClient::new(
            config.data_root.join(crate::memory::FILE),
            sources.clone(),
        )
        .with_store(&*store),
    );
    let mut runtime_config = RuntimeConfig::new(ids.next_id(IdKind::Boot));
    runtime_config.startup_admission = config.startup_admission.clone();
    runtime_config.workspace_archives =
        Some(devworks_engine::runtime::WorkspaceArchiveComposition {
            storage: sqlite.clone(),
            files: archive_files,
            app_build: config.app_build.clone(),
        });
    let import_root = config.data_root.join("work-imports");
    std::fs::create_dir_all(&import_root)
        .map_err(|e| StartError::Thread(format!("could not prepare work import folder: {e}")))?;
    let bindings = devworks_core::services::process::tools::ToolBindings::new(
        std::iter::empty(),
        std::iter::once(
            devworks_core::services::process::tools::RegisteredWorkingRoot {
                id: Id::new("work-import-inbox").expect("static working root ID"),
                project: None,
                path: import_root,
            },
        ),
    )
    .map_err(|reason| StartError::Thread(reason.into()))?;
    let filesystem = Arc::new(NativeFilesystem::new());
    runtime_config.work_import_sources =
        Some(devworks_core::services::imports::ImportSources::new(
            bindings,
            filesystem.clone(),
            filesystem,
        ));
    runtime_config.purge = Some(devworks_engine::runtime::PurgeComposition {
        files: {
            let (data_root, paste_root) = (
                config.data_root.clone(),
                config.data_root.join("staging-ui"),
            );
            Arc::new(move |fence| {
                Arc::new(
                    devworks_adapters::fs::purge::NativeProjectPurgeFiles::new(
                        data_root.clone(),
                        data_root.join("cache"),
                    )
                    .with_paste_root(paste_root.clone())
                    .with_read_fence(fence),
                )
            })
        },
        client: purge_client.clone(),
    });
    runtime_config.shutdown_bound = config.shutdown_bound;
    runtime_config.instructions = Some(devworks_engine::runtime::InstructionComposition {
        source: Arc::new(NativeSourceTree::new()),
        files: Arc::new(
            devworks_adapters::fs::skills::NativeSkillFiles::new(config.data_root.join("skills"))
                .with_projects(config.data_root.join(PROJECT_SKILLS)),
        ),
        roots: devworks_core::services::instructions::SkillRoots {
            shared: config.shared_skills.clone(),
            harness: config.data_root.join("skills"),
        },
    });
    runtime_config.schemas = Some(devworks_engine::schemas::SchemaComposition {
        reader: Arc::new(devworks_adapters::database::NativeMetadataReader),
        secrets: secrets.clone(),
    });
    // The configured executable names (`tools.processExecutables`), each resolved on PATH
    // when a tool first selects it: nothing scans PATH before the first frame.
    let executables = Arc::new(
        devworks_core::services::process::executables::PathExecutables::from_settings(
            store.clone(),
        ),
    );
    // Container CLIs share that resolver unless the configuration names a search path of its own.
    let container_executables = match config.container_search_path.clone() {
        Some(path) => Arc::new(
            devworks_core::services::process::executables::PathExecutables::from_settings_on(
                store.clone(),
                path,
            ),
        ),
        None => executables.clone(),
    };
    let container_bindings = container_bindings(config, container_executables)
        .map_err(|reason| StartError::Thread(reason.into()))?;
    let container_private_files = Arc::new(
        devworks_adapters::containers::NativeRegistryAuth::new(
            config.data_root.join("container-auth"),
        )
        .map_err(|reason| StartError::Thread(reason.into()))?,
    );
    runtime_config.containers = Some(devworks_engine::runtime::ContainerComposition {
        bindings: container_bindings,
        filesystem: Arc::new(NativeFilesystem::new()),
        protected_roots: protected_roots(config),
        secrets: secrets.clone(),
        registry_auth: container_private_files.clone(),
        workload_environment: container_private_files,
        host: devworks_core::services::containers::probe::HostPlatform {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            macos_major: macos_major_version(),
        },
    });
    let mcp_bindings = devworks_core::services::process::tools::ToolBindings::new(
        std::iter::empty(),
        config.process_working_roots.clone(),
    )
    .map(|bindings| {
        bindings
            .with_path_executables(executables, None)
            .with_install_environments(config.data_root.join("install-environments"))
    })
    .and_then(|bindings| bindings.with_credentials(config.process_credentials.clone()))
    .map_err(|reason| StartError::Thread(reason.into()))?;
    runtime_config.process_tools = Some(devworks_engine::runtime::ProcessToolComposition {
        bindings: mcp_bindings.clone(),
        filesystem: Arc::new(NativeFilesystem::new()),
        protected_roots: protected_roots(config),
        secrets: secrets.clone(),
    });
    let mcp_secrets = secrets.clone();
    let mcp_namespace = store.info().store_id.0.as_str().to_owned();
    let mcp_roots = protected_roots(config);
    let mcp_clients: Arc<std::sync::OnceLock<Arc<NativeMcpClients>>> = Arc::default();
    let installed_mcp = mcp_clients.clone();
    runtime_config.mcp = Some(devworks_engine::runtime::McpComposition {
        validator: Arc::new(JsonSchemaValidator),
        factory: Arc::new(move |supervisor, handle| {
            let native = Arc::new(NativeMcpClients::new(
                supervisor,
                mcp_bindings.clone(),
                mcp_secrets.clone(),
                mcp_namespace.clone(),
                Arc::new(NativeFilesystem::new()),
                mcp_roots.clone(),
                Vec::new(),
                handle,
            ));
            let _ = installed_mcp.set(native.clone());
            native
        }),
    });
    runtime_config.providers = Some(
        (match config.providers.clone() {
            Some(providers) => providers,
            None => devworks_engine::providers::driver::ProviderComposition::new(Arc::new(
                devworks_adapters::provider_stream::NativeStreamingHttp::new().map_err(|_| {
                    StartError::Thread(
                        "native inference HTTP client could not be initialized".into(),
                    )
                })?,
            )),
        })
        .with_systemone_validator(Arc::new(JsonSchemaValidator)),
    );
    runtime_config.price_refresh = Some(devworks_engine::prices::PriceComposition::new(Arc::new(
        devworks_adapters::prices::OfficialPriceSource::new(http.clone()),
    )));
    runtime_config.native_notifications = Some(Arc::new(
        crate::native_notifications::NativeOsNotifications::new(),
    ));
    runtime_config.desktop = Some(devworks_engine::runtime::DesktopComposition {
        platform: config
            .desktop_platform
            .clone()
            .unwrap_or_else(|| Arc::new(devworks_adapters::desktop::NativeDesktop)),
    });
    // The run-control quiesce (live work held for Continue) gets this cooperative period; it
    // stays within the shell's Quiescing bound, so the window persists only after it ended.
    runtime_config.shutdown_grace = runtime_config
        .shutdown_grace
        .min(quiesce_bound(config.shutdown_bound));
    let git_root = config.data_root.join("worktrees");
    let git = std::fs::create_dir_all(&git_root)
        .and_then(|()| std::fs::canonicalize(&git_root))
        .ok()
        .and_then(|path| {
            let name = path.to_str()?;
            let digest = devworks_core::fingerprint::sha256_hex(name.as_bytes());
            let root = Id::new(format!("git-root-{}", &digest[..24])).ok()?;
            Some(devworks_engine::git::GitComposition {
                config: devworks_core::services::git::GitConfig {
                    transport: Some(Arc::new(
                        devworks_adapters::git_transport::NativeGitTransportFiles,
                    )),
                    roots: vec![devworks_core::services::git::RegisteredGitRoot { id: root, path }],
                    filesystem: Some(Arc::new(NativeFilesystem::new())),
                    ..devworks_core::services::git::GitConfig::default()
                },
                github_http: Some(http),
            })
        });
    let composition = Composition {
        store: store.clone(),
        clock: clock.clone(),
        ids: id_source.clone(),
        authority: authority.clone(),
        artifacts: artifact_ports,
        process_host: Arc::new(NativeProcessHost),
        // Production run control (stored plan approval, no coordinator launcher until the agent
        // supervisor exists): accepted work is held at shutdown and boot.
        runs: RunComposition::default(),
        power: Arc::new(NativePower),
        diagnostics: diagnostic_ports,
        knowledge: KnowledgeComposition {
            files: Some(artifact_files),
        },
        application,
        web: Some(config.web.clone()),
        // Owned browser sessions: discovered or configured Chromium-family browsers, owned
        // profiles under the data root, launched through the process owner.
        git,
        browser: Some(devworks_engine::browser::BrowserComposition {
            platform: Arc::new(NativeBrowserPlatform::new(
                &config.data_root,
                config.browser_path.clone(),
            )),
            fs: Arc::new(NativeFilesystem::new()),
            app_data_root: Some(config.data_root.to_string_lossy().into_owned()),
            config: devworks_core::services::browser::BrowserConfig::default(),
            resolver: Arc::new(devworks_adapters::web::resolve::SystemResolver),
        }),
        packages: Some(PackageComposition {
            service: package_ports,
            launch: LaunchPorts {
                fs: Arc::new(NativeFilesystem::new()),
                roots: protected_roots(config),
                protected_branches: Vec::new(),
            },
            config: HostConfig::default(),
            runtimes: Some(RuntimePorts {
                cache: runtimes,
                search_path: None,
            }),
            index: Some(package_index),
        }),
        use_cases,
        test_overrides: Default::default(),
        // Repository discovery and structural indexing (DW-23): Tree-sitter grammars, native
        // no-follow source reads and Git through the process owner. Semantic packs register
        // with the indexing environment when installed (DW-24..27).
        indexing: Some(IndexComposition {
            parser: Arc::new(TreeSitterParser::new()),
            source: Arc::new(NativeSourceTree::new()),
            fs: Arc::new(NativeFilesystem::new()),
            settings: IndexSettings::default(),
            config: config.indexing.clone(),
            enrichers: Vec::new(),
        }),
        consumers: Vec::new(),
        config: runtime_config,
    };
    // Recovery and every required consumer start here, before any request below is admitted.
    let runtime = Arc::new(boot(composition).await.map_err(StartError::Boot)?);
    let web = runtime
        .web()
        .cloned()
        .ok_or_else(|| StartError::Thread("the runtime composed no web tools".into()))?;
    let connections = runtime
        .connections()
        .cloned()
        .ok_or_else(|| StartError::Thread("the runtime composed no provider connections".into()))?;
    // A previous boot cannot still be producing a paste. Live draft files require the
    // client keeper snapshot; this owner only reconciles incomplete reserved writes.
    if let Err(error) = ManagedFileCustody::new(store.clone()).recover_client_staging(
        &devworks_adapters::fs::client_staging::NativeClientStagingFiles::new(
            config.data_root.join("staging-ui"),
        ),
    ) {
        // Its durable Writing row continues to hold purge preparation on uncertainty.
        eprintln!("UI staging recovery remains held: {error}");
    }
    // Host bookkeeping after recovery: the local user is the human principal of the UI.
    let user = ActorId(Id::new(USER_ACTOR).expect("valid actor"));
    let (registered, name) = (user.clone(), config.user_name.clone());
    let st = store.clone();
    tokio::task::spawn_blocking(move || {
        run_write(&*st, WriteOptions::recovery(), move |tx| {
            tx.actors()
                .register(&registered, PrincipalKind::Human, &name)?;
            tx.decisions().bind_notification_user(&registered)
        })
    })
    .await
    .map_err(|e| StartError::Thread(e.to_string()))?
    .map_err(StartError::Principal)?;
    let info = store.info();
    let ctx = HostIngress::new(&*store).context(
        user,
        PrincipalKind::Human,
        ids.next_id(IdKind::Command),
        Id::new(UI_INGRESS).expect("valid namespace"),
        Counter::new(info.restore_epoch).map_err(|e| StartError::Thread(e.to_owned()))?,
        Revision::new(1).expect("positive"),
    );
    runtime
        .runs()
        .install_launcher(Arc::new(
            devworks_engine::agent_supervisor::AgentLauncher::new(
                store.clone(),
                id_source,
                runtime.agents().clone(),
                Arc::new(devworks_engine::agent_turns::NativeAgentTurnExecutor::new(
                    &runtime,
                )),
            ),
        ))
        .map_err(StartError::Principal)?;
    if config.app_update.is_none()
        && let Ok(Some(approval)) = updates.pending_approval()
    {
        let (outcome, detail) = crate::app_updates::previous_outcome(&config.data_root, &approval)
            .unwrap_or_else(|error| (InstallOutcome::Uncertain, error));
        if let Err(error) = updates.finish(&approval, outcome, &detail) {
            eprintln!("App update outcome remains held: {error}");
        }
    }
    runtime.notification_wakeup();
    Ok(Host {
        runtime,
        navigation,
        async_runtime: tokio::runtime::Handle::current(),
        ctx,
        sources,
        purge_client,
        data_root: config.data_root.clone(),
        deletion_folders: {
            let roots = protected_roots(config);
            devworks_core::authority::deletion::HostFolders {
                app_data_root: roots.app_data_root,
                skill_roots: roots.skill_roots,
            }
        },
        ids,
        waker: Arc::new(Mutex::new(None)),
        authority,
        connections,
        switchable,
        web,
        mcp_clients,
        restart: Mutex::new(None),
        updates,
    })
}

/// How the engine thread ended.
#[derive(Debug)]
pub struct ShutdownSummary {
    /// The owned-process stop and resource reconciliation ([`Runtime::shutdown`]).
    pub processes: ShutdownReport,
}

/// How long the shell's Quiescing step waits for the runtime's: the shell's own bound, never
/// longer than the runtime's whole shutdown bound.
pub fn quiesce_bound(shutdown_bound: Duration) -> Duration {
    crate::shell::QUIESCE_BOUND.min(shutdown_bound)
}

/// The running engine: its thread, its Tokio runtime and the booted [`Host`].
pub struct Engine {
    host: Arc<Host>,
    shutdown_bound: Duration,
    stop: Option<oneshot::Sender<()>>,
    done: mpsc::Receiver<ShutdownSummary>,
    finished: Option<ShutdownSummary>,
    thread: Option<JoinHandle<()>>,
    /// Stops the connection ticker (dropping it is enough).
    ticker: Option<mpsc::Sender<()>>,
}

/// Runs [`ConnectionService::tick`] every `every` on its own thread until stopped. The thread is
/// never joined: a step in flight finishes within its bounded HTTP timeouts and only writes
/// through the store, which refuses once closed, so it can never hold the close lifecycle.
fn start_connection_ticker(host: Arc<Host>, every: Duration) -> mpsc::Sender<()> {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let service = host.connections.clone();
    drop(host);
    let _ = std::thread::Builder::new()
        .name("devworks-connections".into())
        .spawn(move || {
            loop {
                match stop_rx.recv_timeout(every) {
                    Err(mpsc::RecvTimeoutError::Timeout) => service.tick(),
                    // Stopped, or the engine is gone.
                    _ => return,
                }
            }
        });
    stop_tx
}

fn arm_workspace_restore(host: &Host, id: &Id) -> Result<(), StorageError> {
    let service = host
        .runtime
        .workspace_archives()
        .ok_or(StorageError::ViewUnavailable)?;
    let record = service
        .inspect_workspace_archive(&host.ctx, id)?
        .filter(|value| value.phase == devworks_core::ports::storage::ArchivePhase::AwaitingRestart)
        .ok_or(StorageError::ScopeDenied)?;
    if !host.runtime.archive_shutdown_verified() {
        return Err(StorageError::InvalidRecord(
            "DevWorks could not verify shutdown of all local work. Restore was not armed.".into(),
        ));
    }
    let boot = host
        .runtime
        .store()
        .info()
        .boot_id
        .ok_or(StorageError::ViewUnavailable)?;
    devworks_adapters::storage::sqlite::workspace_archive::arm_restore(
        &host.data_root,
        &record,
        &boot,
        &host.ids.next_id(IdKind::Operation),
    )
}

impl Engine {
    /// Opens the store and boots on a dedicated engine thread; returns once recovery finished
    /// and the runtime admits requests (or why it could not).
    pub fn start(config: DesktopConfig) -> Result<Self, StartError> {
        let shutdown_bound = config.shutdown_bound;
        let connection_tick = config.connection_tick;
        let (booted_tx, booted_rx) = mpsc::channel::<Result<Arc<Host>, StartError>>();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<ShutdownSummary>();
        let thread = std::thread::Builder::new()
            .name("devworks-engine".into())
            .spawn(move || {
                let rt = match desktop_runtime() {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = booted_tx.send(Err(StartError::Thread(e.to_string())));
                        return;
                    }
                };
                rt.block_on(async move {
                    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
                    let ids = Arc::new(HostIds::new());
                    let mut sqlite = SqliteConfig::new(
                        config.data_root.clone(),
                        clock.clone(),
                        ids.clone(),
                        ActorId(Id::new(HOST_ACTOR).expect("valid actor")),
                    );
                    sqlite.app_build = config.app_build.clone();
                    sqlite.app_update = config.app_update.clone();
                    let archive_files = Arc::new(NativeWorkspaceArchives::new(
                        config.data_root.clone(),
                        Arc::new(NativeArtifactFiles::new(
                            &config.data_root,
                            config.data_root.join("cache"),
                        )),
                        Arc::new(NativePackageFiles::new(config.data_root.join("packages"))),
                    ));
                    sqlite.workspace_archives = Some(archive_files.clone());
                    let opened = tokio::task::spawn_blocking(move || SqliteStore::open(sqlite))
                        .await
                        .map_err(|e| StartError::Thread(e.to_string()))
                        .and_then(|r| r.map_err(StartError::Store));
                    let store = match opened {
                        Ok(s) => Arc::new(s),
                        Err(e) => {
                            let _ = booted_tx.send(Err(e));
                            return;
                        }
                    };
                    let host = match compose(&config, store, archive_files, clock, ids).await {
                        Ok(h) => Arc::new(h),
                        Err(e) => {
                            let _ = booted_tx.send(Err(e));
                            return;
                        }
                    };
                    // "Queue state may have changed" hints wake the UI; a missed hint is repaired
                    // by the shell's durable re-query, never by replay.
                    let mut hints = host.runtime.dispatcher().subscribe();
                    let waker = host.waker.clone();
                    let forward = tokio::spawn(async move {
                        while hints.changed().await.is_ok() {
                            if let Some(w) = waker.locked().as_ref() {
                                w();
                            }
                        }
                    });
                    let _ = booted_tx.send(Ok(host.clone()));
                    let _ = stop_rx.await;
                    let processes = host.runtime.shutdown().await;
                    forward.abort();
                    let restart = host.restart.locked().take();
                    if let Some(Restart::Restore(proposal)) = restart.as_ref()
                        && let Err(error) = arm_workspace_restore(&host, proposal)
                    {
                        if let Some(service) = host.runtime.workspace_archives() {
                            let _ = service.note_shutdown_failure(
                                &host.ctx,
                                proposal,
                                &error.to_string(),
                            );
                        }
                        let mut entry = devworks_core::services::diagnostics::LogEntry::new(
                            "workspace-archive",
                            "restore-shutdown",
                            devworks_core::protocol::records::Severity::Error,
                            error.to_string(),
                        );
                        entry.operation = Some("workspace.restore".into());
                        entry.target_id = Some(proposal.clone());
                        let _ = host.runtime.diagnostics().log(entry);
                    }
                    if let Some(Restart::CancelledUpdate(approval)) = restart.as_ref() {
                        let _ = host.updates.finish(
                            approval,
                            InstallOutcome::Cancelled,
                            "DevWorks closed without installing the cancelled update.",
                        );
                    }
                    if let Some(Restart::Update(approval)) = restart {
                        let prepared = if host.runtime.archive_shutdown_verified() {
                            host.updates
                                .begin_install(&approval)
                                .map_err(|error| error.to_string())
                        } else {
                            Err("DevWorks could not verify that all its local work stopped.".into())
                        };
                        let result = prepared.and_then(|()| {
                            crate::app_updates::launch_helper(&host.data_root, (*approval).clone())
                        });
                        if let Err(error) = result {
                            let _ =
                                host.updates
                                    .finish(&approval, InstallOutcome::NotStarted, &error);
                            eprintln!("DevWorks update was held: {error}");
                        }
                    }
                    let _ = done_tx.send(ShutdownSummary { processes });
                });
            })
            .map_err(|e| StartError::Thread(e.to_string()))?;
        let host = booted_rx
            .recv()
            .map_err(|_| StartError::Thread("the engine thread stopped during boot".into()))??;
        let ticker = connection_tick.map(|every| start_connection_ticker(host.clone(), every));
        Ok(Self {
            host,
            shutdown_bound,
            stop: Some(stop_tx),
            done: done_rx,
            finished: None,
            thread: Some(thread),
            ticker,
        })
    }

    pub fn host(&self) -> &Arc<Host> {
        &self.host
    }

    /// The backend the shell admits requests through.
    pub fn backend(&self) -> Arc<dyn Backend> {
        Arc::new(DesktopBackend {
            host: self.host.clone(),
        })
    }

    /// Starts the bounded shutdown (stop delivery, collection and housekeeping, wait for
    /// in-flight deliveries, stop owned trees, release keep-awake) without blocking the caller.
    pub fn begin_shutdown(&mut self) {
        // No new background connection step starts once the close lifecycle begins.
        if let Some(ticker) = self.ticker.take() {
            let _ = ticker.send(());
        }
        // Web requests in flight stop at their next bounded wait; new ones are refused.
        self.host.web.shutdown();
        self.host.updates.stop_polls();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }

    /// The shutdown outcome once the engine finished it.
    pub fn poll_shutdown(&mut self) -> Option<&ShutdownSummary> {
        if self.finished.is_none() {
            self.finished = self.done.try_recv().ok();
        }
        self.finished.as_ref()
    }

    /// Shuts down and waits for the engine thread (blocking).
    pub fn shutdown(mut self) -> Option<ShutdownSummary> {
        self.begin_shutdown();
        if self.finished.is_none() {
            self.finished = self.done.recv().ok();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.finished.take()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.begin_shutdown();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The close lifecycle drives [`Runtime::shutdown`] (AD 20): Quiescing starts it, which fences
/// admission and holds every live feature/issue root for manual Continue (the run-control
/// shutdown path); the shell persists once that quiesce stage has passed, then waits for the
/// rest of the same shutdown (delivery, collection, owned trees), whose stages it shows.
impl crate::shell::HostControl for Engine {
    fn can_cancel_update(&self) -> bool {
        matches!(&*self.host.restart.locked(), Some(Restart::Update(_)))
    }

    fn cancel_update(&mut self) -> bool {
        let mut restart = self.host.restart.locked();
        if matches!(&*restart, Some(Restart::Update(_)))
            && let Some(Restart::Update(approval)) = restart.take()
        {
            *restart = Some(Restart::CancelledUpdate(approval));
            return true;
        }
        false
    }

    fn begin_quiesce(&mut self) {
        Engine::begin_shutdown(self);
    }
    fn quiesced(&mut self) -> bool {
        self.host.runtime.shutdown_stage() >= ShutdownStage::StoppingWork
    }
    fn quiesce_bound(&self) -> Duration {
        quiesce_bound(self.shutdown_bound)
    }
    fn progress(&self) -> Option<String> {
        Some(
            match self.host.runtime.shutdown_stage() {
                ShutdownStage::Open => "Finishing requests in flight…",
                ShutdownStage::DrainingAdmission => "Finishing admitted requests…",
                ShutdownStage::Quiescing => "Holding active work for Continue at the next launch…",
                ShutdownStage::StoppingWork => "Stopping delivery, collection and owned processes…",
                ShutdownStage::Done => "Runtime stopped.",
            }
            .to_owned(),
        )
    }
    fn begin_shutdown(&mut self) {
        Engine::begin_shutdown(self);
    }
    fn poll_shutdown(&mut self) -> Option<String> {
        self.poll_shutdown().map(|s| {
            let stopped = s.processes.stopped.len();
            let recorded = s.processes.reconciliation.is_ok();
            format!(
                "Runtime stopped: {stopped} owned process tree{} stopped; resource reconciliation {}.",
                if stopped == 1 { "" } else { "s" },
                if recorded { "recorded" } else { "could not be recorded (it runs again at the next launch)" }
            )
        })
    }
}

/// [`Backend`] over the booted runtime with the host-bound user context.
pub struct DesktopBackend {
    host: Arc<Host>,
}
impl DesktopBackend {
    fn archive_service(
        &self,
        received: ControlEpoch,
    ) -> Result<&devworks_core::services::workspace_archive::WorkspaceArchiveService, String> {
        if !self.host.runtime.control_quiet_since(received) {
            return Err("Workspace archive actions require direct desktop input.".into());
        }
        self.host
            .runtime
            .workspace_archives()
            .map(AsRef::as_ref)
            .ok_or_else(|| "Workspace archive service is unavailable.".into())
    }
}

impl Backend for DesktopBackend {
    fn update_status(&self) -> Result<UpdateStatus, String> {
        self.host
            .updates
            .status()
            .map_err(|error| error.to_string())
    }

    fn poll_updates(&self, manual: bool) -> Result<(), String> {
        self.host
            .updates
            .poll(manual)
            .map_err(|error| error.to_string())
    }

    fn cancel_update_download(&self) {
        self.host.updates.cancel_poll();
    }

    fn set_update_channel(
        &self,
        channel: UpdateChannel,
        received: ControlEpoch,
    ) -> Result<(), String> {
        if !self.host.runtime.control_quiet_since(received) {
            return Err("Release settings require direct desktop input.".into());
        }
        self.host
            .updates
            .set_channel(&self.host.ctx, channel)
            .map_err(|error| error.to_string())
    }

    fn approve_app_update(
        &self,
        frame: &[u8],
        proposal: &Id,
        received: ControlEpoch,
    ) -> Result<(), String> {
        use devworks_core::protocol::CommandReply;
        use devworks_core::protocol::results::DomainDecision;
        let mut restart = self.host.restart.locked();
        if restart.is_some() || !self.host.runtime.control_quiet_since(received) {
            return Err(
                "This restart requires direct desktop input and an available restart action."
                    .into(),
            );
        }
        let outcome = self
            .host
            .runtime
            .workspace_maintenance_received(&self.host.ctx, APPROVE, frame, received)
            .map_err(|error| format!("The install approval could not be recorded. {error:?}"))?;
        let receipt = match outcome {
            CommandReply::Recorded { receipt, replayed: false, .. } => receipt,
            CommandReply::Rejected { code, .. } => {
                return Err(format!("DevWorks rejected the install approval. {code:?}"));
            }
            CommandReply::CommandStatusUnknown { .. } => {
                return Err("The install approval outcome is unknown. DevWorks will keep the current build.".into());
            }
            _ => return Err("This approval was previously recorded. Review the current update before trying again.".into()),
        };
        let DomainDecision::Accepted { operation_id } = receipt.decision else {
            return Err("The staged release or migration plan changed. Review the current update before approving it.".into());
        };
        let approval = self
            .host
            .updates
            .approval(proposal)
            .map_err(|error| error.to_string())?;
        if approval.operation != operation_id || !self.host.runtime.control_quiet_since(received) {
            let _ = self.host.updates.finish(
                &approval,
                InstallOutcome::NotStarted,
                "The app restart confirmation changed before it was armed.",
            );
            return Err("The app restart confirmation is no longer current.".into());
        }
        *restart = Some(Restart::Update(Box::new(approval)));
        Ok(())
    }

    fn workspace_archive_command(
        &self,
        operation: &str,
        frame: &[u8],
        received: ControlEpoch,
    ) -> CommandOutcome {
        if devworks_core::services::workspace_archive::OPERATIONS.contains(&operation) {
            self.host.runtime.workspace_maintenance_received(
                &self.host.ctx,
                operation,
                frame,
                received,
            )
        } else {
            self.command(operation, frame)
        }
    }

    fn workspace_backup_folder(
        &self,
        path: &Path,
        received: ControlEpoch,
    ) -> Result<(Id, i64), String> {
        self.archive_service(received)?
            .register_folder(&self.host.ctx, path)
            .map_err(|error| error.to_string())
    }

    fn workspace_restore_plan(
        &self,
        path: &Path,
        received: ControlEpoch,
        cancel: &devworks_core::ports::storage::Cancellation,
    ) -> Result<devworks_core::ports::storage::StoredWorkspaceArchive, String> {
        self.archive_service(received)?
            .prepare_restore(&self.host.ctx, path, cancel)
            .map_err(|error| error.to_string())
    }

    fn workspace_migration_plan(
        &self,
        received: ControlEpoch,
    ) -> Result<devworks_core::ports::storage::StoredWorkspaceArchive, String> {
        self.archive_service(received)?
            .prepare_active_migration_check(&self.host.ctx)
            .map_err(|error| error.to_string())
    }

    fn cancel_workspace_archive(
        &self,
        id: &Id,
        revision: i64,
        received: ControlEpoch,
    ) -> Result<(), String> {
        self.archive_service(received)?
            .cancel_archive(&self.host.ctx, id, revision)
            .map_err(|error| error.to_string())
    }

    fn observe_archived_workspace_git(
        &self,
        id: i64,
        received: ControlEpoch,
    ) -> Result<bool, String> {
        let git = self
            .host
            .runtime
            .git()
            .ok_or("Git observation is unavailable.")?;
        self.archive_service(received)?
            .observe_restored_git(&self.host.ctx, id, git)
            .map_err(|error| error.to_string())
    }

    fn observe_archived_workspace_export(
        &self,
        id: i64,
        path: &Path,
        received: ControlEpoch,
    ) -> Result<bool, String> {
        self.archive_service(received)?
            .observe_restored_export(&self.host.ctx, id, path)
            .map_err(|error| error.to_string())
    }

    fn workspace_archive_status(&self) -> Result<crate::settings_screen::archives::Status, String> {
        let service = self
            .host
            .runtime
            .workspace_archives()
            .ok_or("Workspace archive service is unavailable.")?;
        let (current, restored) = service
            .status(&self.host.ctx)
            .map_err(|error| error.to_string())?;
        Ok(crate::settings_screen::archives::Status {
            current,
            restored,
            shipped_schema: service.shipped_schema().map_err(|e| e.to_string())?,
            unresolved: service
                .unresolved_effects(&self.host.ctx, 0)
                .map_err(|error| error.to_string())?,
        })
    }

    fn request_workspace_restore_shutdown(
        &self,
        proposal: &Id,
        received: ControlEpoch,
    ) -> Result<(), String> {
        let record = self
            .archive_service(received)?
            .inspect_workspace_archive(&self.host.ctx, proposal)
            .map_err(|error| error.to_string())?;
        if !record.is_some_and(|value| {
            value.phase == devworks_core::ports::storage::ArchivePhase::AwaitingRestart
        }) {
            return Err("The selected restore no longer waits for restart.".into());
        }
        let mut restart = self.host.restart.locked();
        if restart.is_some() {
            return Err("Another workspace action already owns this restart.".into());
        }
        *restart = Some(Restart::Restore(proposal.clone()));
        Ok(())
    }

    fn publish_current_screen(
        &self,
        screen: Option<(Id, devworks_core::protocol::records::ScreenContext)>,
        selection_key: &str,
    ) {
        self.host.navigation.publish(screen, selection_key);
    }
    fn keep_working_status(
        &self,
        project: &devworks_core::protocol::ProjectId,
    ) -> Option<devworks_engine::KeepWorkingStatus> {
        self.host.runtime.keep_working_status(project)
    }
    fn set_keep_working(
        &self,
        project: &devworks_core::protocol::ProjectId,
        enabled: bool,
        received: ControlEpoch,
    ) -> Result<(), String> {
        self.host
            .runtime
            .set_keep_working(&self.host.ctx, project, enabled, received)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
    fn read_coordinator_notice(
        &self,
        reference: &devworks_core::protocol::provider::ArtifactRef,
    ) -> Result<String, String> {
        let (bytes, _) = self
            .host
            .runtime
            .artifacts()
            .read_exact(
                &**self.host.runtime.store(),
                &self.host.ctx,
                reference,
                // A watcher notice carries an output tail of up to 64 KiB.
                96_000,
            )
            .map_err(|error| format!("host notice read: {error:?}"))?;
        String::from_utf8(bytes).map_err(|_| "coordinator context is not UTF-8".into())
    }
    fn append_coordinator_notice(
        &self,
        session: &Id,
        screen: &str,
        context: &str,
    ) -> Result<(), String> {
        let store = self.host.runtime.store();
        let info = store.info();
        let host = HostIngress::new(&**store).context(
            info.host_actor,
            PrincipalKind::Host,
            self.host.ids.next_id(IdKind::Correlation),
            self.host.ids.next_id(IdKind::Invocation),
            Counter::new(info.restore_epoch).map_err(|e| e.to_string())?,
            Revision::new(1).map_err(|e| e.to_string())?,
        );
        let topic = crate::help_ops::topics(self.query(
            "help.topic_for_screen",
            &crate::help_ops::context(self, screen, None),
        ))
        .ok()
        .and_then(|page| {
            page.items
                .0
                .into_iter()
                .find(|topic| topic.id.as_str() == screen)
        });
        let context = topic.map_or_else(
            || context.to_owned(),
            |topic| format!("{context} ({})", topic.version.0),
        );
        devworks_core::services::sessions::notice::append_screen_context(
            &**store,
            self.host.runtime.artifacts(),
            &host,
            session,
            screen,
            &context,
        )
        .map_err(|e| format!("coordinator context notice: {e:?}"))
    }
    fn purge_client_acknowledge(
        &self,
        scope: &devworks_core::ports::storage::ProjectPurgeClientScope,
    ) {
        self.host.purge_client.acknowledge(scope);
    }

    fn purge_client_publish(
        &self,
        memory: crate::memory::Memory,
        live: crate::project_purge::Live,
        save: bool,
    ) -> Result<Option<Vec<devworks_core::ports::storage::ProjectPurgeClientScope>>, String> {
        self.host.purge_client.publish(memory, live, save).map(Some)
    }
    fn purge_client_freeze(&self, project: &str, frozen: bool) -> Result<(), String> {
        self.host.purge_client.freeze(project, frozen)
    }

    fn query(&self, operation: &str, frame: &[u8]) -> QueryOutcome {
        self.host.runtime.query(&self.host.ctx, operation, frame)
    }
    fn supports_query_cancellation(&self) -> bool {
        true
    }
    fn query_cancellable(
        &self,
        operation: &str,
        frame: &[u8],
        options: &devworks_core::ports::storage::ReadOptions,
    ) -> QueryOutcome {
        self.host
            .runtime
            .query_with(&self.host.ctx, operation, frame, options)
    }
    fn command(&self, operation: &str, frame: &[u8]) -> CommandOutcome {
        self.host.runtime.command(&self.host.ctx, operation, frame)
    }
    fn process_completion(
        &self,
        operation: &devworks_core::protocol::OperationId,
    ) -> Result<
        Option<devworks_core::protocol::results::OperationCompletion>,
        devworks_core::protocol::DomainFault,
    > {
        match self.host.runtime.process_tools() {
            Some(service) => service.completion(&self.host.ctx, operation),
            None => Ok(None),
        }
    }
    fn desktop_completion(
        &self,
        operation: &devworks_core::protocol::OperationId,
    ) -> Result<
        Option<devworks_core::protocol::results::OperationCompletion>,
        devworks_core::protocol::DomainFault,
    > {
        match self.host.runtime.desktop() {
            Some(service) => service.completion(&self.host.ctx, operation),
            None => Ok(None),
        }
    }
    fn systemone_completion(
        &self,
        operation: &devworks_core::protocol::OperationId,
    ) -> Result<
        Option<devworks_core::protocol::results::OperationCompletion>,
        Box<devworks_core::protocol::DomainFault>,
    > {
        self.host
            .runtime
            .systemone_completion(&self.host.ctx, operation)
    }
    fn control_epoch(&self) -> ControlEpoch {
        self.host.runtime.desktop_control_epoch()
    }
    fn human_decision(&self, frame: &[u8], received: ControlEpoch) -> CommandOutcome {
        self.host
            .runtime
            .record_human_decision_received(&self.host.ctx, frame, received)
    }
    fn confirm_project_purge(&self, frame: &[u8], received: ControlEpoch) -> CommandOutcome {
        self.host
            .runtime
            .confirm_project_purge_received(&self.host.ctx, frame, received)
    }
    fn repository_deletion_supported(&self) -> bool {
        use devworks_core::authority::destructive::FilesystemResolver;
        NativeFilesystem::new().destructive_steps_supported()
    }
    fn repository_deletion_preview(
        &self,
        repository: &Id,
    ) -> Result<devworks_core::authority::deletion::DeletionPreview, String> {
        devworks_core::authority::deletion::preview(
            &**self.host.runtime.store(),
            &NativeFilesystem::new(),
            &self.host.deletion_folders,
            repository,
        )
        .map_err(|r| crate::repository_deletion::refusal_text(&r))
    }
    fn delete_repository(
        &self,
        reviewed: &devworks_core::authority::deletion::DeletionPreview,
        received: ControlEpoch,
    ) -> Result<devworks_core::authority::destructive::RemovalOutcome, String> {
        // Desktop automation can generate the confirming click: the dedicated confirmation is
        // refused unless nobody held desktop control from the click until now, so an agent never
        // confirms through the UI (not even by releasing control before this worker runs).
        if !self.host.runtime.control_quiet_since(received) {
            return Err(crate::repository_deletion::refusal_text(
                &devworks_core::authority::deletion::DeletionRefusal::HumanOnly,
            ));
        }
        devworks_core::authority::deletion::delete(
            &**self.host.runtime.store(),
            &NativeFilesystem::new(),
            &self.host.deletion_folders,
            &self.host.ctx,
            reviewed,
        )
        .map_err(|r| crate::repository_deletion::refusal_text(&r))
    }
    fn workspace_reply_status(
        &self,
        session: &Id,
    ) -> Result<Option<devworks_engine::WorkspaceReplyStatus>, String> {
        self.host
            .runtime
            .workspace_reply_status(&self.host.ctx, session)
            .map_err(|error| error.to_string())
    }
    fn text_preview(
        &self,
        session: &Id,
    ) -> Result<Option<devworks_engine::providers::live::LiveTurn>, String> {
        let record = crate::ops::session_record(self.query(
            "sessions.get",
            &crate::ops::session_get(self, session.as_str()),
        ))
        .map_err(|e| e.describe())?;
        Ok(self.host.runtime.providers().and_then(|providers| {
            providers.text_preview(session).filter(|turn| {
                turn.session_id == record.id && turn.context_epoch == record.context_epoch.get()
            })
        }))
    }
    fn set_application_focus(&self, focused: Option<bool>) {
        self.host.runtime.set_notification_focus(focused);
    }
    fn os_notification_permission(
        &self,
    ) -> devworks_core::ports::os_notifications::OsNotificationPermission {
        self.host.runtime.notification_permission()
    }
    fn request_os_notification_permission(
        &self,
    ) -> devworks_core::ports::os_notifications::OsNotificationPermission {
        self.host
            .async_runtime
            .block_on(self.host.runtime.request_notification_permission())
    }
    fn mint_workspace_upload(&self, file: &Path) -> Result<Id, String> {
        self.host
            .sources
            .mint_workspace_upload(&self.host.ctx, file, UPLOAD_TICKET_MS)
            .map_err(|e| match e {
                IssueRefusal::NotAFile => "only regular files can be attached".to_owned(),
                other => format!("{other:?}"),
            })
    }
    fn mint_project_upload(
        &self,
        project: &devworks_core::protocol::ProjectId,
        file: &Path,
    ) -> Result<Id, String> {
        let ctx = self
            .host
            .ctx
            .clone()
            .with_lineage(Some(project.clone()), None, None);
        self.host
            .sources
            .mint_upload(&ctx, file, UPLOAD_TICKET_MS)
            .map_err(|e| match e {
                IssueRefusal::NotAFile => "only regular files can be attached".to_owned(),
                other => format!("{other:?}"),
            })
    }
    fn keep_awake_health(&self) -> Health {
        self.host.runtime.keep_awake_health()
    }
    fn hints(&self) -> Option<watch::Receiver<u64>> {
        Some(self.host.runtime.dispatcher().subscribe())
    }
    fn set_waker(&self, waker: Waker) {
        *self.host.waker.locked() = Some(waker);
    }
    fn token(&self, prefix: &str) -> String {
        self.host.ids.token(prefix)
    }
    fn scratch_dir(&self) -> PathBuf {
        self.host.data_root.join("staging-ui")
    }
    fn stage_pasted_image(&self, session: Option<&Id>, png: &[u8]) -> Result<PathBuf, String> {
        ManagedFileCustody::new(self.host.runtime.store().clone()).stage_paste(
            &devworks_adapters::fs::client_staging::NativeClientStagingFiles::new(
                self.scratch_dir(),
            ),
            session,
            &self.host.ids.next_id(IdKind::Artifact),
            png,
        )
    }
    fn stage_project_text(
        &self,
        project: Option<&devworks_core::protocol::ProjectId>,
        extension: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, String> {
        ManagedFileCustody::new(self.host.runtime.store().clone()).stage_project_text(
            &devworks_adapters::fs::client_staging::NativeClientStagingFiles::new(
                self.scratch_dir(),
            ),
            project,
            &self.host.ids.next_id(IdKind::Artifact),
            extension,
            bytes,
        )
    }
    fn refresh_packages(&self) -> Result<(), String> {
        let host = self
            .host
            .runtime
            .packages()
            .ok_or("packages are not composed")?;
        host.refresh_runtimes();
        host.refresh_indexes();
        host.reconcile();
        Ok(())
    }
    fn fetch_index_bundle(&self, package: &str, digest: &str) -> Result<PathBuf, String> {
        let host = self
            .host
            .runtime
            .packages()
            .ok_or("packages are not composed")?;
        host.fetch_bundle(package, digest, &self.scratch_dir().join("packages"))
    }
    fn index_sources(
        &self,
        project: Option<&str>,
    ) -> Result<devworks_core::ports::storage::StoredSourceConfig, String> {
        let key = project.map_or_else(
            || devworks_core::ports::storage::WORKSPACE_SCOPE_KEY.to_owned(),
            str::to_owned,
        );
        devworks_core::ports::storage::read_snapshot(
            &**self.host.runtime.store(),
            &Default::default(),
            |s| devworks_core::services::indexing::section_config(s.indexing(), &key),
        )
        .map_err(|e| e.to_string())
    }
    fn native_job_capabilities(&self) -> Vec<devworks_core::services::jobs::NativeCapability> {
        self.host.runtime.jobs().catalog().capabilities()
    }
    fn read_stream(&self, stream: &Id, max: u64) -> Result<Vec<u8>, String> {
        use std::io::Read;
        let runtime = &self.host.runtime;
        let opened = runtime
            .artifacts()
            .open_stream(
                &*self.host.authority,
                &**runtime.store(),
                &self.host.ctx,
                stream,
            )
            .map_err(|e| format!("the stored file could not be opened: {e:?}"))?;
        let mut bytes = Vec::new();
        opened
            .take(max.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > max {
            return Err("too large to preview".into());
        }
        Ok(bytes)
    }
    fn stage_api_key(&self, key: Secret) -> Result<Id, String> {
        self.host
            .connections
            .stage_api_key(self.host.ctx.principal(), key)
            .map_err(|e| e.to_string())
    }
    fn stage_mcp_api_key(&self, key: Secret) -> Result<Id, String> {
        self.host
            .connections
            .stage_mcp_api_key(self.host.ctx.principal(), key)
            .map_err(|e| e.to_string())
    }
    fn release_mcp_credentials(&self, handles: Vec<Id>) -> Result<(), String> {
        self.host.connections.release_mcp_credentials(&handles);
        Ok(())
    }
    fn generate_database_encryption_key(&self) -> Result<Secret, String> {
        self.host
            .runtime
            .schemas()
            .ok_or_else(|| "Database metadata is unavailable".to_owned())?
            .generate_encryption_key(&self.host.ctx, &devworks_adapters::OsEntropy)
            .map_err(|_| "Encryption key generation is unavailable".to_owned())
    }
    fn stage_database_password(&self, password: Secret) -> Result<Id, String> {
        self.host
            .runtime
            .schemas()
            .ok_or_else(|| "Database metadata is unavailable".to_owned())?
            .stage_password(&self.host.ctx, password)
            .map_err(|_| {
                "Database password could not be saved in the credential facility".to_owned()
            })
    }
    fn stage_setting_secret(&self, value: Secret) -> Result<String, String> {
        self.host
            .connections
            .stage_setting_secret(value)
            .map(|r| r.as_str().to_owned())
            .map_err(|e| e.to_string())
    }
    fn submit_sign_in_code(&self, flow: &Id, pasted: Secret) -> Result<(), String> {
        self.host
            .connections
            .submit_authorization_code(self.host.ctx.principal(), flow, pasted)
            .map_err(|e| match e {
                IntakeError::UnknownFlow => {
                    "this sign-in is no longer open; start it again".to_owned()
                }
                IntakeError::NotBrowserFlow => {
                    "this sign-in uses a device code, not a pasted code".to_owned()
                }
            })
    }
    fn secret_store(&self) -> crate::backend::SecretStoreStatus {
        crate::backend::SecretStoreStatus {
            name: self.host.connections.secret_store_name().to_owned(),
            session_only: self
                .host
                .switchable
                .as_ref()
                .is_some_and(|s| s.is_session_only()),
            unavailable: self.host.connections.secret_store_unavailable(),
        }
    }
    fn use_session_only_secrets(&self) -> Result<(), String> {
        match &self.host.switchable {
            Some(s) => {
                s.use_session_only();
                Ok(())
            }
            None => Err("this workspace's secret store cannot be switched".into()),
        }
    }
}

#[cfg(test)]
#[path = "host/provider_refresh_shutdown_tests.rs"]
mod provider_refresh_shutdown_tests;

#[cfg(test)]
#[path = "host/usage_shutdown_tests.rs"]
mod usage_shutdown_tests;

#[cfg(test)]
#[path = "host/provider_refresh_lock_tests.rs"]
mod provider_refresh_lock_tests;

// This is the runtime used by production desktop consumers, including native metadata sockets.
fn desktop_runtime() -> Result<tokio::runtime::Runtime, std::io::Error> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}
#[cfg(test)]
mod metadata_runtime_tests {
    #[test]
    fn production_metadata_runtime_supports_socket_io() {
        super::desktop_runtime().unwrap().block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                    .await
                    .unwrap();
                let address = listener.local_addr().unwrap();
                let (connected, accepted) =
                    tokio::join!(tokio::net::TcpStream::connect(address), listener.accept());
                assert!(connected.is_ok());
                assert!(accepted.is_ok());
            })
            .await
            .expect("production metadata runtime I/O completes");
        });
    }
}

#[cfg(test)]
mod container_credential_boundary_tests {
    use super::*;
    use devworks_core::ports::secrets::{SecretKey, SessionSecrets};
    use devworks_core::services::process::executables::PathExecutables;
    use devworks_core::services::process::tools::{BindingRefusal, RegisteredCredential};

    fn no_executables() -> Arc<PathExecutables> {
        Arc::new(PathExecutables::new(
            Arc::new(|| Some(Vec::new())),
            Arc::new(|| None),
        ))
    }

    #[test]
    fn process_grant_does_not_become_a_container_grant() {
        let mut config = DesktopConfig::new(std::env::temp_dir());
        let grant = RegisteredCredential {
            id: Id::new("credential-1").unwrap(),
            project: None,
            handle: Id::new("secret-1").unwrap(),
        };
        config.process_credentials.push(grant.clone());
        let secrets = SessionSecrets::new();
        secrets
            .put(
                &SecretKey::new("store-1", grant.handle.clone()),
                &Secret::new("private-token"),
            )
            .unwrap();
        let bindings = container_bindings(&config, no_executables()).unwrap();
        assert_eq!(
            bindings.resolve_container_credential(&grant.id, None, &secrets, "store-1"),
            Err(BindingRefusal::UnknownCredential)
        );
        config.container_credentials.push(grant.clone());
        let bindings = container_bindings(&config, no_executables()).unwrap();
        assert_eq!(
            bindings.resolve_container_credential(&grant.id, None, &secrets, "store-1"),
            Ok("private-token".into())
        );
    }
}

#[cfg(test)]
mod skill_root_protection_tests {
    use super::*;
    use devworks_core::authority::destructive::{BlockReason, PreflightVerdict, preflight};
    use devworks_core::authority::shell::{ShellAssessment, assess_command};

    #[test]
    fn agent_commands_cannot_write_either_skill_folder() {
        let scratch =
            std::env::temp_dir().join(format!("devworks-skill-roots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(scratch.join("home")).unwrap();
        // A plain path without links (Windows resolves to a `\\?\` path; the host's paths
        // are plain).
        let canonical = std::fs::canonicalize(&scratch).unwrap();
        let canonical = canonical.to_string_lossy();
        let scratch = PathBuf::from(canonical.strip_prefix(r"\\?\").unwrap_or(&canonical));
        std::fs::create_dir_all(scratch.join("data/skills")).unwrap();
        let mut config = DesktopConfig::new(scratch.join("data"));
        // `~/.agents/skills` before the person ever created it.
        config.shared_skills = Some(scratch.join("home/.agents/skills"));
        let roots = protected_roots(&config);
        for root in [
            scratch.join("home/.agents/skills"),
            scratch.join("data/skills"),
        ] {
            // A command names paths with `/` separators, which every platform accepts, quoted
            // for `sh` so a temporary folder with a space in its name stays one word.
            let root = root.display().to_string().replace('\\', "/");
            let root = format!("'{}'", root.replace('\'', r"'\''"));
            let script = format!("echo 'Do evil.' > {root}/evil/SKILL.md");
            let ShellAssessment::Destructive { plans, .. } = assess_command(
                &["sh".to_owned(), "-c".to_owned(), script.clone()],
                Some(&scratch.display().to_string()),
            ) else {
                panic!("{script}: the write is assessed");
            };
            assert_eq!(
                preflight(&NativeFilesystem::new(), &roots, &plans[0]).verdict,
                PreflightVerdict::Blocked(BlockReason::SkillDirectory),
                "{script}"
            );
        }
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
