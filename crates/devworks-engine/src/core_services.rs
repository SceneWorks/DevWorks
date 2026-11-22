//! The one place the engine constructs its core services (code review F-042).
//!
//! A host composes [`crate::Composition`] from ports (storage, clocks, files, adapters, network
//! and process facilities) and never from finished services. [`CoreServices::build`] is the only
//! function that turns those ports into the artifact, resource, run control, diagnostics,
//! knowledge, agent, web, package, settings, provider connection, router, session, history and
//! MCP client services; [`crate::boot`] calls it once and passes the finished set to everything
//! it wires. A host or a fixture that needs one of those services reads it from the
//! [`crate::Runtime`] after boot (`artifacts()`, `control()`, `resources()`, `diagnostics()`,
//! `knowledge()`, `agents()`, `web()`, `connections()`).
//!
//! The engine owns the use-case families of these services. A composition that registers one of
//! them whole is refused with [`crate::BootError::Composition`] ([`owned_family`]); a
//! composition that scripts a single operation of one keeps its handler over the engine's.
use crate::runs::ActivationLauncher;
use devworks_core::authority::reads::VerifiedReader;
use devworks_core::ports::diagnostics::{ExportStaging, StorageHealthProbe};
use devworks_core::ports::files::{ArtifactFiles, ArtifactSources};
use devworks_core::ports::http::{Entropy, HttpTransport};
use devworks_core::ports::packages::{
    HostPlatform, PackageFiles, PackageInspector, RuntimeProbe, SchemaValidator,
};
use devworks_core::ports::router::ConnectionDirectory;
use devworks_core::ports::secrets::{SecretResolver, SecretStore};
use devworks_core::ports::storage::ApplicationStore;
use devworks_core::ports::{Clock, IdSource};
use devworks_core::protocol::ActorId;
use devworks_core::services::UseCases;
use devworks_core::services::agents::AgentService;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::diagnostics::DiagnosticService;
use devworks_core::services::history::HistoryService;
use devworks_core::services::knowledge::KnowledgeService;
use devworks_core::services::mcp::McpClientService;
use devworks_core::services::packages::PackageService;
use devworks_core::services::projects::managed_files::ManagedFileCustody;
use devworks_core::services::resources::ResourceService;
use devworks_core::services::router::InferenceRouter;
use devworks_core::services::router::connections::ConnectionService;
use devworks_core::services::router::connections::profiles::AuthEndpoints;
use devworks_core::services::runs::scope::ExecutionScope;
use devworks_core::services::runs::{ControlConfig, ControlService};
use devworks_core::services::sessions::SessionService;
use devworks_core::services::web::{WebPorts, WebService};
use devworks_core::settings::service::{PathProbe, PrincipalRoles, SettingsService};
use std::sync::Arc;
use std::time::Duration;

/// What the artifact service is built over: its blob and derived-file store, the source
/// resolver of uploads and paths, the verified reader of source files, and the custody that
/// fences project-scoped preview files (none: those previews stay uncached).
#[derive(Clone)]
pub struct ArtifactComposition {
    pub files: Arc<dyn ArtifactFiles>,
    pub sources: Arc<dyn ArtifactSources>,
    pub reader: Arc<dyn VerifiedReader>,
    pub project_previews: Option<Arc<ManagedFileCustody>>,
}
impl ArtifactComposition {
    pub fn new(
        files: Arc<dyn ArtifactFiles>,
        sources: Arc<dyn ArtifactSources>,
        reader: Arc<dyn VerifiedReader>,
    ) -> Self {
        Self {
            files,
            sources,
            reader,
            project_previews: None,
        }
    }
}

/// What the diagnostics service is built over: the store's own health probe, the staging of
/// exported bundles and the resolved secret values every record and export has removed. All are
/// optional; without a probe or staging the matching checks report unavailable.
#[derive(Clone, Default)]
pub struct DiagnosticComposition {
    pub storage: Option<Arc<dyn StorageHealthProbe>>,
    pub export_staging: Option<Arc<dyn ExportStaging>>,
    pub secrets: Vec<String>,
}

/// What the knowledge service is built over: the artifact files its imports read (none: imports
/// are refused). Retrieval is lexical until an embedding route exists.
#[derive(Clone, Default)]
pub struct KnowledgeComposition {
    pub files: Option<Arc<dyn ArtifactFiles>>,
}

/// The ports of run root control: the plan and readiness port (none: the production one) and
/// the retry policy (none: the default), the coordinator launcher (none until the agent
/// supervisor exists: accepted activations then stay pending and are held at the next boot),
/// further lifecycle hooks ([`crate::boot`] always composes the schedules' own hooks first) and
/// the scheduled-inference dispatch port (none until the coordinator turn pipeline exists: a
/// scheduled inference run then fails before dispatch with a visible reason).
#[derive(Clone)]
pub struct RunComposition {
    pub scope: Option<Arc<dyn ExecutionScope>>,
    pub config: Option<ControlConfig>,
    pub launcher: Option<Arc<dyn ActivationLauncher>>,
    pub hooks: Arc<dyn devworks_core::services::runs::recovery::LifecycleHooks>,
    pub inference: Arc<dyn devworks_core::services::jobs::runner::InferenceJobs>,
}
impl Default for RunComposition {
    fn default() -> Self {
        Self {
            scope: None,
            config: None,
            launcher: None,
            hooks: Arc::new(devworks_core::services::runs::recovery::NoLifecycleHooks),
            inference: Arc::new(devworks_core::services::jobs::runner::NoInferencePipeline),
        }
    }
}

/// The ports of the package service. The package host's own ports stay on
/// [`crate::PackageComposition`].
#[derive(Clone)]
pub struct PackageServicePorts {
    pub inspector: Arc<dyn PackageInspector>,
    pub files: Arc<dyn PackageFiles>,
    /// Runtime availability reads (the cache the host's runtime probing fills).
    pub runtimes: Arc<dyn RuntimeProbe>,
    pub platform: HostPlatform,
    pub validator: Arc<dyn SchemaValidator>,
    /// Bound of the wait for a package's invocations to finish before it is changed
    /// (none: the service default).
    pub quiesce_wait: Option<Duration>,
    /// Consumers whose availability depends on installed packages.
    pub availability_consumers: Vec<String>,
    /// The identifier source of the package service's own log records (none: the composition's,
    /// through the one diagnostics service). A host that must tell package logs apart supplies
    /// its own; the records still go to the same store.
    pub log_ids: Option<Arc<dyn IdSource>>,
}

/// Builds the web tools' network ports over the secret resolver of the provider connection
/// service (so `web.searchCredential` resolves from the same secret store as the provider
/// credentials). [`CoreServices::build`] calls it once with the connection service it built.
pub type WebPortsFactory = Arc<dyn Fn(Arc<dyn SecretResolver>) -> WebPorts + Send + Sync>;

/// What the settings service is built over: who holds which role and what the filesystem holds.
#[derive(Clone)]
pub struct SettingsPorts {
    pub roles: Arc<dyn PrincipalRoles>,
    pub paths: Arc<dyn PathProbe>,
}

/// What the provider connection service is built over: outbound HTTP, the secret facility, the
/// random generator and, for a test that points sign-in at a local fake, the OAuth endpoints
/// (none: the providers').
#[derive(Clone)]
pub struct ConnectionPorts {
    pub http: Arc<dyn HttpTransport>,
    pub secrets: Arc<dyn SecretStore>,
    pub entropy: Arc<dyn Entropy>,
    pub endpoints: Option<AuthEndpoints>,
}

/// What the router reads connection states from: the directory port (none: the durable rows of
/// the provider connection service, which must then be composed).
#[derive(Clone, Default)]
pub struct RouterPorts {
    pub directory: Option<Arc<dyn ConnectionDirectory>>,
}

/// The coordinator that conducts a human's Home session (none: the creator conducts it).
#[derive(Clone, Default)]
pub struct SessionPorts {
    pub coordinator: Option<ActorId>,
}

/// The services built over the store, the clock and the identifier source alone, plus the ports
/// of those that need more. A service whose ports are absent is not built and its use case
/// family is not registered. [`Default`] composes none of them.
#[derive(Clone, Default)]
pub struct ApplicationComposition {
    pub settings: Option<SettingsPorts>,
    pub connections: Option<ConnectionPorts>,
    pub router: Option<RouterPorts>,
    pub sessions: Option<SessionPorts>,
    pub history: bool,
    /// The outgoing MCP client configuration service.
    pub mcp: bool,
}

/// Every core service of the engine, constructed once by [`CoreServices::build`].
#[derive(Clone)]
pub struct CoreServices {
    pub artifacts: Arc<ArtifactService>,
    pub resources: Arc<ResourceService>,
    pub control: Arc<ControlService>,
    pub diagnostics: Arc<DiagnosticService>,
    pub knowledge: Arc<KnowledgeService>,
    pub agents: Arc<AgentService>,
    pub web: Option<Arc<WebService>>,
    /// The ports the web service was built with (for the package host's brokered HTTP calls and
    /// the host's index transport).
    pub web_ports: Option<WebPorts>,
    pub packages: Option<Arc<PackageService>>,
    pub settings: Option<Arc<SettingsService>>,
    pub connections: Option<Arc<ConnectionService>>,
    pub router: Option<Arc<InferenceRouter>>,
    pub sessions: Option<Arc<SessionService>>,
    pub history: Option<Arc<HistoryService>>,
    pub mcp: Option<Arc<McpClientService>>,
    /// Port combinations that cannot be built; [`crate::boot`] refuses the composition.
    pub(crate) refusals: Vec<String>,
    /// The package index ports, built from the web tools' ports.
    pub(crate) package_index: Option<Arc<crate::package_host::IndexPorts>>,
}

/// The ports [`CoreServices::build`] reads.
pub struct CorePorts<'a> {
    pub store: &'a Arc<dyn ApplicationStore>,
    pub clock: &'a Arc<dyn Clock>,
    pub ids: &'a Arc<dyn IdSource>,
    pub artifacts: &'a ArtifactComposition,
    pub diagnostics: &'a DiagnosticComposition,
    pub runs: &'a RunComposition,
    pub knowledge: &'a KnowledgeComposition,
    pub application: &'a ApplicationComposition,
    pub web: Option<&'a WebPortsFactory>,
    pub packages: Option<&'a crate::PackageComposition>,
}

impl CoreServices {
    /// Constructs every core service from its ports. Nothing here reads the store, recovers or
    /// starts anything.
    pub fn build(ports: CorePorts<'_>) -> Self {
        let CorePorts {
            store,
            clock,
            ids,
            artifacts: a,
            diagnostics: d,
            runs: r,
            knowledge: k,
            application: app,
            web,
            packages,
        } = ports;
        let mut refusals = Vec::new();
        let settings = app.settings.as_ref().map(|p| {
            Arc::new(SettingsService {
                roles: p.roles.clone(),
                paths: p.paths.clone(),
                ids: ids.clone(),
            })
        });
        let connections = app.connections.as_ref().map(|p| {
            let service = ConnectionService::new(
                ids.clone(),
                clock.clone(),
                store.clone(),
                p.http.clone(),
                p.secrets.clone(),
                p.entropy.clone(),
            );
            Arc::new(match &p.endpoints {
                Some(endpoints) => service.with_endpoints(endpoints.clone()),
                None => service,
            })
        });
        let router = app.router.as_ref().and_then(|p| {
            let directory: Option<Arc<dyn ConnectionDirectory>> = match (&p.directory, &connections)
            {
                (Some(directory), _) => Some(directory.clone()),
                (None, Some(connections)) => Some(connections.clone()),
                (None, None) => None,
            };
            if directory.is_none() {
                refusals.push(
                    "the router needs a connection directory or the provider connection service"
                        .to_owned(),
                );
            }
            directory.map(|directory| {
                Arc::new(InferenceRouter::new(ids.clone(), clock.clone(), directory))
            })
        });
        let sessions = app.sessions.as_ref().map(|p| {
            let service = SessionService::new(ids.clone(), store.clone());
            Arc::new(match &p.coordinator {
                Some(coordinator) => service.with_coordinator(coordinator.clone()),
                None => service,
            })
        });
        let history = app
            .history
            .then(|| Arc::new(HistoryService::new(ids.clone())));
        let mcp = app
            .mcp
            .then(|| Arc::new(McpClientService::new(ids.clone())));
        let mut artifacts = ArtifactService::new(
            a.files.clone(),
            a.sources.clone(),
            a.reader.clone(),
            clock.clone(),
            ids.clone(),
        );
        if let Some(custody) = &a.project_previews {
            artifacts = artifacts.with_project_previews(custody.clone());
        }
        let artifacts = Arc::new(artifacts);
        let resources = Arc::new(ResourceService::new(ids.clone(), clock.clone()));
        let control = {
            let service = match &r.scope {
                Some(scope) => ControlService::with_scope(ids.clone(), scope.clone()),
                None => ControlService::new(ids.clone()),
            };
            Arc::new(match r.config {
                Some(config) => service.with_config(config),
                None => service,
            })
        };
        let diagnostics = Arc::new(DiagnosticService::new(
            store.clone(),
            ids.clone(),
            clock.clone(),
            d.storage.clone(),
        ));
        if let Some(staging) = &d.export_staging {
            diagnostics.set_export_staging(staging.clone());
        }
        for secret in &d.secrets {
            diagnostics.register_secret(secret);
        }
        // Unified Knowledge: imports read their saved artifact version, and a large delete by
        // source continues through its durable consumer, registered by boot.
        let knowledge = Arc::new(
            KnowledgeService::new(ids.clone(), clock.clone(), k.files.clone())
                .with_summaries(store.clone(), artifacts.clone()),
        );
        let agents = Arc::new(AgentService::new(ids.clone(), control.clone()));
        // The shared web tools: fetched pages become artifacts and every search and fetch is
        // recorded through the one diagnostics service.
        let web_ports = web.and_then(|factory| match &connections {
            Some(connections) => Some(factory(connections.clone())),
            None => {
                refusals.push(
                    "the web tools resolve credentials through the provider connection service"
                        .to_owned(),
                );
                None
            }
        });
        let web = web_ports.as_ref().map(|ports| {
            Arc::new(WebService::new(
                store.clone(),
                artifacts.clone(),
                diagnostics.clone(),
                clock.clone(),
                ids.clone(),
                ports.clone(),
            ))
        });
        let package_index = packages.and_then(|p| p.index.as_ref()).and_then(|factory| {
            factory(web_ports.as_ref())
                .map_err(|reason| refusals.push(reason))
                .ok()
                .map(Arc::new)
        });
        let packages = packages.map(|p| {
            let log = match &p.service.log_ids {
                Some(log_ids) => Arc::new(DiagnosticService::new(
                    store.clone(),
                    log_ids.clone(),
                    clock.clone(),
                    None,
                )),
                None => diagnostics.clone(),
            };
            let mut service = PackageService::new(
                store.clone(),
                ids.clone(),
                artifacts.clone(),
                p.service.inspector.clone(),
                p.service.files.clone(),
                p.service.runtimes.clone(),
                p.service.platform.clone(),
                log,
                p.service.validator.clone(),
            );
            if let Some(wait) = p.service.quiesce_wait {
                service.quiesce_wait = wait;
            }
            if !p.service.availability_consumers.is_empty() {
                service.set_availability_consumers(p.service.availability_consumers.clone());
            }
            Arc::new(service)
        });
        Self {
            artifacts,
            resources,
            control,
            diagnostics,
            knowledge,
            agents,
            web,
            web_ports,
            packages,
            settings,
            connections,
            router,
            sessions,
            history,
            mcp,
            refusals,
            package_index,
        }
    }

    /// The services over the same ports as a composition's, for a fixture that must drive one
    /// directly (for example to seed a store) before or beside [`crate::boot`]. It is this same
    /// [`CoreServices::build`]; only the artifact ports are given, the others are defaults. The
    /// booted runtime builds and owns its own set, readable from the [`crate::Runtime`].
    #[cfg(feature = "test-support")]
    pub fn for_fixture(
        store: &Arc<dyn ApplicationStore>,
        clock: &Arc<dyn Clock>,
        ids: &Arc<dyn IdSource>,
        artifacts: &ArtifactComposition,
    ) -> Self {
        Self::build(CorePorts {
            store,
            clock,
            ids,
            artifacts,
            diagnostics: &DiagnosticComposition::default(),
            runs: &RunComposition::default(),
            knowledge: &KnowledgeComposition::default(),
            application: &ApplicationComposition::default(),
            web: None,
            packages: None,
        })
    }

    /// Registers the families of these services over `use_cases`.
    pub(crate) fn register(&self, use_cases: UseCases) -> UseCases {
        let mut use_cases = use_cases
            .with_artifacts(self.artifacts.clone())
            .with_resources(self.resources.clone())
            .with_runs(self.control.clone())
            .with_diagnostics(self.diagnostics.clone())
            .with_knowledge(self.knowledge.clone())
            .with_agents(self.agents.clone());
        if let Some(web) = &self.web {
            use_cases = use_cases.with_web(web.clone());
        }
        if let Some(packages) = &self.packages {
            use_cases = use_cases.with_packages(packages.clone());
        }
        if let Some(settings) = &self.settings {
            use_cases = use_cases.with_settings(settings.clone());
        }
        if let Some(sessions) = &self.sessions {
            use_cases = use_cases.with_sessions(sessions.clone());
        }
        if let Some(history) = &self.history {
            use_cases = use_cases.with_history(history.clone());
        }
        if let Some(router) = &self.router {
            use_cases = use_cases.with_router(router.clone());
        }
        if let Some(connections) = &self.connections {
            use_cases = use_cases.with_connections(connections.clone());
        }
        if let Some(mcp) = &self.mcp {
            use_cases = use_cases.with_mcp(mcp.clone());
        }
        use_cases
    }
}

/// The first operation `use_cases` registers that belongs to a family the engine owns, as its
/// family and name. [`CoreServices::build`] registers every owned family itself; the only way a
/// composition scripts one of its operations is [`crate::TestOverrides`].
pub(crate) fn owned_operation(use_cases: &UseCases) -> Option<(&'static str, &'static str)> {
    use devworks_core::services::{
        AGENT_OPERATIONS, artifacts, diagnostics, history, knowledge, mcp, packages, resources,
        router, runs, sessions, web, workspace_archive,
    };
    let families: [(&'static str, &[&str]); 16] = [
        ("artifacts", &artifacts::OPERATIONS),
        ("resources", &resources::OPERATIONS),
        ("runs", &runs::OPERATIONS),
        ("diagnostics", &diagnostics::OPERATIONS),
        ("workspace archive", &workspace_archive::OPERATIONS),
        ("knowledge", &knowledge::OPERATIONS),
        ("agents", &AGENT_OPERATIONS),
        ("web", &web::OPERATIONS),
        ("packages", &packages::OPERATIONS),
        ("settings", &devworks_core::settings::service::OPERATIONS),
        ("sessions", &sessions::OPERATIONS),
        ("history", &history::OPERATIONS),
        ("router", &router::OPERATIONS),
        ("router", &[router::decisions::OPERATION]),
        ("connections", &router::connections::OPERATIONS),
        ("mcp", &mcp::OPERATIONS),
    ];
    use_cases.operations().find_map(|op| {
        families
            .iter()
            .find(|(_, operations)| operations.contains(&op))
            .map(|(family, _)| (*family, op))
    })
}
