//! The one place the engine constructs its core services (code review F-042).
//!
//! A host composes [`crate::Composition`] from ports (storage, clocks, files, adapters, network
//! and process facilities) and never from finished services. [`CoreServices::build`] is the only
//! function that turns those ports into the artifact, resource, run control, diagnostics,
//! knowledge, agent, web and package services; [`crate::boot`] calls it once and passes the
//! finished set to everything it wires. A host or a fixture that needs one of those services
//! reads it from the [`crate::Runtime`] after boot (`artifacts()`, `control()`, `resources()`,
//! `diagnostics()`, `knowledge()`, `agents()`, `web()`).
//!
//! The engine owns the use-case families of these services. A composition that registers one of
//! them itself is refused with [`crate::BootError::Composition`] ([`owned_family`]).
use crate::runs::ActivationLauncher;
use devworks_core::authority::reads::VerifiedReader;
use devworks_core::ports::diagnostics::{ExportStaging, StorageHealthProbe};
use devworks_core::ports::files::{ArtifactFiles, ArtifactSources};
use devworks_core::ports::packages::{
    HostPlatform, PackageFiles, PackageInspector, RuntimeProbe, SchemaValidator,
};
use devworks_core::ports::retrieval::{NoVectorIndex, VectorIndex};
use devworks_core::ports::storage::ApplicationStore;
use devworks_core::ports::{Clock, IdSource};
use devworks_core::services::UseCases;
use devworks_core::services::agents::AgentService;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::diagnostics::DiagnosticService;
use devworks_core::services::knowledge::KnowledgeService;
use devworks_core::services::packages::PackageService;
use devworks_core::services::projects::managed_files::ManagedFileCustody;
use devworks_core::services::resources::ResourceService;
use devworks_core::services::runs::scope::ExecutionScope;
use devworks_core::services::runs::{ControlConfig, ControlService};
use devworks_core::services::web::{WebPorts, WebService};
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
/// are refused) and the vector index (lexical retrieval until an embedding route exists).
#[derive(Clone)]
pub struct KnowledgeComposition {
    pub files: Option<Arc<dyn ArtifactFiles>>,
    pub vectors: Arc<dyn VectorIndex>,
}
impl Default for KnowledgeComposition {
    fn default() -> Self {
        Self {
            files: None,
            vectors: Arc::new(NoVectorIndex),
        }
    }
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
    pub packages: Option<Arc<PackageService>>,
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
    pub web: Option<&'a WebPorts>,
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
            web,
            packages,
        } = ports;
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
            KnowledgeService::new(
                ids.clone(),
                clock.clone(),
                k.files.clone(),
                k.vectors.clone(),
            )
            .with_summaries(store.clone(), artifacts.clone()),
        );
        let agents = Arc::new(AgentService::new(ids.clone(), control.clone()));
        // The shared web tools: fetched pages become artifacts and every search and fetch is
        // recorded through the one diagnostics service.
        let web = web.map(|ports| {
            Arc::new(WebService::new(
                store.clone(),
                artifacts.clone(),
                diagnostics.clone(),
                clock.clone(),
                ids.clone(),
                ports.clone(),
            ))
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
            packages,
        }
    }

    /// The services over the same ports as a composition's, for a fixture that must drive one
    /// directly (for example to seed a store) before or beside [`crate::boot`]. It is this same
    /// [`CoreServices::build`]; only the artifact ports are given, the others are defaults. The
    /// booted runtime builds and owns its own set, readable from the [`crate::Runtime`].
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
        use_cases
    }
}

/// The first family the engine owns that `use_cases` already registers, if any.
pub(crate) fn owned_family(use_cases: &UseCases) -> Option<&'static str> {
    use devworks_core::services::{
        AGENT_OPERATIONS, artifacts, diagnostics, knowledge, packages, resources, runs, web,
    };
    let families: [(&'static str, &[&str]); 8] = [
        ("artifacts", &artifacts::OPERATIONS),
        ("resources", &resources::OPERATIONS),
        ("runs", &runs::OPERATIONS),
        ("diagnostics", &diagnostics::OPERATIONS),
        ("knowledge", &knowledge::OPERATIONS),
        ("agents", &AGENT_OPERATIONS),
        ("web", &web::OPERATIONS),
        ("packages", &packages::OPERATIONS),
    ];
    families.into_iter().find_map(|(name, operations)| {
        operations
            .iter()
            .any(|op| use_cases.has_command(op) || use_cases.has_query(op))
            .then_some(name)
    })
}
