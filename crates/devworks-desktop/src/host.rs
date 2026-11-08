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
use devworks_adapters::index::NativeSourceTree;
use devworks_adapters::index::treesitter::TreeSitterParser;
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
use devworks_core::ports::diagnostics::StorageHealthProbe;
use devworks_core::ports::http::HttpTransport;
use devworks_core::ports::packages::{HostPlatform, RuntimeCache};
use devworks_core::ports::retrieval::NoVectorIndex;
use devworks_core::ports::router::ConnectionDirectory;
use devworks_core::ports::secrets::{Secret, SecretResolver, SecretStore, SwitchableSecrets};
use devworks_core::ports::storage::{ApplicationStore, StorageError, WriteOptions, run_write};
use devworks_core::ports::{Clock, IdKind, IdSource};
use devworks_core::protocol::{ActorId, Counter, Id, Revision};
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::diagnostics::DiagnosticService;
use devworks_core::services::history::HistoryService;
use devworks_core::services::indexing::IndexSettings;
use devworks_core::services::knowledge::KnowledgeService;
use devworks_core::services::packages::PackageService;
use devworks_core::services::process::ShutdownReport;
use devworks_core::services::resources::ResourceService;
use devworks_core::services::router::InferenceRouter;
use devworks_core::services::router::connections::{ConnectionService, IntakeError};
use devworks_core::services::runs::ControlService;
use devworks_core::services::sessions::SessionService;
use devworks_core::services::web::{WebPorts, WebService};
use devworks_core::settings::service::SettingsService;
use devworks_core::settings::view::Health;
use devworks_engine::package_host::{HostConfig, IndexPorts, LaunchPorts, RuntimePorts};
use devworks_engine::{
    BootError, BootReport, Composition, IndexComposition, IndexingConfig, PackageComposition,
    RunComposition, Runtime, RuntimeConfig, ShutdownStage, boot,
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
/// Retry-key namespace of the desktop UI surface.
pub const UI_INGRESS: &str = "desktop-ui";
/// Lifetime of an upload ticket minted for an attachment (it is spent at once).
pub const UPLOAD_TICKET_MS: i64 = 10 * 60 * 1000;

/// Snapshot the host's executable search path at boot. IDs are ordinary command names so the
/// tool caller selects a host-owned executable without passing a path. The first PATH match
/// wins, as it would for a native command; the binding rechecks the resolved file at dispatch.
fn process_executables_on_path()
-> Vec<devworks_core::services::process::tools::RegisteredExecutable> {
    use devworks_core::services::process::tools::RegisteredExecutable;
    let mut seen = std::collections::HashSet::new();
    let mut registered = Vec::new();
    let Some(path) = std::env::var_os("PATH") else {
        return registered;
    };
    for directory in std::env::split_paths(&path).filter(|path| path.is_absolute()) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            #[cfg(windows)]
            let name = match path.extension().and_then(|extension| extension.to_str()) {
                Some(extension) if extension.eq_ignore_ascii_case("exe") => path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or(name),
                _ => continue,
            };
            let Ok(id) = Id::new(name) else {
                continue;
            };
            if seen.contains(id.as_str()) {
                continue;
            }
            let Ok(canonical) = path.canonicalize() else {
                continue;
            };
            let Ok(metadata) = std::fs::metadata(&canonical) else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 == 0 {
                    continue;
                }
            }
            seen.insert(id.as_str().to_owned());
            registered.push(RegisteredExecutable { id, path });
        }
    }
    registered
}

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
pub type WebPortsFactory = Arc<dyn Fn(Arc<dyn SecretResolver>) -> WebPorts + Send + Sync>;

/// What the composition root is configured with.
#[derive(Clone)]
pub struct DesktopConfig {
    pub data_root: PathBuf,
    /// The user's display name for attribution.
    pub user_name: String,
    /// The application build recorded with applied migrations.
    pub app_build: String,
    /// Shared bound of the owned-process stop at shutdown (AD20 default 30 s).
    pub shutdown_bound: Duration,
    /// An explicitly configured Chromium-family browser executable (`DEVWORKS_BROWSER`), offered
    /// as the `configured` browser asset beside the discovered Chrome, Chromium and Edge.
    pub browser_path: Option<PathBuf>,
    /// Explicit host grants for process environment credential handles. An unregistered
    /// credentialRef is refused; the generic provider connection handles are not exported.
    pub process_credentials: Vec<devworks_core::services::process::tools::RegisteredCredential>,
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
            shutdown_bound: Duration::from_secs(30),
            browser_path: std::env::var_os("DEVWORKS_BROWSER")
                .filter(|p| !p.is_empty())
                .map(PathBuf::from),
            process_credentials: Vec::new(),
            desktop_platform: None,
            connections: None,
            secrets: None,
            http: None,
            providers: None,
            connection_tick: Some(CONNECTION_TICK),
            web: Arc::new(native_ports),
            indexing: IndexingConfig::default(),
        }
    }
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
    async_runtime: tokio::runtime::Handle,
    ctx: InvocationContext,
    sources: Arc<HostArtifactSources>,
    purge_client: Arc<crate::project_purge::NativePurgeClient>,
    data_root: PathBuf,
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
}
impl Host {
    pub fn runtime(&self) -> &Arc<Runtime> {
        &self.runtime
    }
    pub fn report(&self) -> &BootReport {
        self.runtime.report()
    }
    /// The human invocation context every UI request is admitted with.
    pub fn user(&self) -> &InvocationContext {
        &self.ctx
    }
    /// The production [`SecretResolver`] for settings secret references (the same OS secret
    /// store the provider connections use).
    pub fn secret_resolver(&self) -> Arc<dyn SecretResolver> {
        self.connections.clone()
    }
}

/// Composes the services over `store` and boots (A3). Must be awaited on a Tokio runtime with
/// timers; the tasks boot starts run there.
async fn compose(
    config: &DesktopConfig,
    sqlite: Arc<SqliteStore>,
    clock: Arc<dyn Clock>,
    ids: Arc<HostIds>,
) -> Result<Host, StartError> {
    let store: Arc<dyn ApplicationStore> = sqlite.clone();
    let id_source: Arc<dyn IdSource> = ids.clone();
    let sources = Arc::new(
        HostArtifactSources::new(clock.clone(), id_source.clone()).with_store(store.clone()),
    );
    let artifact_files = Arc::new(
        NativeArtifactFiles::new(config.data_root.clone(), config.data_root.join("cache"))
            .with_store(store.clone()),
    );
    let artifacts = Arc::new(ArtifactService::new(
        artifact_files.clone(),
        sources.clone(),
        Arc::new(NativeFilesystem::new()),
        clock.clone(),
        id_source.clone(),
    ));
    let settings = Arc::new(SettingsService {
        roles: Arc::new(HostRoles {
            store: store.clone(),
        }),
        paths: Arc::new(FsPathProbe),
        ids: id_source.clone(),
    });
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
    let connections = Arc::new(ConnectionService::new(
        id_source.clone(),
        clock.clone(),
        store.clone(),
        http.clone(),
        secrets.clone(),
        Arc::new(devworks_adapters::OsEntropy),
    ));
    let directory: Arc<dyn ConnectionDirectory> = config
        .connections
        .clone()
        .unwrap_or_else(|| connections.clone());
    // The Home coordinator agent is bound by the Home chat item (DW-16); until it exists a
    // human's Home session is conducted by its creator, and no reply is ever fabricated.
    // Unified Knowledge. Retrieval is lexical until an embedding route exists (vector search is
    // never reported as run); imports read their saved artifact version. A large delete by
    // source continues through its durable consumer, registered with the dispatcher below.
    let knowledge = Arc::new(KnowledgeService::new(
        id_source.clone(),
        clock.clone(),
        Some(artifact_files),
        Arc::new(NoVectorIndex),
    ));
    let use_cases = UseCases::new()
        .with_queue()
        .with_settings(settings)
        .with_sessions(Arc::new(SessionService::new(
            id_source.clone(),
            store.clone(),
        )))
        .with_history(Arc::new(HistoryService::new(id_source.clone())))
        // The router resolves tiers, named models, effort and availability over the provider
        // connections' durable rows (the same unit of work as each route).
        .with_router(Arc::new(InferenceRouter::new(
            id_source.clone(),
            clock.clone(),
            directory,
        )))
        .with_connections(connections.clone())
        .with_knowledge(knowledge.clone());
    let authority = Arc::new(StoreAuthority {
        store: store.clone(),
        clock: clock.clone(),
    });
    // Diagnostics read the store's own health probes; exports are staged as the user's uploads.
    let diagnostics = Arc::new(DiagnosticService::new(
        store.clone(),
        id_source.clone(),
        clock.clone(),
        Some(sqlite as Arc<dyn StorageHealthProbe>),
    ));
    diagnostics.set_export_staging(Arc::new(
        DiagnosticExports::new(
            config.data_root.join("diagnostics").join("exports"),
            sources.clone(),
        )
        .with_store(store.clone()),
    ));
    // Managed executable packages (ADDON-01..04, EXT-01..07): verified bundles install into
    // content-addressed directories under the data root; helpers launch through the process
    // owner. No optional runtime is assumed present until a runtime probe is qualified.
    let runtimes = Arc::new(RuntimeCache::new());
    let packages = Arc::new(PackageService::new(
        store.clone(),
        id_source.clone(),
        artifacts.clone(),
        Arc::new(BundleInspector),
        Arc::new(NativePackageFiles::new(config.data_root.join("packages"))),
        runtimes.clone(),
        HostPlatform {
            target_triple: host_target_triple(),
            host_version: env!("CARGO_PKG_VERSION").into(),
            protocol_major: 1,
            protocol_minor: 0,
        },
        diagnostics.clone(),
        Arc::new(JsonSchemaValidator),
    ));
    // The shared web tools: fetched pages become artifacts through the one artifact service and
    // every search and fetch is recorded through the one diagnostics service. The search
    // credential resolves through the provider connection service, i.e. the same OS secret
    // store (one secret-store abstraction).
    let web = Arc::new(WebService::new(
        store.clone(),
        artifacts.clone(),
        diagnostics.clone(),
        clock.clone(),
        id_source.clone(),
        (config.web)(connections.clone()),
    ));
    let use_cases = use_cases.with_web(web.clone());
    let purge_client = Arc::new(
        crate::project_purge::NativePurgeClient::new(
            config.data_root.join(crate::memory::FILE),
            sources.clone(),
        )
        .with_store(&*store),
    );
    let mut runtime_config = RuntimeConfig::new(ids.next_id(IdKind::Boot));
    runtime_config.purge = Some(devworks_engine::runtime::PurgeComposition {
        files: Arc::new(
            devworks_adapters::fs::purge::NativeProjectPurgeFiles::new(
                config.data_root.clone(),
                config.data_root.join("cache"),
                store.clone(),
            )
            .with_paste_root(config.data_root.join("staging-ui"))
            .with_read_fence(artifacts.clone()),
        ),
        client: purge_client.clone(),
    });
    runtime_config.shutdown_bound = config.shutdown_bound;
    runtime_config.schemas = Some(devworks_engine::schemas::SchemaComposition {
        reader: Arc::new(devworks_adapters::database::NativeMetadataReader),
        secrets: secrets.clone(),
    });
    let executables = process_executables_on_path();
    let container_executables: Vec<_> = executables
        .iter()
        .filter(|executable| matches!(executable.id.as_str(), "docker" | "podman" | "container"))
        .cloned()
        .collect();
    if !container_executables.is_empty() {
        let bindings = devworks_core::services::process::tools::ToolBindings::new(
            container_executables,
            std::iter::empty(),
        )
        .and_then(|bindings| bindings.with_credentials(config.process_credentials.clone()))
        .map_err(|reason| StartError::Thread(reason.into()))?;
        runtime_config.containers = Some(devworks_engine::runtime::ContainerComposition {
            bindings,
            filesystem: Arc::new(NativeFilesystem::new()),
            protected_roots: ProtectedRoots::new(
                Vec::new(),
                Vec::new(),
                Some(config.data_root.display().to_string()),
            ),
            secrets: secrets.clone(),
            registry_auth: Arc::new(
                devworks_adapters::containers::NativeRegistryAuth::new(
                    config.data_root.join("container-auth"),
                )
                .map_err(|reason| StartError::Thread(reason.into()))?,
            ),
            host: devworks_core::services::containers::probe::HostPlatform {
                os: std::env::consts::OS,
                arch: std::env::consts::ARCH,
                macos_major: macos_major_version(),
            },
        });
    }
    if !executables.is_empty() {
        let bindings = devworks_core::services::process::tools::ToolBindings::new(
            executables,
            std::iter::empty(),
        )
        .and_then(|bindings| bindings.with_credentials(config.process_credentials.clone()))
        .map_err(|reason| StartError::Thread(reason.into()))?;
        runtime_config.process_tools = Some(devworks_engine::runtime::ProcessToolComposition {
            bindings,
            filesystem: Arc::new(NativeFilesystem::new()),
            protected_roots: ProtectedRoots::new(
                Vec::new(),
                Vec::new(),
                Some(config.data_root.display().to_string()),
            ),
            secrets,
        });
    }
    runtime_config.providers = Some(match config.providers.clone() {
        Some(providers) => providers,
        None => devworks_engine::providers::driver::ProviderComposition::new(Arc::new(
            devworks_adapters::provider_stream::NativeStreamingHttp::new().map_err(|_| {
                StartError::Thread("native inference HTTP client could not be initialized".into())
            })?,
        )),
    });
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
                    github: Some(Arc::new(
                        devworks_core::services::git::github::GitHubClient {
                            http,
                            secrets: connections.clone(),
                        },
                    )),
                    ..devworks_core::services::git::GitConfig::default()
                },
            })
        });
    let composition = Composition {
        store: store.clone(),
        clock: clock.clone(),
        ids: id_source.clone(),
        authority: authority.clone(),
        artifacts,
        resources: Arc::new(ResourceService::new(id_source.clone(), clock.clone())),
        process_host: Arc::new(NativeProcessHost),
        // The production run-control service (stored plan approval, no coordinator launcher
        // until the agent supervisor exists): accepted work is held at shutdown and boot.
        runs: RunComposition::new(Arc::new(ControlService::new(id_source.clone()))),
        power: Arc::new(NativePower),
        diagnostics,
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
        }),
        packages: Some(PackageComposition {
            service: packages,
            launch: LaunchPorts {
                fs: Arc::new(NativeFilesystem::new()),
                roots: ProtectedRoots::new(
                    Vec::new(),
                    Vec::new(),
                    Some(config.data_root.display().to_string()),
                ),
                protected_branches: Vec::new(),
            },
            config: HostConfig::default(),
            runtimes: Some(RuntimePorts {
                cache: runtimes,
                search_path: None,
            }),
            index: Some(IndexPorts {
                transport: Arc::new(NativeIndexTransport::default()),
                verifier: Arc::new(BundleIndexVerifier),
            }),
        }),
        use_cases,
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
        consumers: vec![devworks_engine::knowledge::source_deletion_consumer(
            knowledge,
        )],
        config: runtime_config,
    };
    // Recovery and every required consumer start here, before any request below is admitted.
    let runtime = boot(composition).await.map_err(StartError::Boot)?;
    // A previous boot cannot still be producing a paste. Live draft files require the
    // client keeper snapshot; this owner only reconciles incomplete reserved writes.
    if let Err(error) = devworks_adapters::fs::client_staging::NativeClientStagingFiles::new(
        config.data_root.join("staging-ui"),
        store.clone(),
    )
    .recover_abandoned()
    {
        // Its durable Writing row continues to hold purge preparation on uncertainty.
        eprintln!("UI staging recovery remains held: {error}");
    }
    // Host bookkeeping after recovery: the local user is the human principal of the UI.
    let user = ActorId(Id::new(USER_ACTOR).expect("valid actor"));
    let (registered, name) = (user.clone(), config.user_name.clone());
    let st = store.clone();
    tokio::task::spawn_blocking(move || {
        run_write(&*st, WriteOptions::default(), move |tx| {
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
    runtime.notification_wakeup();
    Ok(Host {
        runtime: Arc::new(runtime),
        async_runtime: tokio::runtime::Handle::current(),
        ctx,
        sources,
        purge_client,
        data_root: config.data_root.clone(),
        ids,
        waker: Arc::new(Mutex::new(None)),
        authority,
        connections,
        switchable,
        web,
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
                    let host = match compose(&config, store, clock, ids).await {
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
                            if let Some(w) =
                                waker.lock().unwrap_or_else(|p| p.into_inner()).as_ref()
                            {
                                w();
                            }
                        }
                    });
                    let _ = booted_tx.send(Ok(host.clone()));
                    let _ = stop_rx.await;
                    let processes = host.runtime.shutdown().await;
                    forward.abort();
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
impl Backend for DesktopBackend {
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
    fn human_decision(&self, frame: &[u8]) -> CommandOutcome {
        self.host
            .runtime
            .record_human_decision(&self.host.ctx, frame)
    }
    fn confirm_project_purge(&self, frame: &[u8]) -> CommandOutcome {
        self.host
            .runtime
            .confirm_project_purge(&self.host.ctx, frame)
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
        *self.host.waker.lock().unwrap_or_else(|p| p.into_inner()) = Some(waker);
    }
    fn token(&self, prefix: &str) -> String {
        self.host.ids.token(prefix)
    }
    fn scratch_dir(&self) -> PathBuf {
        self.host.data_root.join("staging-ui")
    }
    fn stage_pasted_image(&self, session: Option<&Id>, png: &[u8]) -> Result<PathBuf, String> {
        devworks_adapters::fs::client_staging::NativeClientStagingFiles::new(
            self.scratch_dir(),
            self.host.runtime.store().clone(),
        )
        .write(session, &self.host.ids.next_id(IdKind::Artifact), png)
    }
    fn stage_project_text(
        &self,
        project: Option<&devworks_core::protocol::ProjectId>,
        extension: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, String> {
        devworks_adapters::fs::client_staging::NativeClientStagingFiles::new(
            self.scratch_dir(),
            self.host.runtime.store().clone(),
        )
        .write_project(
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
