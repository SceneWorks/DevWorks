//! Managed executable packages end to end over the real store, runtime, process owner and a
//! protocol-speaking fixture helper (`devworks-package-fixture`) inside signed test bundles:
//! installation and verification, exact-version approval, publisher trust, quiescent
//! publication, state generations and guarded rollback, availability, the process protocol and
//! its failure handling, the broker's event-handler rule, disable/uninstall and boot recovery.
//! No test runs inference or touches the network. These tests spawn processes, so they run
//! serialized in their own binary.
mod common;
use common::*;
use devworks_adapters::fs::NativeFilesystem;
use devworks_adapters::fs::artifacts::NativeArtifactFiles;
use devworks_adapters::packages::{BundleIndexVerifier, JsonSchemaValidator, write_index};
use devworks_adapters::packages::{
    BundleInspector, NativePackageFiles, host_target_triple, key_fingerprint, write_bundle,
};
use devworks_adapters::power::NativePower;
use devworks_adapters::process::NativeProcessHost;
use devworks_core::authority::destructive::{FileIdentity, ProtectedRoots};
use devworks_core::authority::reads::ReadRoot;
use devworks_core::authority::service::bind_principal;
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::files::{ArtifactSources, ResolvedSource, SourceOrigin, SourceRefusal};
use devworks_core::ports::packages::HostPlatform;
use devworks_core::ports::packages::{
    BundleFile, IndexTransport, InspectedFile, PackageFileError, PackageFiles, RuntimeCache,
    RuntimeObservation,
};
use devworks_core::ports::storage::*;
use devworks_core::ports::{IdKind, IdSource};
use devworks_core::protocol::Id;
use devworks_core::protocol::data::ArtifactSource;
use devworks_core::protocol::records::{PackageAvailability, PackageRecord, ProposalRecord};
use devworks_core::protocol::results::{
    DomainCommandReply, DomainDecision, DomainQueryReply, OperationResult,
};
use devworks_core::protocol::{ActorId, CommandReply, DomainErrorCode, ProjectId};
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::diagnostics::DiagnosticService;
use devworks_core::services::packages::index::AVAILABILITY_CHANGED;
use devworks_core::services::packages::{
    HelperKey, MAX_PACKAGES, PROJECT_ENABLEMENT_SCHEMA, PackageService, UPDATE_POLICY_SCHEMA,
    host_schema_ref,
};
use devworks_core::services::resources::ResourceService;
use devworks_core::settings::catalog::AgentRole;
use devworks_core::settings::catalog::{PACKAGE_INDEX_SOURCES_KEY, descriptor};
use devworks_core::settings::model;
use devworks_engine::package::PackageInvocation;
use devworks_engine::package_host::discovery::resolve_command;
use devworks_engine::package_host::{
    HostConfig, IndexPorts, InstanceStatus, InvocationKind, InvokeOutcome, InvokeRefusal,
    InvokeRequest, LaunchPorts, RuntimePorts,
};
use devworks_engine::{Composition, PackageComposition, Runtime, RuntimeConfig, boot};
use devworks_extension_protocol::package::{InvocationOutcome, InvocationScope};
use devworks_extension_protocol::scalars as wire;
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How long one test may hold the binary's test slot before its watchdog fails the binary.
const WATCHDOG: Duration = Duration::from_secs(300);
/// How long a wait for a durable package state fails after (failure-only: a passing run never
/// waits for it). A loaded machine launches each helper slowly (files re-verified, process
/// spawned, handshake), and a restart budget's failure takes several launches.
const PATIENCE: Duration = Duration::from_secs(60);

/// The binary's one test slot (the tests spawn processes, so they run one at a time), with a
/// watchdog: a test still holding it after [`WATCHDOG`] aborts the binary, naming the test,
/// instead of blocking every later test until the job's timeout.
struct Serial {
    _slot: MutexGuard<'static, ()>,
    done: Arc<(Mutex<bool>, Condvar)>,
}
impl Drop for Serial {
    fn drop(&mut self) {
        *self.done.0.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.done.1.notify_all();
    }
}
fn serial() -> Serial {
    static SERIAL: Mutex<()> = Mutex::new(());
    let slot = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let done = Arc::new((Mutex::new(false), Condvar::new()));
    let (watched, test) = (
        done.clone(),
        std::thread::current().name().unwrap_or("a test").to_owned(),
    );
    std::thread::spawn(move || {
        let (finished, cv) = &*watched;
        let finished = cv
            .wait_timeout_while(
                finished.lock().unwrap_or_else(|p| p.into_inner()),
                WATCHDOG,
                |done| !*done,
            )
            .unwrap_or_else(|p| p.into_inner())
            .0;
        if !*finished {
            // Written to the stream itself: the test harness's capture would drop it.
            use std::io::Write;
            let _ = writeln!(
                std::io::stderr(),
                "{test}: still running after {WATCHDOG:?}; aborting the test binary"
            );
            std::process::abort();
        }
    });
    Serial { _slot: slot, done }
}

/// Joins `t`, failing the test when it has not finished within `bound`.
fn joined<T>(t: std::thread::JoinHandle<T>, bound: Duration, what: &str) -> T {
    let until = Instant::now() + bound;
    while !t.is_finished() {
        assert!(
            Instant::now() < until,
            "{what}: not finished within {bound:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    t.join().unwrap()
}

/// A step that, once armed for an identifier kind, holds the next allocation of that kind until
/// the test releases it. Failure-only bound: a test that never releases it (it failed first)
/// lets the step go on after a minute instead of leaving a thread blocked.
#[derive(Default)]
struct Gate {
    armed: Mutex<Option<IdKind>>,
    state: Mutex<GateState>,
    changed: Condvar,
}
#[derive(Default)]
struct GateState {
    entered: bool,
    open: bool,
    returned: bool,
}
impl Gate {
    fn arm(&self, kind: IdKind) {
        *self.armed.lock().unwrap() = Some(kind);
    }
    fn pass(&self, kind: IdKind) {
        {
            let mut armed = self.armed.lock().unwrap();
            if *armed != Some(kind) {
                return;
            }
            *armed = None;
        }
        let mut s = self.state.lock().unwrap();
        s.entered = true;
        self.changed.notify_all();
        let mut s = self
            .changed
            .wait_timeout_while(s, Duration::from_secs(60), |s| !s.open)
            .unwrap()
            .0;
        s.returned = true;
        self.changed.notify_all();
    }
    /// Waits up to `bound` for the held step to be entered.
    fn entered(&self, bound: Duration) -> bool {
        let s = self.state.lock().unwrap();
        self.changed
            .wait_timeout_while(s, bound, |s| !s.entered)
            .unwrap()
            .0
            .entered
    }
    fn returned(&self) -> bool {
        self.state.lock().unwrap().returned
    }
    fn release(&self) {
        self.state.lock().unwrap().open = true;
        self.changed.notify_all();
    }
}
/// Releases a gate when dropped (also when the test fails first).
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
/// The harness's identifiers, allocated through a gate.
struct GatedIds(Arc<TestIds>, Arc<Gate>);
impl IdSource for GatedIds {
    fn next_id(&self, kind: IdKind) -> Id {
        self.1.pass(kind);
        self.0.next_id(kind)
    }
}

const HELPER: &str = env!("CARGO_BIN_EXE_devworks-package-fixture");
/// The consumer the tests register for availability announcements.
const CODE_INTELLIGENCE: &str = "code-intelligence-test";

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

/// What a fixture bundle declares.
#[derive(Clone)]
struct Spec {
    package: String,
    version: String,
    mode: String,
    contributions: Vec<&'static str>,
    permissions: Vec<Value>,
    state_schema: String,
    targets: Vec<String>,
    dependencies: Vec<Value>,
    runtimes: Vec<Value>,
    signer: Option<u8>,
    publisher: String,
    extra: Option<(String, Vec<u8>)>,
    /// A configuration schema document the package ships as a schema asset.
    config_schema: Option<Value>,
    /// Invocations the helper multiplexes (`requestedLimits.maxConcurrent`).
    max_concurrent: u32,
}
impl Spec {
    fn new(package: &str, version: &str) -> Self {
        Self {
            package: package.into(),
            version: version.into(),
            mode: "normal".into(),
            contributions: vec![
                "echo", "wait", "sleep", "state", "event", "stubborn", "crash", "config",
            ],
            permissions: vec![
                json!({"kind": "StateWrite"}),
                json!({"kind": "WorktreeRead"}),
            ],
            state_schema: "a".repeat(64),
            targets: vec![host_target_triple()],
            dependencies: vec![],
            runtimes: vec![],
            signer: Some(1),
            publisher: "acme".into(),
            extra: None,
            config_schema: None,
            max_concurrent: 4,
        }
    }
    fn ids(&self) -> Vec<String> {
        self.contributions
            .iter()
            .map(|c| format!("{}/{c}", self.package))
            .collect()
    }
}
fn schema(id: &str, digest: &str) -> Value {
    json!({"schemaId": id, "revision": "1", "sha256": digest})
}
fn helper_bytes() -> Vec<u8> {
    static BYTES: Mutex<Option<Vec<u8>>> = Mutex::new(None);
    let mut b = BYTES.lock().unwrap();
    if b.is_none() {
        *b = Some(std::fs::read(HELPER).expect("fixture helper binary"));
    }
    b.clone().unwrap()
}
/// Manifest bytes and the bundle of `spec` (signed with its key, if any).
fn bundle(spec: &Spec) -> (Vec<u8>, String) {
    let helper = helper_bytes();
    let fixture = json!({"mode": spec.mode, "inventory": spec.ids()})
        .to_string()
        .into_bytes();
    let help = format!("# {}\n", spec.package).into_bytes();
    let entry = if cfg!(windows) {
        "bin/helper.exe"
    } else {
        "bin/helper"
    };
    let mut files: Vec<(String, Vec<u8>)> = vec![
        (entry.into(), helper),
        ("fixture.json".into(), fixture),
        ("help.md".into(), help),
    ];
    if let Some(e) = &spec.extra {
        files.push(e.clone());
    }
    let config_doc = spec
        .config_schema
        .as_ref()
        .map(|d| d.to_string().into_bytes());
    if let Some(d) = &config_doc {
        files.push(("schemas/config.json".into(), d.clone()));
    }
    let config_ref = schema(
        &format!("{}/config", spec.package),
        &config_doc
            .as_ref()
            .map_or("d".repeat(64), |d| sha256_hex(d)),
    );
    let assets: Vec<Value> = config_doc
        .iter()
        .map(|_| json!({"reference": config_ref, "path": "schemas/config.json"}))
        .collect();
    let contributions: Vec<Value> = spec
        .ids()
        .iter()
        .map(|id| {
            json!({"kind": "Tool", "meta": {"id": id,
                "inputSchema": schema(&format!("{}/input", spec.package), &"b".repeat(64)),
                "outputSchema": schema(&format!("{}/output", spec.package), &"c".repeat(64)),
                "helpTopic": "help", "required": false, "cancellation": "Cooperative"},
                "requestedEffect": "ReadCandidate"})
        })
        .collect();
    let manifest = json!({
        "packageId": spec.package, "version": spec.version, "publisherId": spec.publisher,
        "protocolMin": {"major": 1, "minor": 0}, "protocolMax": {"major": 1, "minor": 0},
        "minimumHostVersion": "0.1.0",
        "dependencies": spec.dependencies, "runtimes": spec.runtimes, "schemaAssets": assets,
        "requestedLimits": {"maxConcurrent": spec.max_concurrent, "queueCapacity": 16, "maxMemoryBytes": "1073741824", "streamCreditBytes": 65536},
        "targets": spec.targets.iter().map(|t| json!({"targetTriple": t, "entrypoint": entry})).collect::<Vec<_>>(),
        "files": files.iter().map(|(p, b)| json!({"path": p, "sha256": sha256_hex(b), "byteLength": b.len().to_string()})).collect::<Vec<_>>(),
        "stateSchema": schema(&format!("{}/state", spec.package), &spec.state_schema),
        "configurationSchema": config_ref,
        "helpFile": "help.md",
        "requestedPermissions": spec.permissions,
        "contributions": contributions,
    })
    .to_string()
    .into_bytes();
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect();
    let signer = spec.signer.map(key);
    (
        write_bundle(&manifest, &refs, signer.as_ref()),
        sha256_hex(&manifest),
    )
}

/// Upload tickets naming files beneath one read root.
struct Sources {
    root: ReadRoot,
}
impl ArtifactSources for Sources {
    fn resolve(
        &self,
        _: &InvocationContext,
        source: &ArtifactSource,
        _: Option<&ProjectId>,
    ) -> Result<ResolvedSource, SourceRefusal> {
        match source {
            ArtifactSource::Upload { ticket_id } => Ok(ResolvedSource {
                root: self.root.clone(),
                relative: format!("{}.bin", ticket_id.as_str()),
                origin: SourceOrigin::default(),
            }),
            _ => Err(SourceRefusal::NotFound),
        }
    }
}
#[cfg(unix)]
fn identity(m: &std::fs::Metadata) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    Some(FileIdentity {
        volume: m.dev(),
        file: m.ino(),
    })
}
#[cfg(not(unix))]
fn identity(_: &std::fs::Metadata) -> Option<FileIdentity> {
    None
}

/// Runtimes the tests declare available.
/// Index documents and bundles served from memory (no network in tests).
#[derive(Default)]
struct FakeTransport(
    Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
    Mutex<Option<Arc<Gate>>>,
);
impl FakeTransport {
    fn serve(&self, location: &str, bytes: Vec<u8>) {
        self.0.lock().unwrap().insert(location.into(), bytes);
    }
}
impl IndexTransport for FakeTransport {
    fn fetch(&self, location: &str, max: u64) -> Result<Vec<u8>, String> {
        let gate = self.1.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.pass(IdKind::Operation);
        }
        let b = self
            .0
            .lock()
            .unwrap()
            .get(location)
            .cloned()
            .ok_or_else(|| format!("{location}: not found"))?;
        if b.len() as u64 > max {
            return Err("too large".into());
        }
        Ok(b)
    }
}

struct App {
    h: Harness,
    rt: tokio::runtime::Runtime,
    app: Option<Runtime>,
    store: Arc<dyn ApplicationStore>,
    user: InvocationContext,
    agent: InvocationContext,
    sources: std::path::PathBuf,
    keys: AtomicUsize,
    /// The deadline the next invocations get (default 30 s).
    deadline: Mutex<Option<Duration>>,
    transport: Arc<FakeTransport>,
    runtimes: Arc<RuntimeCache>,
    /// Probe runtimes on this PATH (fake executables) instead of using a fixed cache.
    probe_path: Option<String>,
    /// The install-directory port, when a test instruments it.
    files: Option<Arc<dyn PackageFiles>>,
    /// The host's restart budget per instance key (`config()` otherwise).
    restart_limit: Option<u32>,
    host_tick: Option<Duration>,
    probe_timeout: Option<Duration>,
    process_host: Option<Arc<dyn devworks_core::ports::process::ProcessHost>>,
    retry_backoff: Option<Duration>,
    /// The runtime's one shutdown bound (10 s otherwise).
    shutdown_bound: Option<Duration>,
    /// Gates the identifiers the package host allocates (invocations).
    host_ids: Arc<Gate>,
    /// Gates the identifiers of the package service's logs.
    log_ids: Arc<Gate>,
}
fn config() -> HostConfig {
    HostConfig {
        handshake: Duration::from_millis(1500),
        cancel_grace: Duration::from_millis(500),
        admission_wait: Duration::from_secs(10),
        restart_limit: 2,
        restart_backoff: Duration::from_millis(20),
        stderr_tail: 4096,
        max_concurrency: 16,
        tick: Duration::from_millis(50),
        runtime_timeout: Duration::from_millis(1500),
        runtime_interval: Duration::from_secs(3600),
        index_interval: Duration::from_secs(3600),
        healthy_after: Duration::from_secs(5),
    }
}
fn seed_project(h: &Harness) {
    let c = rusqlite::Connection::open(h.root.join("workspace.sqlite")).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c.execute_batch(
        "INSERT OR IGNORE INTO projects(id,title,lifecycle,revision,scope_revision,created_at) VALUES('project-a','A','Active',1,1,0);",
    )
    .unwrap();
}
impl App {
    fn new() -> Self {
        Self::over(Harness::new(), None)
    }
    fn probing(path: &std::path::Path) -> Self {
        Self::over(Harness::new(), Some(path.display().to_string()))
    }
    fn with_files(files: impl FnOnce(&Harness) -> Arc<dyn PackageFiles>) -> Self {
        let h = Harness::new();
        let f = files(&h);
        Self::build(h, None, Some(f))
    }
    fn over(h: Harness, probe_path: Option<String>) -> Self {
        Self::build(h, probe_path, None)
    }
    fn build(h: Harness, probe_path: Option<String>, files: Option<Arc<dyn PackageFiles>>) -> Self {
        let mut me = Self::unbooted(h, probe_path, files);
        me.boot();
        me
    }
    fn unbooted(
        h: Harness,
        probe_path: Option<String>,
        files: Option<Arc<dyn PackageFiles>>,
    ) -> Self {
        let rt = runtime();
        let store = h.open();
        seed_project(&h);
        let user = principal(&*store, "user-a", PrincipalKind::Human);
        let agent = principal(&*store, "agent-a", PrincipalKind::Agent);
        let (u, b) = (
            user.clone(),
            NewPrincipalBinding {
                actor: ActorId(id("agent-a")),
                kind: PrincipalKind::Agent,
                role: Some(AgentRole::Worker),
                project_id: None,
                tool_families: None,
                bound_by: ActorId(id("user-a")),
            },
        );
        let _ = write(&*store, move |tx| {
            bind_principal(tx, &u, b, None)
                .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
        });
        let sources = h.root.with_extension("sources");
        std::fs::create_dir_all(&sources).unwrap();
        let me = Self {
            h,
            rt,
            app: None,
            store,
            user,
            agent,
            sources,
            keys: AtomicUsize::new(0),
            deadline: Mutex::new(None),
            transport: Arc::new(FakeTransport::default()),
            runtimes: Arc::new(RuntimeCache::new()),
            probe_path,
            files,
            restart_limit: None,
            host_tick: None,
            probe_timeout: None,
            process_host: None,
            retry_backoff: None,
            shutdown_bound: None,
            host_ids: Arc::default(),
            log_ids: Arc::default(),
        };
        if me.probe_path.is_none() {
            me.runtimes.set(std::collections::BTreeMap::from([(
                "dotnet".to_owned(),
                RuntimeObservation::Found("8.0.4".into()),
            )]));
        }
        me
    }
    fn boot(&mut self) {
        let composition = self.composition();
        self.app = Some(self.rt.block_on(boot(composition)).expect("boot"));
    }
    fn composition(&self) -> Composition {
        let h = &self.h;
        let store = self.store.clone();
        let location = self.sources.clone();
        let src = Arc::new(Sources {
            root: ReadRoot {
                identity: identity(&std::fs::metadata(&location).unwrap()),
                location,
            },
        });
        let artifacts = Arc::new(ArtifactService::new(
            Arc::new(NativeArtifactFiles::new(
                h.root.clone(),
                h.root.with_extension("cache"),
            )),
            src,
            Arc::new(NativeFilesystem::new()),
            h.clock(),
            h.ids.clone(),
        ));
        let diagnostics = Arc::new(DiagnosticService::new(
            store.clone(),
            h.ids.clone(),
            h.clock(),
            None,
        ));
        // The package service records its logs (in the same store) with gated identifiers.
        let package_log = Arc::new(DiagnosticService::new(
            store.clone(),
            Arc::new(GatedIds(h.ids.clone(), self.log_ids.clone())),
            h.clock(),
            None,
        ));
        let mut service = PackageService::new(
            store.clone(),
            h.ids.clone(),
            artifacts.clone(),
            Arc::new(BundleInspector),
            self.files
                .clone()
                .unwrap_or_else(|| Arc::new(NativePackageFiles::new(h.root.join("packages")))),
            self.runtimes.clone(),
            HostPlatform {
                target_triple: host_target_triple(),
                host_version: "0.1.0".into(),
                protocol_major: 1,
                protocol_minor: 0,
            },
            package_log,
            Arc::new(JsonSchemaValidator),
        );
        service.quiesce_wait = Duration::from_millis(150);
        service.set_availability_consumers(vec![CODE_INTELLIGENCE.into()]);
        let mut config_rt = RuntimeConfig::new(id("dispatcher-1"));
        config_rt.collect_interval = Duration::from_secs(3600);
        config_rt.shutdown_bound = self.shutdown_bound.unwrap_or(Duration::from_secs(10));
        Composition {
            git: None,
            browser: None,
            indexing: None,
            store: store.clone(),
            clock: h.clock(),
            // The package host allocates its invocation identifiers from these.
            ids: Arc::new(GatedIds(h.ids.clone(), self.host_ids.clone())),
            authority: Arc::new(Authority),
            artifacts,
            resources: Arc::new(ResourceService::new(h.ids.clone(), h.clock())),
            process_host: self
                .process_host
                .clone()
                .unwrap_or_else(|| Arc::new(NativeProcessHost)),
            runs: devworks_engine::RunComposition::new(Arc::new(
                devworks_core::services::runs::ControlService::new(h.ids.clone()),
            )),
            power: Arc::new(NativePower),
            diagnostics,
            packages: Some(PackageComposition {
                service: Arc::new(service),
                launch: LaunchPorts {
                    fs: Arc::new(NativeFilesystem::new()),
                    roots: ProtectedRoots::new(vec![], vec![], None),
                    protected_branches: vec![],
                },
                config: HostConfig {
                    runtime_timeout: self.probe_timeout.unwrap_or(config().runtime_timeout),
                    restart_limit: self.restart_limit.unwrap_or(config().restart_limit),
                    tick: self.host_tick.unwrap_or(config().tick),
                    restart_backoff: self.retry_backoff.unwrap_or(config().restart_backoff),
                    ..config()
                },
                runtimes: self.probe_path.as_ref().map(|p| RuntimePorts {
                    cache: self.runtimes.clone(),
                    search_path: Some(p.clone()),
                }),
                index: Some(IndexPorts {
                    transport: self.transport.clone(),
                    verifier: Arc::new(BundleIndexVerifier),
                }),
            }),
            use_cases: UseCases::new(),
            consumers: Vec::new(),
            config: config_rt,
        }
    }
    fn shutdown(&mut self) {
        if let Some(app) = self.app.take() {
            self.rt.block_on(app.shutdown());
        }
    }
    fn app(&self) -> &Runtime {
        self.app.as_ref().unwrap()
    }
    fn key(&self, prefix: &str) -> String {
        format!("{prefix}-{}", self.keys.fetch_add(1, Ordering::SeqCst))
    }
    /// Saves `bytes` as a bundle artifact of project-a as `ctx`; returns its reference.
    fn save(&self, ctx: &InvocationContext, bytes: &[u8]) -> Value {
        let ticket = self.key("t");
        std::fs::write(self.sources.join(format!("{ticket}.bin")), bytes).unwrap();
        let frame = json!({"protocol": "1.0-draft.1", "requestKey": self.key("save"), "command": {
            "kind": "Save", "projectId": "project-a", "title": "Bundle",
            "mediaType": "application/vnd.devworks.package", "source": {"kind": "Upload", "ticketId": ticket},
            "expectedDigest": sha256_hex(bytes), "expectedBytes": bytes.len().to_string()}});
        match applied(
            self.app()
                .command(ctx, "artifacts.save", frame.to_string().as_bytes())
                .unwrap(),
        ) {
            OperationResult::ArtifactsSave { value } => {
                serde_json::to_value(&value.artifact).unwrap()
            }
            other => panic!("{other:?}"),
        }
    }
    fn registry(&self) -> i64 {
        read(&*self.store, |s| s.packages().registry_revision())
    }
    fn lifecycle(&self, ctx: &InvocationContext, op: &str, command: Value) -> DomainCommandReply {
        let frame = json!({"protocol": "1.0-draft.1", "requestKey": self.key("addon"), "schemaRevision": 2, "command": command});
        self.app()
            .command(ctx, op, frame.to_string().as_bytes())
            .expect("addressed")
    }
    fn install(&self, ctx: &InvocationContext, spec: &Spec) -> DomainCommandReply {
        self.install_from(ctx, ctx, spec)
    }
    /// Installs a bundle `saver` saved (a person's download, an agent's build) as `ctx`.
    fn install_from(
        &self,
        saver: &InvocationContext,
        ctx: &InvocationContext,
        spec: &Spec,
    ) -> DomainCommandReply {
        let (b, _) = bundle(spec);
        let r = self.save(saver, &b);
        self.lifecycle(ctx, "addons.install", json!({"kind": "Install", "bundle": r, "expectedRegistryRevision": self.registry().to_string()}))
    }
    fn update(&self, ctx: &InvocationContext, spec: &Spec) -> DomainCommandReply {
        self.update_from(ctx, ctx, spec)
    }
    fn update_from(
        &self,
        saver: &InvocationContext,
        ctx: &InvocationContext,
        spec: &Spec,
    ) -> DomainCommandReply {
        let (b, _) = bundle(spec);
        let r = self.save(saver, &b);
        self.lifecycle(ctx, "addons.update", json!({"kind": "Update", "packageId": spec.package, "bundle": r, "expectedRegistryRevision": self.registry().to_string()}))
    }
    fn simple(
        &self,
        ctx: &InvocationContext,
        op: &str,
        kind: &str,
        package: &str,
    ) -> DomainCommandReply {
        self.lifecycle(ctx, op, json!({"kind": kind, "packageId": package, "expectedRegistryRevision": self.registry().to_string()}))
    }
    fn domain(&self, ctx: &InvocationContext, op: &str, body: Value) -> DomainCommandReply {
        // addons.propose returns a ProposalRecord and registers only revision 1.
        let mut frame = json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": self.key("cmd"), "body": body});
        if op != "addons.propose" {
            frame["schemaRevision"] = json!(2);
        }
        self.app()
            .command(ctx, op, frame.to_string().as_bytes())
            .expect("addressed")
    }
    fn catalog(&self, ctx: &InvocationContext) -> Vec<PackageRecord> {
        let frame = json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": "q-catalog", "schemaRevision": 2,
            "body": {"kind": "addons.catalog", "page": {"limit": 100}}});
        match self
            .app()
            .query(ctx, "addons.catalog", frame.to_string().as_bytes())
            .unwrap()
        {
            DomainQueryReply::Result {
                result: OperationResult::AddonsCatalog { value },
                ..
            } => value.items.0,
            other => panic!("{other:?}"),
        }
    }
    fn record(&self, package: &str) -> PackageRecord {
        self.catalog(&self.user)
            .into_iter()
            .find(|p| p.id.as_str() == package)
            .unwrap_or_else(|| panic!("{package} is not in the catalog"))
    }
    fn logs(&self, package: &str) -> Vec<String> {
        let frame = json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": "q-logs",
            "body": {"kind": "addons.logs", "packageId": package, "afterSequence": "0", "page": {"limit": 200}}});
        match self
            .app()
            .query(&self.user, "addons.logs", frame.to_string().as_bytes())
            .unwrap()
        {
            DomainQueryReply::Result {
                result: OperationResult::AddonsLogs { value },
                ..
            } => value.items.0.iter().map(|d| d.code.0.clone()).collect(),
            other => panic!("{other:?}"),
        }
    }
    fn invoke(
        &self,
        package: &str,
        contribution: &str,
        kind: InvocationKind,
    ) -> Result<devworks_engine::package_host::InvocationHandle, InvokeRefusal> {
        self.invoke_in(package, contribution, kind, InvocationScope::Workspace)
    }
    fn invoke_in(
        &self,
        package: &str,
        contribution: &str,
        kind: InvocationKind,
        scope: InvocationScope,
    ) -> Result<devworks_engine::package_host::InvocationHandle, InvokeRefusal> {
        self.app().packages().unwrap().invoke(InvokeRequest {
            package_id: package.into(),
            contribution_id: format!("{package}/{contribution}"),
            scope,
            input: serde_json::from_value(json!({"artifactId": "input-a", "versionId": "v1", "sha256": "e".repeat(64), "byteLength": "4", "mediaType": "application/json"})).unwrap(),
            operation_id: id(&self.key("op")),
            deadline: self.deadline.lock().unwrap().unwrap_or(Duration::from_secs(30)),
            allowed_operations: vec![],
            kind,
        })
    }
    /// Waits until `f` holds for the package's record.
    fn until(
        &self,
        package: &str,
        what: &str,
        f: impl Fn(&PackageRecord) -> bool,
    ) -> PackageRecord {
        let until = Instant::now() + PATIENCE;
        loop {
            let r = self.record(package);
            if f(&r) {
                return r;
            }
            assert!(
                Instant::now() < until,
                "{what}: {r:?}\nlogs: {:?}\ninstances: {:?}\nfailures: {}",
                self.logs(package),
                self.app().packages().unwrap().instance_status(package),
                self.app().packages().unwrap().failures(package)
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    fn ready(&self, package: &str) {
        let until = Instant::now() + PATIENCE;
        loop {
            let s = self.app().packages().unwrap().instance_status(package);
            if s.iter()
                .any(|(_, st)| *st == devworks_engine::package_host::InstanceStatus::Ready)
            {
                return;
            }
            assert!(
                Instant::now() < until,
                "{package} never became ready: {s:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    /// How many of the package's log records have `code` (durable: they stay once recorded).
    fn logged(&self, package: &str, code: &str) -> usize {
        self.logs(package).iter().filter(|c| *c == code).count()
    }
    /// Waits until the package's log has a record with `code`.
    fn until_logged(&self, package: &str, code: &str) {
        let until = Instant::now() + PATIENCE;
        while self.logged(package, code) == 0 {
            assert!(
                Instant::now() < until,
                "{package} never logged {code}: {:?}",
                self.logs(package)
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}
impl Drop for App {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn applied(reply: DomainCommandReply) -> OperationResult {
    match reply {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied { result } | DomainDecision::NoChange { result } => result,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}
fn package(reply: DomainCommandReply) -> PackageRecord {
    match applied(reply) {
        OperationResult::AddonsInstall { value }
        | OperationResult::AddonsUpdate { value }
        | OperationResult::AddonsEnable { value }
        | OperationResult::AddonsDisable { value }
        | OperationResult::AddonsRollback { value }
        | OperationResult::AddonsUninstall { value }
        | OperationResult::AddonsConfigure { value } => value,
        other => panic!("{other:?}"),
    }
}
fn refused(reply: DomainCommandReply) -> (DomainErrorCode, Option<String>) {
    match reply {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Refused { fault } => (fault.code, fault.target_kind.map(|t| t.0)),
            other => panic!("expected a refusal, got {other:?}"),
        },
        other => panic!("{other:?}"),
    }
}
fn waits(r: &PackageRecord) -> Vec<String> {
    r.pending_waits
        .as_ref()
        .map(|w| w.0.iter().map(|w| format!("{w:?}")).collect())
        .unwrap_or_default()
}
fn availability(r: &PackageRecord) -> String {
    format!("{:?}", r.availability.clone().unwrap())
}
fn digest_of(r: &PackageRecord) -> String {
    String::from(r.digest.clone())
}
fn finished(o: Option<InvokeOutcome>) -> InvocationOutcome {
    match o {
        Some(InvokeOutcome::Finished(o)) => o,
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------

#[test]
fn a_person_installs_a_signed_pack_whose_helper_handshakes_and_serves_invocations() {
    let _s = serial();
    let a = App::new();
    let before = a.registry();
    let spec = Spec::new("fixture-pack", "1.0.0");
    let rec = package(a.install(&a.user, &spec));
    assert_eq!(availability(&rec), "Ready");
    assert!(rec.enabled);
    assert_eq!(rec.trust_state.0, "ApprovedSigned");
    assert_eq!(
        rec.signer_key.as_ref().unwrap().0,
        key_fingerprint(&key(1).verifying_key())
    );
    assert_eq!(rec.state_generation.as_ref().unwrap().get(), 1);
    assert_eq!(rec.registry_revision.as_ref().unwrap().get(), before + 1);
    assert_eq!(rec.package_kind.as_ref().unwrap().0, "HarnessPackage");
    assert!(rec.host_target_supported.unwrap());
    assert_eq!(
        rec.requested_access
            .as_ref()
            .unwrap()
            .0
            .iter()
            .map(|l| l.0.clone())
            .collect::<Vec<_>>(),
        vec!["state.write", "worktree.read"]
    );
    assert_eq!(rec.contributions.as_ref().unwrap().0.len(), 8);
    a.ready("fixture-pack");

    let h = a
        .invoke("fixture-pack", "echo", InvocationKind::Ordinary)
        .unwrap();
    match finished(h.wait(Duration::from_secs(10))) {
        InvocationOutcome::Completed { artifacts } => {
            assert_eq!(artifacts[0].artifact_id.as_str(), "input-a")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(h.progress().0, vec!["working".to_owned()]);
    // An unknown contribution is refused before anything is sent.
    assert!(matches!(
        a.app().packages().unwrap().invoke(InvokeRequest {
            package_id: "fixture-pack".into(),
            contribution_id: "fixture-pack/missing".into(),
            scope: InvocationScope::Workspace,
            input: serde_json::from_value(json!({"artifactId": "i", "versionId": "v", "sha256": "e".repeat(64), "byteLength": "1", "mediaType": "a/b"})).unwrap(),
            operation_id: id("op-x"),
            deadline: Duration::from_secs(5),
            allowed_operations: vec![],
            kind: InvocationKind::Ordinary,
        }),
        Err(InvokeRefusal::NoSuchContribution)
    ));
    let logs = a.logs("fixture-pack");
    assert!(logs.contains(&"PackageActivated".to_owned()), "{logs:?}");
    assert!(logs.contains(&"PackageHelperReady".to_owned()), "{logs:?}");
    // History attributes the activation to the person.
    let events = read(&*a.store, |s| {
        s.history().page(
            &EventFilter::Aggregate {
                kind: "package".into(),
                id: "fixture-pack".into(),
            },
            0,
            50,
        )
    })
    .events;
    assert!(
        events
            .iter()
            .any(|e| e.kind == "PackageActivated" && e.actor.0.as_str() == "user-a")
    );
    // Every invocation is attributed to the exact version, generation and contribution it ran.
    let digest = digest_of(&rec);
    let until = Instant::now() + PATIENCE;
    loop {
        let events = read(&*a.store, |s| {
            s.history().page(
                &EventFilter::Aggregate {
                    kind: "package".into(),
                    id: "fixture-pack".into(),
                },
                0,
                100,
            )
        })
        .events;
        let ended = events.iter().find(|e| e.kind == "PackageInvocationEnded");
        if let Some(e) = ended {
            assert_eq!(e.payload["digest"], json!(digest));
            assert_eq!(e.payload["stateGeneration"], json!(1));
            assert_eq!(e.payload["contributionId"], json!("fixture-pack/echo"));
            assert_eq!(e.payload["outcome"], json!("Completed"));
            assert!(events.iter().any(|e| e.kind == "PackageInvoked"));
            break;
        }
        assert!(
            Instant::now() < until,
            "the invocation was never attributed"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // A second install of an installed package is a conflict; the registry is unchanged.
    let r = a.registry();
    assert_eq!(
        refused(a.install(
            &a.user,
            &Spec {
                version: "1.0.1".into(),
                ..spec.clone()
            }
        ))
        .0,
        DomainErrorCode::Conflict
    );
    assert_eq!(a.registry(), r);
}

#[test]
fn refused_bundles_change_nothing_and_say_why() {
    let _s = serial();
    let a = App::new();
    let before = a.registry();
    let spec = Spec::new("bad-pack", "1.0.0");
    let (good, _) = bundle(&spec);
    // A modified file breaks its manifest digest.
    let mut tampered = good.clone();
    let n = tampered.len();
    tampered[n - 3] ^= 0x55;
    // A valid signature by key 1 over another manifest, spliced onto this one.
    let (other, _) = bundle(&Spec {
        version: "9.9.9".into(),
        ..spec.clone()
    });
    let forged = splice_signature(&good, &other);
    for (bytes, code) in [
        (tampered, "IntegrityMismatch"),
        (forged, "InvalidSignature"),
    ] {
        let r = a.save(&a.user, &bytes);
        let reply = a.lifecycle(
            &a.user,
            "addons.install",
            json!({"kind": "Install", "bundle": r, "expectedRegistryRevision": before.to_string()}),
        );
        assert_eq!(
            refused(reply),
            (
                DomainErrorCode::InvalidArgument,
                Some("package_bundle".into())
            )
        );
        assert!(
            a.logs("bundle").contains(&code.to_owned()),
            "{code}: {:?}",
            a.logs("bundle")
        );
    }
    assert_eq!(a.registry(), before);
    assert!(a.catalog(&a.user).is_empty());
    // A stale registry revision is a revision conflict that writes nothing.
    let (b, _) = bundle(&spec);
    let r = a.save(&a.user, &b);
    let reply = a.lifecycle(&a.user, "addons.install", json!({"kind": "Install", "bundle": r, "expectedRegistryRevision": (before + 5).to_string()}));
    assert_eq!(refused(reply).0, DomainErrorCode::RevisionConflict);
    assert!(a.catalog(&a.user).is_empty());
}

/// Replaces the signature block of `bundle` with the one of `donor`.
fn splice_signature(bundle: &[u8], donor: &[u8]) -> Vec<u8> {
    let sig = |b: &[u8]| -> (usize, usize) {
        let m = u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
        let at = 12 + m;
        let s = u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize;
        (at, 4 + s)
    };
    let (at, len) = sig(bundle);
    let (dat, dlen) = sig(donor);
    let mut out = bundle[..at].to_vec();
    out.extend_from_slice(&donor[dat..dat + dlen]);
    out.extend_from_slice(&bundle[at + len..]);
    out
}

#[test]
fn agents_cannot_activate_contents_a_person_did_not_approve() {
    let _s = serial();
    let a = App::new();
    // A signed package from an untrusted publisher, installed by an agent, waits for trust.
    let spec = Spec::new("agent-pack", "1.0.0");
    let rec = package(a.install_from(&a.user, &a.agent, &spec));
    assert_eq!(availability(&rec), "NotInstalled");
    assert_eq!(waits(&rec), vec!["TrustApproval"]);
    // A bundle an agent produced is agent-authored content: it waits for exact approval.
    let built = package(a.install(
        &a.agent,
        &Spec {
            version: "1.0.3".into(),
            ..spec.clone()
        },
    ));
    assert_eq!(waits(&built), vec!["ExactVersionApproval"]);
    assert!(!rec.enabled);
    assert!(
        a.app()
            .packages()
            .unwrap()
            .instance_status("agent-pack")
            .is_empty()
    );

    // The agent proposes its own build; approval is bound to the exact digest it names.
    let authored = Spec {
        version: "1.0.1".into(),
        ..spec.clone()
    };
    let (b, digest) = bundle(&authored);
    let source = a.save(&a.agent, &b);
    let wrong = a.domain(
        &a.agent,
        "addons.propose",
        json!({"kind": "addons.propose", "source": source, "manifestDigest": "f".repeat(64)}),
    );
    assert_eq!(refused(wrong).0, DomainErrorCode::SourceChanged);
    let proposal: ProposalRecord = match applied(a.domain(&a.agent, "addons.propose", json!({"kind": "addons.propose", "source": source, "manifestDigest": digest, "basePackageId": "agent-pack"}))) {
        OperationResult::AddonsPropose { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(format!("{:?}", proposal.state), "Pending");
    assert_eq!(String::from(proposal.content_digest.clone()), digest);
    assert!(
        proposal.summary.0.contains("state.write"),
        "{}",
        proposal.summary.0
    );
    let rec = a.record("agent-pack");
    assert_eq!(rec.pending_proposal_id.as_ref().unwrap(), &proposal.id);
    // The agent installing its own proposed contents still waits for a person, even signed.
    let rec = package(a.lifecycle(&a.agent, "addons.install", json!({"kind": "Install", "bundle": source, "expectedRegistryRevision": a.registry().to_string()})));
    assert_eq!(waits(&rec), vec!["ExactVersionApproval"]);
    assert_eq!(availability(&rec), "NotInstalled");
    // An agent cannot decide a proposal.
    let svc = a.app().packages().unwrap().service().clone();
    let pid = proposal.id.clone();
    assert!(
        svc.decide_proposal(&a.agent, &pid, 1, &digest, true, "self-approve")
            .unwrap()
            .is_err()
    );

    // The person installs exactly those contents: approved, active, the proposal settled.
    let rec = package(a.lifecycle(&a.user, "addons.install", json!({"kind": "Install", "bundle": source, "expectedRegistryRevision": a.registry().to_string()})));
    assert_eq!(availability(&rec), "Ready");
    assert_eq!(digest_of(&rec), digest);
    let stored = read(&*a.store, move |s| s.packages().proposal(&pid)).unwrap();
    assert_eq!(stored.state, "Approved");
    assert_eq!(stored.decided_by.unwrap().0.as_str(), "user-a");

    // A rejected proposal never activates and leaves the active version unchanged.
    let next = Spec {
        version: "1.0.2".into(),
        ..spec.clone()
    };
    let (b2, d2) = bundle(&next);
    let src2 = a.save(&a.agent, &b2);
    let p2: ProposalRecord = match applied(a.domain(
        &a.agent,
        "addons.propose",
        json!({"kind": "addons.propose", "source": src2, "manifestDigest": d2}),
    )) {
        OperationResult::AddonsPropose { value } => value,
        other => panic!("{other:?}"),
    };
    let decided = svc
        .decide_proposal(&a.user, &p2.id, 1, &d2, false, "not needed")
        .unwrap()
        .unwrap();
    assert_eq!(decided.state, "Rejected");
    let d2c = d2.clone();
    let phase = read(&*a.store, move |s| {
        s.packages().package_version("agent-pack", &d2c)
    })
    .unwrap()
    .phase;
    assert_eq!(phase, VersionPhase::Rejected);
    let rec = a.record("agent-pack");
    assert_eq!(digest_of(&rec), digest);
    assert!(rec.pending_version.is_none());
    let again = a.lifecycle(&a.user, "addons.update", json!({"kind": "Update", "packageId": "agent-pack", "bundle": src2, "expectedRegistryRevision": a.registry().to_string()}));
    assert_eq!(
        refused(again),
        (DomainErrorCode::Conflict, Some("package_version".into()))
    );
}

#[test]
fn a_trusted_publishers_compatible_update_activates_and_anything_more_waits() {
    let _s = serial();
    let a = App::new();
    let spec = Spec::new("trusted-pack", "1.0.0");
    let rec = package(a.install(&a.user, &spec));
    a.ready("trusted-pack");
    // Only a person may trust a publisher.
    let policy = |trust: bool| {
        json!({"kind": "addons.configure", "packageId": "trusted-pack", "expectedRevision": a.record("trusted-pack").revision.get().to_string(),
        "config": {"schema": host_schema_ref(UPDATE_POLICY_SCHEMA).unwrap(), "value": {"trustPublisherUpdates": trust}}})
    };
    assert_eq!(
        refused(a.domain(&a.agent, "addons.configure", policy(true))).0,
        DomainErrorCode::PermissionDenied
    );
    let trusted = package(a.domain(&a.user, "addons.configure", policy(true)));
    assert!(trusted.publisher_trusted.unwrap());
    assert_eq!(rec.digest, a.record("trusted-pack").digest);

    // Same key, newer stable minor, same access: activates for an agent, at the boundary.
    let rec = package(a.update_from(
        &a.user,
        &a.agent,
        &Spec {
            version: "1.1.0".into(),
            ..spec.clone()
        },
    ));
    assert_eq!(rec.version.0, "1.1.0", "{rec:?}");
    assert_eq!(rec.trust_state.0, "TrustedPublisher");
    a.ready("trusted-pack");
    // Expanded access waits for access approval.
    let more = Spec {
        version: "1.2.0".into(),
        permissions: vec![
            json!({"kind": "StateWrite"}),
            json!({"kind": "WorktreeRead"}),
            json!({"kind": "Inference"}),
        ],
        ..spec.clone()
    };
    let rec = package(a.update_from(&a.user, &a.agent, &more));
    assert_eq!(
        (rec.version.0.as_str(), waits(&rec)),
        ("1.1.0", vec!["AccessApproval".to_owned()])
    );
    // Another signing key waits for trust.
    let rec = package(a.update_from(
        &a.user,
        &a.agent,
        &Spec {
            version: "1.3.0".into(),
            signer: Some(2),
            ..spec.clone()
        },
    ));
    assert_eq!(waits(&rec), vec!["TrustApproval"]);
    // A major version change needs a reviewed transition.
    let rec = package(a.update_from(
        &a.user,
        &a.agent,
        &Spec {
            version: "2.0.0".into(),
            ..spec.clone()
        },
    ));
    assert_eq!(waits(&rec), vec!["ExactVersionApproval"]);
    // Agent-authored contents need exact approval even when the trusted key signed them.
    let authored = Spec {
        version: "1.4.0".into(),
        ..spec.clone()
    };
    let (b, d) = bundle(&authored);
    let src = a.save(&a.agent, &b);
    applied(a.domain(
        &a.agent,
        "addons.propose",
        json!({"kind": "addons.propose", "source": src, "manifestDigest": d}),
    ));
    let rec = package(a.lifecycle(&a.agent, "addons.update", json!({"kind": "Update", "packageId": "trusted-pack", "bundle": src, "expectedRegistryRevision": a.registry().to_string()})));
    assert_eq!(
        (rec.version.0.as_str(), waits(&rec)),
        ("1.1.0", vec!["ExactVersionApproval".to_owned()])
    );
    // Revoked trust: compatible updates wait again.
    package(a.domain(&a.user, "addons.configure", policy(false)));
    let rec = package(a.update_from(
        &a.user,
        &a.agent,
        &Spec {
            version: "1.5.0".into(),
            ..spec.clone()
        },
    ));
    assert_eq!(waits(&rec), vec!["TrustApproval"]);
    assert_eq!(rec.version.0, "1.1.0");
}

#[test]
fn an_update_waits_for_in_flight_work_and_publishes_exactly_once_at_the_boundary() {
    let _s = serial();
    let a = App::new();
    let spec = Spec::new("busy-pack", "1.0.0");
    let v1 = package(a.install(&a.user, &spec));
    a.ready("busy-pack");
    let running = a
        .invoke("busy-pack", "wait", InvocationKind::Ordinary)
        .unwrap();
    let before = a.registry();
    let rec = package(a.update(
        &a.user,
        &Spec {
            version: "1.1.0".into(),
            ..spec.clone()
        },
    ));
    // Still busy: staged, the active version and registry are unchanged, the invocation runs.
    assert_eq!(waits(&rec), vec!["Quiescence"]);
    assert_eq!(rec.digest, v1.digest);
    assert_eq!(a.registry(), before);
    assert!(running.wait(Duration::from_millis(100)).is_none());
    // The pinned invocation finishes on its version; then the host publishes once.
    assert!(matches!(
        finished(Some(running.cancel())),
        InvocationOutcome::CancelledBeforeEffect
    ));
    let rec = a.until("busy-pack", "the staged update activates", |r| {
        r.version.0 == "1.1.0"
    });
    assert_eq!(a.registry(), before + 1);
    assert!(rec.pending_version.is_none());
    assert_eq!(rec.predecessor_version.as_ref().unwrap().0, "1.0.0");
    assert_eq!(rec.state_generation.as_ref().unwrap().get(), 2);
    a.ready("busy-pack");
    let h = a
        .invoke("busy-pack", "echo", InvocationKind::Ordinary)
        .unwrap();
    assert!(matches!(
        finished(h.wait(Duration::from_secs(10))),
        InvocationOutcome::Completed { .. }
    ));
}

#[test]
fn state_generations_carry_state_forward_and_rollback_never_loses_later_writes() {
    let _s = serial();
    let a = App::new();
    let spec = Spec::new("state-pack", "1.0.0");
    let v1 = package(a.install(&a.user, &spec));
    a.ready("state-pack");
    let h = a
        .invoke("state-pack", "state", InvocationKind::Ordinary)
        .unwrap();
    finished(h.wait(Duration::from_secs(10)));
    let (progress, _) = h.progress();
    assert_eq!(
        progress,
        vec![
            "read:StateSnapshot:1",
            "cas:StateSnapshot:2",
            "forged:PermissionDenied"
        ]
    );
    let state = |a: &App| read(&*a.store, |s| s.packages().package_state("state-pack")).unwrap();
    assert_eq!((state(&a).generation, state(&a).revision), (1, 2));

    // Same state schema: the update carries the state into generation 2.
    package(a.update(
        &a.user,
        &Spec {
            version: "1.1.0".into(),
            ..spec.clone()
        },
    ));
    a.until("state-pack", "v1.1 active", |r| r.version.0 == "1.1.0");
    assert_eq!((state(&a).generation, state(&a).revision), (2, 2));
    a.ready("state-pack");
    // A wrong expected generation is a revision conflict.
    let target = String::from(v1.digest.clone());
    let wrong = a.lifecycle(&a.user, "addons.rollback", json!({"kind": "Rollback", "packageId": "state-pack", "targetManifestDigest": target, "expectedRegistryRevision": a.registry().to_string(), "expectedStateGeneration": "1"}));
    assert_eq!(refused(wrong).0, DomainErrorCode::RevisionConflict);
    // Rollback to v1 (same schema) keeps every later write.
    let rec = package(a.lifecycle(&a.user, "addons.rollback", json!({"kind": "Rollback", "packageId": "state-pack", "targetManifestDigest": target, "expectedRegistryRevision": a.registry().to_string(), "expectedStateGeneration": "2"})));
    assert_eq!(rec.version.0, "1.0.0");
    assert_eq!((state(&a).generation, state(&a).revision), (3, 2));
    a.ready("state-pack");

    // A new state schema seeds generation 4 with the carried state; once v2 writes, a downgrade
    // to the old schema is refused.
    package(a.update(
        &a.user,
        &Spec {
            version: "2.0.0".into(),
            state_schema: "9".repeat(64),
            ..spec.clone()
        },
    ));
    let v2 = a.until("state-pack", "v2 active", |r| r.version.0 == "2.0.0");
    assert_eq!(state(&a).generation, 4);
    a.ready("state-pack");
    let h = a
        .invoke("state-pack", "state", InvocationKind::Ordinary)
        .unwrap();
    finished(h.wait(Duration::from_secs(10)));
    assert_eq!(state(&a).revision, 3);
    let back = a.lifecycle(&a.user, "addons.rollback", json!({"kind": "Rollback", "packageId": "state-pack", "targetManifestDigest": String::from(v1.digest.clone()), "expectedRegistryRevision": a.registry().to_string(), "expectedStateGeneration": v2.state_generation.unwrap().get().to_string()}));
    assert_eq!(
        refused(back),
        (DomainErrorCode::Conflict, Some("package_state".into()))
    );
    assert_eq!(a.record("state-pack").version.0, "2.0.0");
    assert_eq!(state(&a).revision, 3);
}

#[test]
fn a_new_version_that_fails_to_start_is_rolled_back_before_any_work_was_admitted() {
    let _s = serial();
    let a = App::new();
    let spec = Spec::new("flaky-pack", "1.0.0");
    package(a.install(&a.user, &spec));
    a.ready("flaky-pack");
    package(a.update(
        &a.user,
        &Spec {
            version: "1.1.0".into(),
            mode: "exit-on-activate".into(),
            ..spec.clone()
        },
    ));
    let rec = a.until("flaky-pack", "guarded rollback to 1.0.0", |r| {
        r.version.0 == "1.0.0" && matches!(r.availability, Some(PackageAvailability::Ready))
    });
    assert_eq!(rec.predecessor_version.as_ref().unwrap().0, "1.1.0");
    assert!(
        a.logs("flaky-pack")
            .contains(&"PackageRolledBack".to_owned())
    );
    a.ready("flaky-pack");

    // Without a predecessor the failure is recorded and only this package is unavailable.
    package(a.install(
        &a.user,
        &Spec {
            mode: "hang-initialize".into(),
            ..Spec::new("hung-pack", "1.0.0")
        },
    ));
    let rec = a.until("hung-pack", "startup failure recorded", |r| {
        matches!(r.availability, Some(PackageAvailability::Failed))
    });
    assert_eq!(rec.failure_code.unwrap().0, "StartupTimeout");
    for (mode, code) in [
        ("wrong-digest", "ManifestMismatch"),
        ("wrong-inventory", "InventoryMismatch"),
    ] {
        let pkg = format!("{mode}-pack");
        package(a.install(
            &a.user,
            &Spec {
                mode: mode.into(),
                ..Spec::new(&pkg, "1.0.0")
            },
        ));
        let rec = a.until(&pkg, "handshake failure recorded", |r| {
            matches!(r.availability, Some(PackageAvailability::Failed))
        });
        assert_eq!(rec.failure_code.unwrap().0, code);
    }
    // The other package still serves invocations.
    let h = a
        .invoke("flaky-pack", "echo", InvocationKind::Ordinary)
        .unwrap();
    assert!(matches!(
        finished(h.wait(Duration::from_secs(10))),
        InvocationOutcome::Completed { .. }
    ));
    // Enabling again is the explicit retry of a failed helper.
    assert!(
        package(a.simple(&a.user, "addons.enable", "Enable", "hung-pack"))
            .failure_code
            .is_none()
    );
}

#[test]
fn a_crashed_helper_leaves_unknown_outcomes_and_is_restarted() {
    let _s = serial();
    let a = App::new();
    package(a.install(&a.user, &Spec::new("crash-pack", "1.0.0")));
    a.ready("crash-pack");
    let waiting = a
        .invoke("crash-pack", "wait", InvocationKind::Ordinary)
        .unwrap();
    let crashing = a
        .invoke("crash-pack", "crash", InvocationKind::Ordinary)
        .unwrap();
    assert!(matches!(
        crashing.wait(Duration::from_secs(10)),
        Some(InvokeOutcome::Unknown(_))
    ));
    // Its multiplexed sibling ends unknown too: never a fabricated success.
    assert!(matches!(
        waiting.wait(Duration::from_secs(10)),
        Some(InvokeOutcome::Unknown(_))
    ));
    a.ready("crash-pack");
    let h = a
        .invoke("crash-pack", "echo", InvocationKind::Ordinary)
        .unwrap();
    assert!(matches!(
        finished(h.wait(Duration::from_secs(10))),
        InvocationOutcome::Completed { .. }
    ));
    assert!(a.logs("crash-pack").contains(&"HelperExited".to_owned()));
}

/// An invocation admitted to a ready instance whose session closes before it is sent (the
/// helper just ended) sent nothing: it waits for the restart within its admission wait instead
/// of being refused. The admitted invocation is held between the admission and the send while
/// the helper crashes.
#[test]
fn an_admission_racing_a_closing_session_waits_for_the_restart() {
    let _s = serial();
    let a = App::new();
    package(a.install(&a.user, &Spec::new("race-pack", "1.0.0")));
    a.ready("race-pack");
    let _release = Release(a.host_ids.clone());
    // The next invocation is held once admitted, before its identifiers are minted and it is
    // sent.
    a.host_ids.arm(IdKind::Invocation);
    let host = a.app().packages().unwrap().clone();
    let request = InvokeRequest {
        package_id: "race-pack".into(),
        contribution_id: "race-pack/echo".into(),
        scope: InvocationScope::Workspace,
        input: serde_json::from_value(json!({"artifactId": "input-a", "versionId": "v1", "sha256": "e".repeat(64), "byteLength": "4", "mediaType": "application/json"})).unwrap(),
        operation_id: id(&a.key("op")),
        deadline: Duration::from_secs(30),
        allowed_operations: vec![],
        kind: InvocationKind::Ordinary,
    };
    let racing = std::thread::spawn(move || {
        host.invoke(request)
            .map(|h| (h.session_id().to_owned(), h.wait(Duration::from_secs(10))))
    });
    assert!(
        a.host_ids.entered(Duration::from_secs(20)),
        "the invocation was never admitted"
    );
    // Meanwhile the helper ends: its session is closed once its invocations ended unknown.
    let crashing = a
        .invoke("race-pack", "crash", InvocationKind::Ordinary)
        .unwrap();
    let crashed = crashing.session_id().to_owned();
    assert!(matches!(
        crashing.wait(Duration::from_secs(10)),
        Some(InvokeOutcome::Unknown(_))
    ));
    a.host_ids.release();
    let (session, outcome) = joined(racing, Duration::from_secs(30), "the held invocation")
        .expect("admitted to the restarted helper, not refused");
    assert_ne!(session, crashed, "sent to the restarted helper");
    assert!(matches!(
        finished(outcome),
        InvocationOutcome::Completed { .. }
    ));
}

/// The runtime's shutdown waits for the package host's cooperative stop only within what is
/// left of its one bound (DW-86), and only after the live work was held for Continue: a host
/// stop that does not return (held here while recording its helper's stop) is abandoned at the
/// deadline, still blocked when shutdown returns, and the hold had committed before it began.
#[test]
fn shutdown_abandons_a_blocked_package_host_stop_at_its_bound_after_the_continue_hold() {
    let _s = serial();
    let mut a = App::unbooted(Harness::new(), None, None);
    a.shutdown_bound = Some(Duration::from_secs(5));
    a.boot();
    // One live issue root, held for manual Continue at shutdown.
    let c = rusqlite::Connection::open(a.h.root.join("workspace.sqlite")).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c.execute_batch(
        "INSERT INTO work_items VALUES('root-i','project-a','issue',NULL,NULL,NULL,'root-i',1,0,1,1,1,'{}');",
    )
    .unwrap();
    drop(c);
    let frame = json!({"protocol": "1.0-draft.1", "requestKey": a.key("start"), "command": {"kind": "runs.start",
        "rootId": "root-i", "expectedWorkRevision": "1", "expectedControlRevision": "1"}});
    applied(
        a.app()
            .command(&a.user, "runs.start", frame.to_string().as_bytes())
            .unwrap(),
    );
    let held = || {
        read(&*a.store, |s| {
            devworks_core::services::runs::active_blockers(
                s.runs(),
                &BlockerFilter::RootLevel(devworks_core::protocol::RootId(id("root-i"))),
            )
        })
        .iter()
        .any(|b| b.cause == HoldCause::RestartContinue)
    };
    assert!(!held());
    package(a.install(&a.user, &Spec::new("stuck-pack", "1.0.0")));
    a.ready("stuck-pack");
    let gate = a.log_ids.clone();
    let _release = Release(gate.clone());
    // The package service's next log, the helper's stop recorded by the host's shutdown, is
    // held until the test releases it (after shutdown returned).
    gate.arm(IdKind::Diagnostic);
    let app = a.app.take().unwrap();
    let (held_when_blocked, blocked_at_return) = std::thread::scope(|scope| {
        let watcher = scope.spawn(|| gate.entered(Duration::from_secs(30)) && held());
        a.rt.block_on(within("shutdown", app.shutdown()));
        let blocked = gate.entered(Duration::ZERO) && !gate.returned();
        (watcher.join().unwrap(), blocked)
    });
    assert!(
        held_when_blocked,
        "the work was held for Continue before the host's stop began"
    );
    assert!(
        blocked_at_return,
        "shutdown returned at its bound while the host's stop was still blocked"
    );
    assert!(held());
}

#[test]
fn a_cancellation_the_helper_ignores_terminates_its_tree_and_reports_unknown() {
    let _s = serial();
    let a = App::new();
    package(a.install(&a.user, &Spec::new("stubborn-pack", "1.0.0")));
    a.ready("stubborn-pack");
    let h = a
        .invoke("stubborn-pack", "stubborn", InvocationKind::Ordinary)
        .unwrap();
    let started = Instant::now();
    assert!(matches!(h.cancel(), InvokeOutcome::Unknown(_)));
    assert!(started.elapsed() < Duration::from_secs(5));
    // The owned tree was stopped and recorded; the package recovers with a new helper, which is
    // then the only unsettled tree.
    a.ready("stubborn-pack");
    let unsettled = read(&*a.store, |s| {
        s.resources()
            .processes(&ProcessFilter::Unsettled, None, 100)
    });
    assert_eq!(unsettled.len(), 1, "{unsettled:?}");
    a.ready("stubborn-pack");
}

#[test]
fn event_handlers_commit_only_through_commit_event() {
    let _s = serial();
    let a = App::new();
    package(a.install(&a.user, &Spec::new("event-pack", "1.0.0")));
    a.ready("event-pack");
    // A claimed delivery of the package's subscription.
    let consumer = "pkg-event-pack-sub-1";
    let e = enqueue(&*a.store, consumer, None, json!({}));
    let claimed = write(&*a.store, move |tx| {
        tx.queue().claim_ready(&ClaimRequest {
            consumer: consumer.into(),
            owner: id("package-host"),
            lease_ms: 60_000,
            limit: 1,
        })
    })
    .unwrap()
    .remove(0);
    assert_eq!(claimed.id, e.id);
    let generation = read(&*a.store, |s| s.packages().package_state("event-pack")).unwrap();
    let commit = json!({"subscriptionId": "sub-1", "eventId": claimed.event_id.0.as_str(), "expectedCheckpoint": "0",
        "stateGeneration": generation.generation.to_string(), "expectedStateRevision": generation.revision.to_string(),
        "stateProposal": {"artifactId": "state-b", "versionId": "v1", "sha256": "b".repeat(64), "byteLength": "2", "mediaType": "application/json"},
        "intents": [{"actionKey": "notify", "call": {"kind": "ArtifactRead", "artifact": {"artifactId": "n", "versionId": "v1", "sha256": "c".repeat(64), "byteLength": "1", "mediaType": "text/plain"}}}]});
    let path = std::env::temp_dir().join("devworks-fixture-event-pack-commit.json");
    std::fs::write(&path, commit.to_string()).unwrap();
    let inv = PackageInvocation {
        package_id: String::new(),
        subscription_id: id("sub-1"),
        process_session_id: String::new(),
        invocation_id: String::new(),
        invocation_token: String::new(),
        state_generation: 0,
        delivery_id: claimed.id.clone(),
        fence: claimed.fence().unwrap(),
        cancellation_epoch: claimed.cancellation_epoch,
        actor: a.store.info().host_actor,
        broker_consumer: "package-broker".into(),
    };
    let h = a
        .invoke("event-pack", "event", InvocationKind::Event(Box::new(inv)))
        .unwrap();
    finished(h.wait(Duration::from_secs(10)));
    let _ = std::fs::remove_file(&path);
    let (progress, _) = h.progress();
    let next = generation.revision + 1;
    assert_eq!(
        progress,
        vec![
            "cas:PermissionDenied".to_owned(),
            "call:PermissionDenied".to_owned(),
            format!("commit:EventCommitAccepted:{next}"),
            "replay:EventCommitAccepted:true".to_owned(),
        ]
    );
    let state = read(&*a.store, |s| s.packages().package_state("event-pack")).unwrap();
    assert_eq!(state.revision, next);
    let processed = read(&*a.store, move |s| {
        s.packages()
            .processing("event-pack", &id("sub-1"), &claimed.event_id)
    });
    assert_eq!(processed.unwrap().intents.as_array().unwrap().len(), 1);
    assert_eq!(
        read(&*a.store, |s| s.queue().status("package-broker")).ready,
        1
    );
    // An ordinary invocation may not commit events.
    let h = a
        .invoke("event-pack", "state", InvocationKind::Ordinary)
        .unwrap();
    finished(h.wait(Duration::from_secs(10)));
}

#[test]
fn disable_cancels_and_retires_and_uninstall_keeps_state_for_a_reinstall() {
    let _s = serial();
    let a = App::new();
    let spec = Spec::new("life-pack", "1.0.0");
    package(a.install(&a.user, &spec));
    a.ready("life-pack");
    let h = a
        .invoke("life-pack", "state", InvocationKind::Ordinary)
        .unwrap();
    finished(h.wait(Duration::from_secs(10)));
    let running = a
        .invoke("life-pack", "wait", InvocationKind::Ordinary)
        .unwrap();
    let rec = package(a.simple(&a.user, "addons.disable", "Disable", "life-pack"));
    assert!(!rec.enabled);
    assert_eq!(availability(&rec), "Disabled");
    // Its invocation is cancelled with the retiring helper; nothing is invented about it.
    assert!(matches!(
        running.wait(Duration::from_secs(10)),
        Some(InvokeOutcome::Unknown(_))
    ));
    let until = Instant::now() + PATIENCE;
    while !a
        .app()
        .packages()
        .unwrap()
        .instance_status("life-pack")
        .is_empty()
    {
        assert!(Instant::now() < until, "the helper was not retired");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(matches!(
        a.invoke("life-pack", "echo", InvocationKind::Ordinary),
        Err(InvokeRefusal::Unavailable(PackageAvailability::Disabled, _))
    ));
    package(a.simple(&a.user, "addons.enable", "Enable", "life-pack"));
    a.ready("life-pack");

    let digest = digest_of(&a.record("life-pack"));
    let dir = a.h.root.join("packages").join("life-pack").join(&digest);
    assert!(dir.exists());
    let rec = package(a.simple(&a.user, "addons.uninstall", "Uninstall", "life-pack"));
    assert_eq!(availability(&rec), "NotInstalled");
    let until = Instant::now() + PATIENCE;
    while dir.exists() {
        assert!(
            Instant::now() < until,
            "uninstalled files were not removed after retirement"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let kept = read(&*a.store, |s| s.packages().package_state("life-pack")).unwrap();
    assert_eq!(kept.revision, 2);
    // Reinstalling continues from the retained state in a new generation.
    package(a.install(&a.user, &spec));
    let state = read(&*a.store, |s| s.packages().package_state("life-pack")).unwrap();
    assert_eq!((state.generation, state.revision), (kept.generation + 1, 2));
}

#[test]
fn availability_reports_dependencies_platforms_and_project_enablement() {
    let _s = serial();
    let a = App::new();
    let base = Spec::new("base-pack", "1.2.0");
    let (_, base_digest) = bundle(&base);
    let dependent = Spec {
        dependencies: vec![
            json!({"packageId": "base-pack", "versionRequirement": "^1.2", "manifestDigest": base_digest, "required": true}),
        ],
        runtimes: vec![
            json!({"runtimeId": "dotnet", "versionRequirement": ">=8", "required": true}),
        ],
        ..Spec::new("dependent-pack", "1.0.0")
    };
    let rec = package(a.install(&a.user, &dependent));
    assert_eq!(availability(&rec), "MissingDependency");
    let prereqs = rec.prerequisites.unwrap().0;
    assert_eq!(
        prereqs
            .iter()
            .map(|p| (p.kind.0.as_str(), p.satisfied))
            .collect::<Vec<_>>(),
        vec![("package", false), ("runtime", true)]
    );
    assert!(
        a.app()
            .packages()
            .unwrap()
            .instance_status("dependent-pack")
            .is_empty()
    );
    package(a.install(&a.user, &base));
    a.until("dependent-pack", "the dependency is satisfied", |r| {
        matches!(r.availability, Some(PackageAvailability::Ready))
    });
    a.ready("dependent-pack");
    // A missing runtime.
    let rec = package(a.install(
        &a.user,
        &Spec {
            runtimes: vec![
                json!({"runtimeId": "node", "versionRequirement": ">=20", "required": true}),
            ],
            ..Spec::new("node-pack", "1.0.0")
        },
    ));
    assert_eq!(availability(&rec), "MissingDependency");
    // No prebuilt helper for this target: never launched.
    let rec = package(a.install(
        &a.user,
        &Spec {
            targets: vec!["riscv64gc-unknown-none-elf".into()],
            ..Spec::new("other-pack", "1.0.0")
        },
    ));
    assert_eq!(availability(&rec), "Incompatible");
    assert!(!rec.host_target_supported.unwrap());
    assert!(
        a.app()
            .packages()
            .unwrap()
            .instance_status("other-pack")
            .is_empty()
    );

    // Project enablement: disabled for project-a only.
    let body = json!({"kind": "addons.configure", "packageId": "base-pack", "expectedRevision": a.record("base-pack").revision.get().to_string(),
        "config": {"schema": host_schema_ref(PROJECT_ENABLEMENT_SCHEMA).unwrap(), "value": {"disabledProjects": ["project-a"]}}});
    package(a.domain(&a.user, "addons.configure", body));
    a.ready("base-pack");
    let scoped = InvocationScope::Project {
        project_id: wire::ProjectId(wire::Id::try_from("project-a".to_owned()).unwrap()),
    };
    assert!(matches!(
        a.invoke_in("base-pack", "echo", InvocationKind::Ordinary, scoped),
        Err(InvokeRefusal::ProjectDisabled)
    ));
    let h = a
        .invoke("base-pack", "echo", InvocationKind::Ordinary)
        .unwrap();
    assert!(matches!(
        finished(h.wait(Duration::from_secs(10))),
        InvocationOutcome::Completed { .. }
    ));
    let in_project = a
        .user
        .clone()
        .with_lineage(Some(ProjectId(id("project-a"))), None, None);
    let seen = a
        .catalog(&in_project)
        .into_iter()
        .find(|p| p.id.as_str() == "base-pack")
        .unwrap();
    assert!(!seen.enabled);
    assert_eq!(availability(&seen), "Disabled");
    // A configuration against another schema is refused.
    let body = json!({"kind": "addons.configure", "packageId": "base-pack", "expectedRevision": a.record("base-pack").revision.get().to_string(),
        "config": {"schema": schema("base-pack/other", &"d".repeat(64)), "value": {}}});
    assert_eq!(
        refused(a.domain(&a.user, "addons.configure", body)).0,
        DomainErrorCode::SchemaMismatch
    );
}

#[test]
fn boot_recovery_removes_orphans_and_restarts_helpers() {
    let _s = serial();
    let mut a = App::new();
    package(a.install(&a.user, &Spec::new("boot-pack", "1.0.0")));
    a.ready("boot-pack");
    a.shutdown();
    let root = a.h.root.join("packages");
    std::fs::create_dir_all(root.join(".staging").join("interrupted")).unwrap();
    std::fs::write(root.join(".staging").join("interrupted").join("half"), b"x").unwrap();
    let orphan = root.join("boot-pack").join("0".repeat(64));
    std::fs::create_dir_all(&orphan).unwrap();
    std::fs::write(orphan.join("helper"), b"never committed").unwrap();
    a.boot();
    assert!(!orphan.exists());
    assert!(!root.join(".staging").join("interrupted").exists());
    a.ready("boot-pack");
    let h = a
        .invoke("boot-pack", "echo", InvocationKind::Ordinary)
        .unwrap();
    assert!(matches!(
        finished(h.wait(Duration::from_secs(10))),
        InvocationOutcome::Completed { .. }
    ));
    assert_eq!(availability(&a.record("boot-pack")), "Ready");
}

#[test]
fn installed_files_are_reverified_before_every_launch() {
    let _s = serial();
    let mut a = App::new();
    let rec = package(a.install(&a.user, &Spec::new("tamper-pack", "1.0.0")));
    a.ready("tamper-pack");
    a.shutdown();
    let dir =
        a.h.root
            .join("packages")
            .join("tamper-pack")
            .join(digest_of(&rec));
    std::fs::write(
        dir.join("fixture.json"),
        b"{\"mode\":\"normal\",\"inventory\":[]}",
    )
    .unwrap();
    a.boot();
    let rec = a.until("tamper-pack", "integrity failure recorded", |r| {
        matches!(r.availability, Some(PackageAvailability::Failed))
    });
    assert_eq!(rec.failure_code.unwrap().0, "InstallIntegrity");
    assert!(
        a.app()
            .packages()
            .unwrap()
            .instance_status("tamper-pack")
            .iter()
            .all(|(_, s)| *s != devworks_engine::package_host::InstanceStatus::Ready)
    );
}

#[test]
fn the_host_enforces_deadlines_and_caps_concurrent_invocations() {
    let _s = serial();
    let a = App::new();
    package(a.install(&a.user, &Spec::new("limits-pack", "1.0.0")));
    a.ready("limits-pack");
    // A deadline the invocation outlives is cancelled by the host itself.
    *a.deadline.lock().unwrap() = Some(Duration::from_millis(200));
    let h = a
        .invoke("limits-pack", "wait", InvocationKind::Ordinary)
        .unwrap();
    assert!(matches!(
        finished(h.wait(Duration::from_secs(10))),
        InvocationOutcome::CancelledBeforeEffect
    ));
    *a.deadline.lock().unwrap() = None;
    // The manifest declares four concurrent invocations: a fifth waits for a free slot.
    let running: Vec<_> = (0..4)
        .map(|_| {
            a.invoke("limits-pack", "wait", InvocationKind::Ordinary)
                .unwrap()
        })
        .collect();
    let host = a.app().packages().unwrap().clone();
    let fifth = std::thread::spawn(move || {
        let started = Instant::now();
        let r = host.invoke(InvokeRequest {
            package_id: "limits-pack".into(),
            contribution_id: "limits-pack/echo".into(),
            scope: InvocationScope::Workspace,
            input: serde_json::from_value(json!({"artifactId": "i", "versionId": "v", "sha256": "e".repeat(64), "byteLength": "1", "mediaType": "a/b"})).unwrap(),
            operation_id: id("op-fifth"),
            deadline: Duration::from_secs(10),
            allowed_operations: vec![],
            kind: InvocationKind::Ordinary,
        });
        (
            r.map(|h| h.wait(Duration::from_secs(10)).is_some()),
            started.elapsed(),
        )
    });
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        !fifth.is_finished(),
        "a fifth invocation was admitted past the declared limit"
    );
    assert!(matches!(
        finished(Some(running[0].cancel())),
        InvocationOutcome::CancelledBeforeEffect
    ));
    let (admitted, waited) = joined(fifth, Duration::from_secs(30), "the fifth invocation");
    assert!(admitted.unwrap());
    assert!(waited >= Duration::from_millis(400));
    for r in &running[1..] {
        r.cancel();
    }
}

// ---------------------------------------------------------------------------------------------
// Available packs, runtime probes, configuration schemas and availability announcements

/// Configures the index sources directly (as a person's settings save would have).
fn set_index_sources(store: &dyn ApplicationStore, sources: Value) {
    write(store, move |tx| {
        let current = model::current_layer(tx.configuration(), &ConfigScope::Workspace)?;
        let schema = serde_json::to_value(descriptor(PACKAGE_INDEX_SOURCES_KEY).unwrap().schema_ref()).unwrap();
        let a = serde_json::from_value(json!({"key": PACKAGE_INDEX_SOURCES_KEY, "value": {"schema": schema, "value": sources}})).unwrap();
        let plan = model::plan(&current, &[a]);
        let body = model::body(&plan, &ActorId(id("user-a")), tx.now_ms(), json!({"kind": "test"}));
        tx.configuration().append_revision(&ConfigScope::Workspace, model::NAMESPACE, current.revision, model::BODY_SCHEMA_VERSION, body)?;
        Ok(())
    })
    .unwrap();
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn entry(spec: &Spec, bundle: &[u8], manifest: &str, location: &str) -> Value {
    json!({"packageId": spec.package, "version": spec.version, "manifestDigest": manifest,
        "bundleDigest": sha256_hex(bundle), "bundleBytes": bundle.len(), "targets": spec.targets,
        "kind": "HarnessPackage", "languages": [], "bundle": location})
}

#[test]
fn signed_indexes_offer_packs_that_install_through_the_inspector_and_approval_path() {
    let _s = serial();
    let a = App::new();
    // An absolute directory on this platform; index and bundle paths use its separator.
    let base = std::env::temp_dir().join(format!("devworks-test-index-{}", std::process::id()));
    let dir = base.display().to_string();
    let at = |name: &str| base.join(name).display().to_string();
    let v1 = Spec::new("index-pack", "1.0.0");
    let v2 = Spec {
        version: "1.1.0".into(),
        ..v1.clone()
    };
    let (b1, d1) = bundle(&v1);
    let (b2, d2) = bundle(&v2);
    let publisher = key(7);
    let index = json!({"publisherId": "acme", "generatedAtMs": 1, "entries": [
        entry(&v1, &b1, &d1, "index-pack-1.0.0.dwpkg"), entry(&v2, &b2, &d2, "index-pack-1.1.0.dwpkg")]});
    a.transport
        .serve(&at("devworks-index.json"), write_index(&index, &publisher));
    a.transport.serve(&at("index-pack-1.0.0.dwpkg"), b1.clone());
    // 1.1.0 is served with other bytes than the signed digest names.
    a.transport.serve(&at("index-pack-1.1.0.dwpkg"), b1.clone());
    // No source is configured by default: nothing is offered.
    let host = a.app().packages().unwrap().clone();
    assert_eq!(host.refresh_indexes(), 0);
    assert!(a.catalog(&a.user).is_empty());
    set_index_sources(
        &*a.store,
        json!([
            {"location": dir, "publicKey": hex(publisher.verifying_key().as_bytes())},
            // The same document under a key it was not signed with is refused.
            {"location": dir, "publicKey": hex(key(8).verifying_key().as_bytes())},
        ]),
    );
    assert_eq!(host.refresh_indexes(), 1);
    let offered = a.record("index-pack");
    assert_eq!(availability(&offered), "NotInstalled");
    assert_eq!(offered.trust_state.0, "Available");
    let offers = offered.available.clone().unwrap().0;
    assert_eq!(
        offers
            .iter()
            .map(|o| o.version.0.clone())
            .collect::<Vec<_>>(),
        vec!["1.1.0", "1.0.0"]
    );
    assert!(offers.iter().all(|o| o.host_target_supported && o.newer));
    let failed = read(&*a.store, |s| s.packages().indexes());
    assert_eq!(
        failed
            .iter()
            .map(|x| x.status.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        ["Failed", "Verified"].into()
    );
    assert!(a.logs("index").contains(&"IndexUnavailable".to_owned()));

    // A download that does not hash to the signed digest is refused.
    let staging = a.h.root.with_extension("downloads");
    assert!(
        host.fetch_bundle("index-pack", &d2, &staging)
            .unwrap_err()
            .contains("does not match")
    );
    assert!(
        host.fetch_bundle("index-pack", &"0".repeat(64), &staging)
            .is_err()
    );
    // The listed bundle downloads, verifies and installs as a person's install.
    let path = host.fetch_bundle("index-pack", &d1, &staging).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let r = a.save(&a.user, &bytes);
    let rec = package(a.lifecycle(&a.user, "addons.install", json!({"kind": "Install", "bundle": r, "expectedRegistryRevision": a.registry().to_string()})));
    assert_eq!(
        (rec.version.0.as_str(), availability(&rec)),
        ("1.0.0", "Ready".to_owned())
    );
    let rec = a.record("index-pack");
    let offers = rec.available.unwrap().0;
    assert_eq!(
        offers
            .iter()
            .filter(|o| o.newer)
            .map(|o| o.version.0.clone())
            .collect::<Vec<_>>(),
        vec!["1.1.0"]
    );
}

/// A fake runtime command on PATH: an executable shell script on Unix, a `.cmd` shim on
/// Windows (found through PATHEXT, run through its interpreter).
fn fake(dir: &std::path::Path, name: &str, unix: &str, windows: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = windows;
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{unix}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[cfg(windows)]
    {
        let _ = unix;
        std::fs::write(
            dir.join(format!("{name}.cmd")),
            format!("@echo off\r\n{windows}\r\n"),
        )
        .unwrap();
    }
}

#[test]
fn declared_runtimes_are_probed_as_owned_processes_and_reported_with_their_versions() {
    let _s = serial();
    let dir = std::env::temp_dir().join(format!("devworks-runtimes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    fake(&dir, "node", "echo v20.11.1", "echo v20.11.1");
    fake(&dir, "dotnet", "echo 8.0.204", "echo 8.0.204");
    fake(&dir, "swift", "exec /bin/sleep 20", ":spin\r\ngoto spin");
    fake(
        &dir,
        "cargo",
        "echo 'cargo 1.82.0 (8f40fc59f 2024-08-21)'",
        "echo cargo 1.82.0 (8f40fc59f 2024-08-21)",
    );
    fake(
        &dir,
        "rustc",
        "echo broken >&2; exit 1",
        "echo broken 1>&2\r\nexit /b 1",
    );
    fake(&dir, "zig", "echo '0.13.0'", "echo 0.13.0");
    // A file that is not a command (no execute bit; no PATHEXT extension) is not resolved.
    std::fs::write(dir.join("deno"), "not a program").unwrap();
    let a = App::probing(&dir);
    let host = a.app().packages().unwrap().clone();
    // Booting probed already; a refresh probes again (and the declared zig once declared).
    let seen = host.refresh_runtimes();
    assert_eq!(seen["node"], RuntimeObservation::Found("20.11.1".into()));
    assert_eq!(seen["dotnet"], RuntimeObservation::Found("8.0.204".into()));
    assert_eq!(seen["cargo"], RuntimeObservation::Found("1.82.0".into()));
    assert!(
        matches!(&seen["swift"], RuntimeObservation::Missing(w) if w.contains("did not answer")),
        "{:?}",
        seen["swift"]
    );
    assert!(
        matches!(&seen["rustc"], RuntimeObservation::Missing(w) if w.contains("failed")),
        "{:?}",
        seen["rustc"]
    );
    // Resolution: the shim (PATHEXT on Windows), never a non-command file.
    let path = dir.display().to_string();
    // Whatever the case of the PATHEXT entries, the name reported is the one on disk.
    for pathext in [".EXE;.CMD", ".exe;.cmd"] {
        let node = resolve_command("node", &path, Some(pathext)).unwrap();
        assert_eq!(
            node.file_name().unwrap().to_str().unwrap(),
            if cfg!(windows) { "node.cmd" } else { "node" }
        );
    }
    assert!(resolve_command("deno", &path, Some(".EXE;.CMD")).is_none());
    // A declared runtime outside the known set is probed with the generic form.
    let needs = |pkg: &str, rt: &str, req: &str| Spec {
        runtimes: vec![json!({"runtimeId": rt, "versionRequirement": req, "required": true})],
        ..Spec::new(pkg, "1.0.0")
    };
    package(a.install(&a.user, &needs("zig-pack", "zig", ">=0.12")));
    let seen = host.refresh_runtimes();
    assert_eq!(seen["zig"], RuntimeObservation::Found("0.13.0".into()));
    let ready = a.record("zig-pack");
    assert_eq!(availability(&ready), "Ready");
    let q = &ready.prerequisites.unwrap().0[0];
    assert_eq!(
        (
            format!("{:?}", q.state.clone().unwrap()),
            q.detected_version.as_ref().unwrap().0.as_str()
        ),
        ("Ready".into(), "0.13.0")
    );
    let old = package(a.install(&a.user, &needs("node-pack", "node", ">=21")));
    assert_eq!(availability(&old), "MissingDependency");
    let q = &old.prerequisites.unwrap().0[0];
    assert_eq!(
        (
            format!("{:?}", q.state.clone().unwrap()),
            q.detected_version.as_ref().unwrap().0.as_str()
        ),
        ("Incompatible".into(), "20.11.1")
    );
    let gone = package(a.install(&a.user, &needs("swift-pack", "swift", ">=5")));
    let q = &gone.prerequisites.unwrap().0[0];
    assert_eq!(format!("{:?}", q.state.clone().unwrap()), "Missing");
    assert!(q.detected_version.is_none());
    assert!(
        gone.availability_reason
            .unwrap()
            .0
            .contains("did not answer")
    );
    // Every probe ran as an owned process that is settled now.
    let unsettled = read(&*a.store, |s| {
        s.resources()
            .processes(&ProcessFilter::Unsettled, None, 100)
    });
    assert!(
        unsettled.iter().all(|p| !p.program.starts_with("swift")),
        "{unsettled:?}"
    );
    drop(a);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn configuration_values_are_validated_against_the_packages_schema_with_field_errors() {
    let _s = serial();
    let a = App::new();
    let spec = Spec {
        config_schema: Some(
            json!({"type": "object", "additionalProperties": false, "required": ["threads"],
            "properties": {"threads": {"type": "integer", "minimum": 1}, "mode": {"enum": ["fast", "thorough"]}}}),
        ),
        ..Spec::new("config-pack", "1.0.0")
    };
    let rec = package(a.install(&a.user, &spec));
    let schema = serde_json::to_value(rec.configuration_schema.clone().unwrap()).unwrap();
    let configure = |value: Value| {
        let body = json!({"kind": "addons.configure", "packageId": "config-pack",
            "expectedRevision": a.record("config-pack").revision.get().to_string(),
            "config": {"schema": schema, "value": value}});
        a.domain(&a.user, "addons.configure", body)
    };
    let reply = configure(json!({"threads": 0, "mode": "slow"}));
    let CommandReply::Recorded { receipt, .. } = &reply else {
        panic!("{reply:?}")
    };
    let DomainDecision::Refused { fault } = &receipt.decision else {
        panic!("{receipt:?}")
    };
    assert_eq!(fault.code, DomainErrorCode::InvalidArgument);
    assert_eq!(
        fault.target_kind.as_ref().unwrap().0,
        "package_configuration"
    );
    let diagnostic = fault
        .diagnostic_id
        .clone()
        .expect("the field errors' diagnostic");
    let frame = json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": "q-l",
        "body": {"kind": "addons.logs", "packageId": "config-pack", "afterSequence": "0", "page": {"limit": 200}}});
    let entries = match a
        .app()
        .query(&a.user, "addons.logs", frame.to_string().as_bytes())
        .unwrap()
    {
        DomainQueryReply::Result {
            result: OperationResult::AddonsLogs { value },
            ..
        } => value.items.0,
        other => panic!("{other:?}"),
    };
    let detail = entries
        .iter()
        .find(|d| d.id == diagnostic)
        .expect("the refusal's diagnostic");
    assert!(
        detail.message.0.contains("/threads") && detail.message.0.contains("/mode"),
        "{}",
        detail.message.0
    );
    // Nothing changed; a value the schema accepts is applied.
    assert_eq!(a.record("config-pack").revision, rec.revision);
    let applied = package(configure(json!({"threads": 4, "mode": "fast"})));
    assert!(applied.revision.get() > rec.revision.get());
    // Host-defined schemas are validated the same way.
    let body = json!({"kind": "addons.configure", "packageId": "config-pack", "expectedRevision": a.record("config-pack").revision.get().to_string(),
        "config": {"schema": host_schema_ref(PROJECT_ENABLEMENT_SCHEMA).unwrap(), "value": {"disabledProjects": "all"}}});
    let reply = a.domain(&a.user, "addons.configure", body);
    assert_eq!(refused(reply).0, DomainErrorCode::InvalidArgument);
}

fn announcements(store: &dyn ApplicationStore, package: &str) -> Vec<Value> {
    let p = package.to_owned();
    read(store, move |s| {
        s.history().page(
            &EventFilter::Aggregate {
                kind: "package_availability".into(),
                id: p,
            },
            0,
            100,
        )
    })
    .events
    .into_iter()
    .filter(|e| e.kind == AVAILABILITY_CHANGED)
    .map(|e| e.payload)
    .collect()
}

#[test]
fn availability_and_version_changes_are_announced_atomically_with_the_registry_change() {
    let _s = serial();
    let a = App::new();
    let spec = Spec::new("watched-pack", "1.0.0");
    package(a.install(&a.user, &spec));
    let first = announcements(&*a.store, "watched-pack");
    assert_eq!(first.len(), 1);
    assert_eq!(
        (
            first[0]["toAvailability"].as_str(),
            first[0]["toVersion"].as_str(),
            first[0]["reason"].as_str()
        ),
        (Some("Ready"), Some("1.0.0"), Some("addons.install"))
    );
    assert!(first[0]["fromVersion"].is_null());
    // A refused change (stale registry revision) announces nothing.
    let stale = a.lifecycle(
        &a.user,
        "addons.disable",
        json!({"kind": "Disable", "packageId": "watched-pack", "expectedRegistryRevision": "1"}),
    );
    assert_eq!(refused(stale).0, DomainErrorCode::RevisionConflict);
    assert_eq!(announcements(&*a.store, "watched-pack").len(), 1);
    // Disable: availability changes, in the same unit of work as the registry head.
    package(a.simple(&a.user, "addons.disable", "Disable", "watched-pack"));
    let all = announcements(&*a.store, "watched-pack");
    assert_eq!(all.len(), 2);
    assert_eq!(
        (
            all[1]["fromAvailability"].as_str(),
            all[1]["toAvailability"].as_str()
        ),
        (Some("Ready"), Some("Disabled"))
    );
    assert_eq!(all[1]["registryRevision"].as_i64(), Some(a.registry()));
    package(a.simple(&a.user, "addons.enable", "Enable", "watched-pack"));
    a.ready("watched-pack");
    // An update announces the old and new version.
    package(a.update(
        &a.user,
        &Spec {
            version: "1.1.0".into(),
            ..spec.clone()
        },
    ));
    a.until("watched-pack", "the update activates", |r| {
        r.version.0 == "1.1.0"
    });
    let all = announcements(&*a.store, "watched-pack");
    let last = all.last().unwrap();
    assert_eq!(
        (last["fromVersion"].as_str(), last["toVersion"].as_str()),
        (Some("1.0.0"), Some("1.1.0"))
    );
    // Uninstall announces NotInstalled.
    package(a.simple(&a.user, "addons.uninstall", "Uninstall", "watched-pack"));
    let all = announcements(&*a.store, "watched-pack");
    assert_eq!(
        all.last().unwrap()["toAvailability"].as_str(),
        Some("NotInstalled")
    );
    // Each announcement is one ordered delivery for the registered consumer.
    let status = read(&*a.store, |s| s.queue().status(CODE_INTELLIGENCE));
    assert_eq!(status.ready as usize, all.len());
    // And none is written when the unit of work that would announce it rolls back.
    let svc = a.app().packages().unwrap().service().clone();
    let before = announcements(&*a.store, "watched-pack").len();
    package(a.install(
        &a.user,
        &Spec {
            version: "2.0.0".into(),
            ..spec.clone()
        },
    ));
    let after_install = announcements(&*a.store, "watched-pack").len();
    assert_eq!(after_install, before + 1);
    let rolled: Result<(), StorageError> = write(&*a.store, move |tx| {
        tx.packages().put_observation(&StoredObservation {
            package_id: "watched-pack".into(),
            availability: "Failed".into(),
            version: None,
            digest: None,
            revision: 0,
        })?;
        svc.observe(tx, "test")?;
        Err(StorageError::InvalidRecord(
            "abandon the unit of work".into(),
        ))
    });
    assert!(rolled.is_err());
    assert_eq!(
        announcements(&*a.store, "watched-pack").len(),
        after_install
    );
}

// ---------------------------------------------------------------------------------------------
// Publication, compatibility, restart budget, retirement and configuration delivery

#[test]
fn a_quiescent_retry_announces_its_publication_before_admission_reopens() {
    let _s = serial();
    let a = App::new();
    let spec = Spec::new("retry-pack", "1.0.0");
    package(a.install(&a.user, &spec));
    a.ready("retry-pack");
    let host = a.app().packages().unwrap().clone();
    let running = a
        .invoke("retry-pack", "wait", InvocationKind::Ordinary)
        .unwrap();
    let rec = package(a.update(
        &a.user,
        &Spec {
            version: "1.1.0".into(),
            ..spec.clone()
        },
    ));
    assert_eq!(waits(&rec), vec!["Quiescence"]);
    let announced = host.publications();
    finished(Some(running.cancel()));
    a.until("retry-pack", "the staged update activates", |r| {
        r.version.0 == "1.1.0"
    });
    // The host's own activation advanced the admission epoch like a command's settlement.
    assert!(host.publications() > announced);
    a.ready("retry-pack");
    let h = a
        .invoke("retry-pack", "state", InvocationKind::Ordinary)
        .unwrap();
    assert!(matches!(
        finished(h.wait(Duration::from_secs(10))),
        InvocationOutcome::Completed { .. }
    ));
    // It ran against the new generation (its pinned generation is current).
    let (progress, _) = h.progress();
    assert!(
        progress[0].starts_with("read:StateSnapshot"),
        "{progress:?}"
    );
}

#[test]
fn a_staged_update_that_would_regress_stays_pending_until_it_is_compatible() {
    let _s = serial();
    let a = App::new();
    let spec = Spec::new("compat-pack", "1.0.0");
    package(a.install(&a.user, &spec));
    a.ready("compat-pack");
    let running = a
        .invoke("compat-pack", "wait", InvocationKind::Ordinary)
        .unwrap();
    // Compatible when staged (dotnet 8.0.4 is observed), waiting only for quiescence.
    let next = Spec {
        version: "1.1.0".into(),
        runtimes: vec![
            json!({"runtimeId": "dotnet", "versionRequirement": ">=8", "required": true}),
        ],
        ..spec.clone()
    };
    let rec = package(a.update(&a.user, &next));
    assert_eq!(waits(&rec), vec!["Quiescence"]);
    // The runtime disappears before the boundary: activating now would regress.
    a.runtimes.set(std::collections::BTreeMap::new());
    finished(Some(running.cancel()));
    let rec = a.until("compat-pack", "the retry keeps it pending", |r| {
        waits(r) == vec!["Compatibility"]
    });
    assert_eq!(rec.version.0, "1.0.0");
    assert_eq!(rec.pending_version.as_ref().unwrap().0, "1.1.0");
    assert_eq!(availability(&rec), "Ready");
    // Once the runtime is back, the next retry activates it.
    a.runtimes.set(std::collections::BTreeMap::from([(
        "dotnet".to_owned(),
        RuntimeObservation::Found("8.0.4".into()),
    )]));
    let until = Instant::now() + PATIENCE;
    while a.record("compat-pack").version.0 != "1.1.0" {
        assert!(
            Instant::now() < until,
            "the compatible update never activated"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_helper_that_dies_after_every_activation_exhausts_its_restart_budget() {
    let _s = serial();
    let a = App::new();
    // No predecessor: the failure is recorded.
    let host = a.app().packages().unwrap().clone();
    package(a.install(
        &a.user,
        &Spec {
            mode: "exit-after-ready".into(),
            ..Spec::new("dying-pack", "1.0.0")
        },
    ));
    let announced = host.publications();
    let rec = a.until("dying-pack", "the failure is recorded", |r| {
        matches!(r.availability, Some(PackageAvailability::Failed))
    });
    assert_eq!(rec.failure_code.unwrap().0, "HelperExited");
    // Recording the failure is a registry change admissions re-read.
    assert!(host.publications() > announced);
    // With a predecessor and nothing admitted: the guarded rollback restores it.
    let spec = Spec::new("relapse-pack", "1.0.0");
    package(a.install(&a.user, &spec));
    a.ready("relapse-pack");
    package(a.update(
        &a.user,
        &Spec {
            version: "1.1.0".into(),
            mode: "exit-after-ready".into(),
            ..spec.clone()
        },
    ));
    a.until("relapse-pack", "guarded rollback to 1.0.0", |r| {
        r.version.0 == "1.0.0" && matches!(r.availability, Some(PackageAvailability::Ready))
    });
    assert!(
        a.logs("relapse-pack")
            .contains(&"PackageRolledBack".to_owned())
    );
}

#[test]
fn uninstall_retires_a_pending_candidate_that_a_person_can_install_again() {
    let _s = serial();
    let a = App::new();
    let spec = Spec::new("retire-pack", "1.0.0");
    package(a.install(&a.user, &spec));
    let next = Spec {
        version: "1.1.0".into(),
        ..spec.clone()
    };
    // An agent's bundle waits for the person's exact approval.
    let rec = package(a.update(&a.agent, &next));
    assert_eq!(waits(&rec), vec!["ExactVersionApproval"]);
    let digest = String::from(rec.pending_digest.clone().unwrap());
    package(a.simple(&a.user, "addons.uninstall", "Uninstall", "retire-pack"));
    let d = digest.clone();
    let phase = read(&*a.store, move |s| {
        s.packages().package_version("retire-pack", &d)
    })
    .unwrap()
    .phase;
    assert_eq!(phase, VersionPhase::Removed);
    // A person's explicit install of those contents is not blocked by the uninstall.
    let rec = package(a.install(&a.user, &next));
    assert_eq!(digest_of(&rec), digest);
    assert_eq!(availability(&rec), "Ready");
    a.ready("retire-pack");
}

#[test]
fn the_committed_configuration_reaches_the_helper_in_activation() {
    let _s = serial();
    let a = App::new();
    let spec = Spec {
        config_schema: Some(
            json!({"type": "object", "properties": {"threads": {"type": "integer"}}}),
        ),
        ..Spec::new("cfg-pack", "1.0.0")
    };
    let rec = package(a.install(&a.user, &spec));
    a.ready("cfg-pack");
    let report = || {
        let h = a
            .invoke("cfg-pack", "config", InvocationKind::Ordinary)
            .unwrap();
        assert!(matches!(
            finished(h.wait(Duration::from_secs(10))),
            InvocationOutcome::Completed { .. }
        ));
        h.progress().0.join("")
    };
    assert_eq!(report(), "config:none");
    let schema = serde_json::to_value(rec.configuration_schema.clone().unwrap()).unwrap();
    let body = json!({"kind": "addons.configure", "packageId": "cfg-pack",
        "expectedRevision": a.record("cfg-pack").revision.get().to_string(),
        "config": {"schema": schema, "value": {"threads": 4}}});
    package(a.domain(&a.user, "addons.configure", body));
    // The reconfigured helper is a new instance that received the value before it was ready.
    a.until("cfg-pack", "the new configuration is current", |_| true);
    let seen = {
        let until = Instant::now() + PATIENCE;
        loop {
            let r = report();
            if r != "config:none" {
                break r;
            }
            assert!(
                Instant::now() < until,
                "the helper never received the configuration"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let delivered: Value = serde_json::from_str(seen.strip_prefix("config:").unwrap()).unwrap();
    assert_eq!(delivered["value"], json!({"threads": 4}));
    assert_eq!(delivered["schema"], schema);
}

fn propose(a: &App, spec: &Spec) -> (ProposalRecord, String, Value) {
    let (b, d) = bundle(spec);
    let source = a.save(&a.agent, &b);
    let body = json!({"kind": "addons.propose", "source": source, "manifestDigest": d, "basePackageId": spec.package});
    match applied(a.domain(&a.agent, "addons.propose", body)) {
        OperationResult::AddonsPropose { value } => (value, d, source),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_superseded_proposal_stays_installable_and_an_approved_one_activates_at_the_boundary() {
    let _s = serial();
    let a = App::new();
    let spec = Spec::new("proposed-pack", "1.0.0");
    package(a.install(&a.user, &spec));
    a.ready("proposed-pack");
    let first = Spec {
        version: "1.1.0".into(),
        ..spec.clone()
    };
    let second = Spec {
        version: "1.2.0".into(),
        ..spec.clone()
    };
    let (_, d1, _) = propose(&a, &first);
    let (p2, d2, _) = propose(&a, &second);
    // The newer proposal superseded the first: retired, not rejected.
    let d = d1.clone();
    let phase = read(&*a.store, move |s| {
        s.packages().package_version("proposed-pack", &d)
    })
    .unwrap()
    .phase;
    assert_eq!(phase, VersionPhase::Removed);
    // A person can still install exactly the superseded contents.
    let rec = package(a.update(&a.user, &first));
    assert_eq!(digest_of(&rec), d1);
    a.ready("proposed-pack");
    // A person's approval of the pending proposal reaches the host: it activates at the next
    // quiescent boundary (no reinstall needed).
    let svc = a.app().packages().unwrap().service().clone();
    let decided = svc
        .decide_proposal(&a.user, &p2.id, 1, &d2, true, "looks right")
        .unwrap()
        .unwrap();
    assert_eq!(decided.state, "Approved");
    let rec = a.until("proposed-pack", "the approved proposal activates", |r| {
        r.version.0 == "1.2.0"
    });
    assert_eq!(digest_of(&rec), d2);
}

#[test]
fn rollback_paths_never_restore_a_version_whose_prerequisites_are_gone() {
    let _s = serial();
    let a = App::new();
    let needs_dotnet = |pkg: &str| Spec {
        runtimes: vec![
            json!({"runtimeId": "dotnet", "versionRequirement": ">=8", "required": true}),
        ],
        ..Spec::new(pkg, "1.0.0")
    };
    // A person's rollback.
    let v1 = package(a.install(&a.user, &needs_dotnet("back-pack")));
    a.ready("back-pack");
    let v2 = package(a.update(
        &a.user,
        &Spec {
            version: "1.1.0".into(),
            runtimes: vec![],
            ..needs_dotnet("back-pack")
        },
    ));
    a.ready("back-pack");
    a.runtimes.set(std::collections::BTreeMap::new());
    let back = a.lifecycle(&a.user, "addons.rollback", json!({"kind": "Rollback", "packageId": "back-pack", "targetManifestDigest": digest_of(&v1), "expectedRegistryRevision": a.registry().to_string(), "expectedStateGeneration": v2.state_generation.unwrap().get().to_string()}));
    assert_eq!(
        refused(back),
        (
            DomainErrorCode::UnsupportedTarget,
            Some("package_compatibility".into())
        )
    );
    assert_eq!(a.record("back-pack").version.0, "1.1.0");
    // The guarded automatic rollback after a failed start: the predecessor cannot run either,
    // so the failure is recorded instead of restoring it.
    a.runtimes.set(std::collections::BTreeMap::from([(
        "dotnet".to_owned(),
        RuntimeObservation::Found("8.0.4".into()),
    )]));
    package(a.install(&a.user, &needs_dotnet("guard-pack")));
    a.ready("guard-pack");
    a.runtimes.set(std::collections::BTreeMap::new());
    package(a.update(
        &a.user,
        &Spec {
            version: "1.1.0".into(),
            runtimes: vec![],
            mode: "exit-on-activate".into(),
            ..needs_dotnet("guard-pack")
        },
    ));
    let rec = a.until("guard-pack", "the failure is recorded", |r| {
        matches!(r.availability, Some(PackageAvailability::Failed))
    });
    assert_eq!(rec.version.0, "1.1.0");
    assert!(
        !a.logs("guard-pack")
            .contains(&"PackageRolledBack".to_owned())
    );
}

// ---------------------------------------------------------------------------------------------
// Review round 2: failure accounting, broker access, obsolete pending versions, reverse pins

/// An invocation whose frame is large (128 long allowed-operation IDs, about 17 KB).
fn big_invoke(
    a: &App,
    package: &str,
    deadline: Duration,
) -> Result<devworks_engine::package_host::InvocationHandle, InvokeRefusal> {
    let ops = (0..128)
        .map(|n| wire::QualifiedId::try_from(format!("{n:0>64}/{}", "o".repeat(64))).unwrap())
        .collect();
    a.app().packages().unwrap().invoke(InvokeRequest {
        package_id: package.into(),
        contribution_id: format!("{package}/echo"),
        scope: InvocationScope::Workspace,
        input: serde_json::from_value(json!({"artifactId": "input-a", "versionId": "v1", "sha256": "e".repeat(64), "byteLength": "4", "mediaType": "application/json"})).unwrap(),
        operation_id: id(&a.key("op")),
        deadline,
        allowed_operations: ops,
        kind: InvocationKind::Ordinary,
    })
}

#[test]
fn a_helper_that_stops_reading_is_restarted_through_its_budget_and_then_recorded_failed() {
    let _s = serial();
    let a = App::new();
    package(a.install(
        &a.user,
        &Spec {
            mode: "deaf-after-ready".into(),
            max_concurrent: 16,
            ..Spec::new("deaf-pack", "1.0.0")
        },
    ));
    // Work keeps arriving: far more than most pipe buffers. Where writes block, the writer
    // stalls on the helper's input; where the pipe takes it all (its capacity is the
    // platform's), the invocations' deadlines expire and the cancellation is ignored. Either way
    // each instance is terminated and its failure counted, however the work and the restarts
    // interleave.
    let until = Instant::now() + Duration::from_secs(60);
    while !matches!(
        a.record("deaf-pack").availability,
        Some(PackageAvailability::Failed)
    ) {
        assert!(Instant::now() < until, "the failure was never recorded");
        let handles: Vec<_> = (0..16)
            .filter_map(|_| big_invoke(&a, "deaf-pack", Duration::from_secs(3)).ok())
            .collect();
        for h in &handles {
            let outcome = h.wait(Duration::from_secs(20));
            assert!(
                matches!(outcome, Some(InvokeOutcome::Unknown(_))),
                "the deaf helper's invocations end unknown: {outcome:?}"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let rec = a.record("deaf-pack");
    let code = rec.failure_code.unwrap().0;
    assert!(
        code == "HelperStalled" || code == "CancellationIgnored",
        "{code}"
    );
    // Restarted through its budget (2 instances), then never again.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(helper_ready_events(&*a.store, "deaf-pack"), 2);
}

#[test]
fn ignored_cancellations_count_against_the_restart_budget() {
    let _s = serial();
    let a = App::new();
    package(a.install(&a.user, &Spec::new("deafcancel-pack", "1.0.0")));
    for _ in 0..2 {
        a.ready("deafcancel-pack");
        let h = a
            .invoke("deafcancel-pack", "stubborn", InvocationKind::Ordinary)
            .unwrap();
        assert!(matches!(h.cancel(), InvokeOutcome::Unknown(_)));
    }
    let rec = a.until("deafcancel-pack", "the failure is recorded", |r| {
        matches!(r.availability, Some(PackageAvailability::Failed))
    });
    assert_eq!(rec.failure_code.unwrap().0, "CancellationIgnored");
}

/// Runs the fixture's `event` contribution of `package` on a fresh claimed delivery with the
/// commit `extra` adds to; returns its progress.
fn run_event(a: &App, package: &str, n: usize, extra: Value) -> Vec<String> {
    let consumer = format!("pkg-{package}-sub-{n}");
    let e = enqueue(&*a.store, &consumer, None, json!({}));
    let c = consumer.clone();
    let claimed = write(&*a.store, move |tx| {
        tx.queue().claim_ready(&ClaimRequest {
            consumer: c.clone(),
            owner: id("package-host"),
            lease_ms: 60_000,
            limit: 1,
        })
    })
    .unwrap()
    .remove(0);
    assert_eq!(claimed.id, e.id);
    let pkg = package.to_owned();
    let generation = read(&*a.store, move |s| s.packages().package_state(&pkg)).unwrap();
    let mut commit = json!({"subscriptionId": format!("sub-{n}"), "eventId": claimed.event_id.0.as_str(), "expectedCheckpoint": "0",
        "stateGeneration": generation.generation.to_string(), "expectedStateRevision": generation.revision.to_string(), "intents": []});
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        commit[k] = v;
    }
    let path = std::env::temp_dir().join(format!("devworks-fixture-{package}-commit.json"));
    std::fs::write(&path, commit.to_string()).unwrap();
    let inv = PackageInvocation {
        package_id: String::new(),
        subscription_id: id(&format!("sub-{n}")),
        process_session_id: String::new(),
        invocation_id: String::new(),
        invocation_token: String::new(),
        state_generation: 0,
        delivery_id: claimed.id.clone(),
        fence: claimed.fence().unwrap(),
        cancellation_epoch: claimed.cancellation_epoch,
        actor: a.store.info().host_actor,
        broker_consumer: "package-broker".into(),
    };
    let h = a
        .invoke(package, "event", InvocationKind::Event(Box::new(inv)))
        .unwrap();
    finished(h.wait(Duration::from_secs(10)));
    let _ = std::fs::remove_file(&path);
    h.progress().0
}

#[test]
fn an_event_commit_that_changes_state_needs_state_write_access() {
    let _s = serial();
    let a = App::new();
    // No StateWrite requested.
    package(a.install(
        &a.user,
        &Spec {
            permissions: vec![json!({"kind": "WorktreeRead"})],
            ..Spec::new("readonly-pack", "1.0.0")
        },
    ));
    a.ready("readonly-pack");
    let before = read(&*a.store, |s| s.packages().package_state("readonly-pack")).unwrap();
    let proposal = json!({"stateProposal": {"artifactId": "state-b", "versionId": "v1", "sha256": "b".repeat(64), "byteLength": "2", "mediaType": "application/json"}});
    let refused = run_event(&a, "readonly-pack", 1, proposal);
    assert_eq!(refused[2], "commit:PermissionDenied:-", "{refused:?}");
    let after = read(&*a.store, |s| s.packages().package_state("readonly-pack")).unwrap();
    assert_eq!(after.revision, before.revision);
    // A commit without a state change (a processing receipt and a checkpoint) is allowed.
    let accepted = run_event(&a, "readonly-pack", 2, json!({}));
    assert!(
        accepted[2].starts_with("commit:EventCommitAccepted"),
        "{accepted:?}"
    );
}

#[test]
fn an_explicit_activation_retires_an_obsolete_approved_pending_version() {
    let _s = serial();
    let a = App::new();
    let spec = Spec::new("obsolete-pack", "1.0.0");
    package(a.install(&a.user, &spec));
    a.ready("obsolete-pack");
    // A person's v1.1 waits for a runtime: approved, pending for compatibility.
    let needs_zig = Spec {
        version: "1.1.0".into(),
        runtimes: vec![
            json!({"runtimeId": "zig", "versionRequirement": ">=0.12", "required": true}),
        ],
        ..spec.clone()
    };
    let rec = package(a.update(&a.user, &needs_zig));
    assert_eq!(waits(&rec), vec!["Compatibility"]);
    let obsolete = String::from(rec.pending_digest.clone().unwrap());
    // The person then activates v1.2 explicitly.
    let rec = package(a.update(
        &a.user,
        &Spec {
            version: "1.2.0".into(),
            ..spec.clone()
        },
    ));
    assert_eq!(rec.version.0, "1.2.0");
    assert!(rec.pending_version.is_none());
    let d = obsolete.clone();
    let phase = read(&*a.store, move |s| {
        s.packages().package_version("obsolete-pack", &d)
    })
    .unwrap()
    .phase;
    assert_eq!(phase, VersionPhase::Removed);
    // Once the runtime appears, a retry has nothing to replace v1.2 with.
    a.runtimes.set(std::collections::BTreeMap::from([
        (
            "dotnet".to_owned(),
            RuntimeObservation::Found("8.0.4".into()),
        ),
        ("zig".to_owned(), RuntimeObservation::Found("0.13.0".into())),
    ]));
    let svc = a.app().packages().unwrap().service().clone();
    assert!(!svc.retry_pending("obsolete-pack").unwrap());
    assert_eq!(a.record("obsolete-pack").version.0, "1.2.0");
    // A pending version still waiting on a person's decision stays pending.
    let rec = package(a.update(
        &a.agent,
        &Spec {
            version: "1.3.0".into(),
            ..spec.clone()
        },
    ));
    assert_eq!(waits(&rec), vec!["ExactVersionApproval"]);
    let rec = package(a.update(
        &a.user,
        &Spec {
            version: "1.4.0".into(),
            ..spec.clone()
        },
    ));
    assert_eq!(rec.version.0, "1.4.0");
    assert_eq!(rec.pending_version.as_ref().unwrap().0, "1.3.0");
    assert_eq!(waits(&rec), vec!["ExactVersionApproval"]);
}

#[test]
fn an_update_that_would_break_a_package_pinning_the_replaced_version_waits() {
    let _s = serial();
    let a = App::new();
    let base = Spec::new("base-dep", "1.0.0");
    let base_rec = package(a.install(&a.user, &base));
    let pinned = Spec {
        dependencies: vec![
            json!({"packageId": "base-dep", "versionRequirement": "^1", "manifestDigest": digest_of(&base_rec), "required": true}),
        ],
        ..Spec::new("pinned-dep", "1.0.0")
    };
    let pinned_rec = package(a.install(&a.user, &pinned));
    assert_eq!(availability(&a.record("pinned-dep")), "Ready");
    // Replacing the pinned version would leave pinned-dep unavailable (a reverse pin).
    let rec = package(a.update(
        &a.user,
        &Spec {
            version: "1.1.0".into(),
            ..base.clone()
        },
    ));
    assert_eq!(waits(&rec), vec!["Compatibility"]);
    // And an update that requires the package pinning the old version: both would end
    // unavailable once the candidate is the active version.
    let rec = package(a.update(
        &a.user,
        &Spec {
            version: "1.2.0".into(),
            dependencies: vec![
                json!({"packageId": "pinned-dep", "versionRequirement": "^1", "manifestDigest": digest_of(&pinned_rec), "required": true}),
            ],
            ..base.clone()
        },
    ));
    assert_eq!(waits(&rec), vec!["Compatibility"]);
    assert_eq!(rec.version.0, "1.0.0");
    for pkg in ["base-dep", "pinned-dep"] {
        assert_eq!(availability(&a.record(pkg)), "Ready", "{pkg}");
    }
}

// ---------------------------------------------------------------------------------------------
// Review round 3: rollback loops, shutdown deadlines, collection races, re-proposals

fn marker(package: &str, what: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("devworks-fixture-{package}-{what}"))
}

/// A fixture marker file removed when dropped (also when the test fails, so no later run
/// sees it).
struct Marker(std::path::PathBuf);
impl Marker {
    fn absent(package: &str, what: &str) -> Self {
        let m = Self(marker(package, what));
        let _ = std::fs::remove_file(&m.0);
        m
    }
    fn set(&self) {
        std::fs::write(&self.0, "1").unwrap();
    }
}
impl Drop for Marker {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn automatic_rollback_never_returns_to_a_version_that_just_failed() {
    let _s = serial();
    let fail = Marker::absent("loop-pack", "fail-on-activate");
    let a = App::new();
    let spec = Spec::new("loop-pack", "1.0.0");
    package(a.install(&a.user, &spec));
    a.ready("loop-pack");
    // From now on 1.0.0 fails too (whatever its state generation).
    fail.set();
    package(a.update(
        &a.user,
        &Spec {
            version: "1.1.0".into(),
            mode: "exit-on-activate".into(),
            ..spec.clone()
        },
    ));
    // 1.1.0 fails, 1.0.0 is restored and fails as well: no viable predecessor remains, so
    // the failure is durable instead of alternating between the two.
    let rec = a.until("loop-pack", "the failure is recorded", |r| {
        matches!(r.availability, Some(PackageAvailability::Failed))
    });
    drop(fail);
    assert_eq!(rec.version.0, "1.0.0");
    std::thread::sleep(Duration::from_secs(2));
    let rec = a.record("loop-pack");
    assert_eq!(
        (rec.version.0.as_str(), availability(&rec)),
        ("1.0.0", "Failed".to_owned())
    );
    let rolled = a
        .logs("loop-pack")
        .iter()
        .filter(|c| *c == "PackageRolledBack")
        .count();
    assert_eq!(rolled, 1);
}

#[test]
fn shutdown_shares_one_deadline_and_sends_nothing_once_it_has_passed() {
    let _s = serial();
    let a = App::new();
    let packages = ["bound-a", "bound-b", "bound-c"];
    let mut markers = Vec::new();
    for p in packages {
        markers.push(Marker::absent(p, "shutdown"));
        let watch = Marker::absent(p, "shutdown-watch");
        watch.set();
        markers.push(watch);
        package(a.install(&a.user, &Spec::new(p, "1.0.0")));
        a.ready(p);
    }
    let host = a.app().packages().unwrap().clone();
    let started = Instant::now();
    host.shutdown(Duration::ZERO);
    let took = started.elapsed();
    // The deadline had passed before the first helper: none was asked to shut down (no
    // minimum wait per phase), every tree was killed at once.
    for p in packages {
        assert!(
            !marker(p, "shutdown").exists(),
            "{p} was sent host.shutdown"
        );
    }
    assert!(took < Duration::from_secs(2), "{took:?}");
}

#[test]
fn a_cancellation_shares_one_grace_between_the_acknowledgement_and_the_stop() {
    let _s = serial();
    let a = App::new();
    package(a.install(
        &a.user,
        &Spec {
            mode: "deaf-after-ready".into(),
            ..Spec::new("graceful-pack", "1.0.0")
        },
    ));
    a.ready("graceful-pack");
    // The helper reads nothing: neither an acknowledgement nor a stop arrives.
    let h = Arc::new(
        a.invoke("graceful-pack", "wait", InvocationKind::Ordinary)
            .unwrap(),
    );
    let started = Instant::now();
    let cancelling = {
        let h = h.clone();
        std::thread::spawn(move || h.cancel())
    };
    // The termination begins by failing the helper's session (before the kill), which ends the
    // invocation: an end that stays observed however late this thread looks (the instance's
    // `Failed` status lasts only until reconciliation retires it).
    let ended = h.wait(Duration::from_secs(10));
    let took = started.elapsed();
    assert!(
        matches!(ended, Some(InvokeOutcome::Unknown(_))),
        "{ended:?}"
    );
    assert!(matches!(
        joined(cancelling, Duration::from_secs(10), "the cancellation"),
        InvokeOutcome::Unknown(_)
    ));
    // The ignored cancellation terminated it...
    a.until_logged("graceful-pack", "CancellationIgnored");
    // ...after one 500 ms grace (not a grace per phase: 1 s).
    assert!(took < Duration::from_millis(850), "{took:?}");
}

#[test]
fn the_handshake_shares_one_deadline_between_initialize_and_activate() {
    let _s = serial();
    let a = App::new();
    // Each answer takes 1 s: within a 1.5 s bound each, not 2 s together.
    package(a.install(
        &a.user,
        &Spec {
            mode: "slow-handshake".into(),
            ..Spec::new("slow-pack", "1.0.0")
        },
    ));
    let rec = a.until("slow-pack", "the startup timeout is recorded", |r| {
        matches!(r.availability, Some(PackageAvailability::Failed))
    });
    assert_eq!(rec.failure_code.unwrap().0, "StartupTimeout");
}

/// Native install directories whose removal of one version pauses (up to a bound) for a
/// publication of the same version, so a test can race the two.
struct RacedFiles {
    inner: NativePackageFiles,
    target: Mutex<Option<(String, String)>>,
    /// Set when the removal of the target started; then when a publish of it finished.
    removing: (Mutex<(bool, bool)>, std::sync::Condvar),
}
impl RacedFiles {
    fn arm(&self, package: &str, digest: &str) {
        *self.target.lock().unwrap() = Some((package.into(), digest.into()));
    }
    fn is_target(&self, package: &str, digest: &str) -> bool {
        self.target
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(p, d)| p == package && d == digest)
    }
    fn wait_removing(&self) {
        let (m, cv) = &self.removing;
        let g = m.lock().unwrap();
        let (g, t) = cv
            .wait_timeout_while(g, Duration::from_secs(20), |s| !s.0)
            .unwrap();
        assert!(!t.timed_out() && g.0, "the removal never started");
    }
}
impl PackageFiles for RacedFiles {
    fn publish(
        &self,
        package_id: &str,
        manifest_digest: &str,
        files: &[BundleFile],
        expected: &[InspectedFile],
    ) -> Result<(), PackageFileError> {
        let r = self
            .inner
            .publish(package_id, manifest_digest, files, expected);
        if self.is_target(package_id, manifest_digest) {
            self.removing.0.lock().unwrap().1 = true;
            self.removing.1.notify_all();
        }
        r
    }
    fn verify(
        &self,
        package_id: &str,
        manifest_digest: &str,
        expected: &[InspectedFile],
    ) -> Result<(), PackageFileError> {
        self.inner.verify(package_id, manifest_digest, expected)
    }
    fn path(&self, package_id: &str, manifest_digest: &str, relative: &str) -> std::path::PathBuf {
        self.inner.path(package_id, manifest_digest, relative)
    }
    fn directory(&self, package_id: &str, manifest_digest: &str) -> std::path::PathBuf {
        self.inner.directory(package_id, manifest_digest)
    }
    fn remove(&self, package_id: &str, manifest_digest: &str) -> Result<(), PackageFileError> {
        if self.is_target(package_id, manifest_digest) {
            *self.target.lock().unwrap() = None;
            let (m, cv) = &self.removing;
            let mut g = m.lock().unwrap();
            g.0 = true;
            cv.notify_all();
            // Give a concurrent publication of the same version the chance to run first.
            let _ = cv
                .wait_timeout_while(g, Duration::from_secs(1), |s| !s.1)
                .unwrap();
            // Re-arm so a publish after this removal is still observed.
            *self.target.lock().unwrap() = Some((package_id.into(), manifest_digest.into()));
        }
        self.inner.remove(package_id, manifest_digest)
    }
    fn idle_for(&self, package_id: &str, manifest_digest: &str) -> Option<Duration> {
        self.inner.idle_for(package_id, manifest_digest)
    }
    fn installed(&self) -> Result<Vec<(String, String)>, PackageFileError> {
        self.inner.installed()
    }
    fn clean_staging(&self, older_than: Option<Duration>) -> Result<usize, PackageFileError> {
        self.inner.clean_staging(older_than)
    }
}

#[test]
fn collection_never_removes_a_directory_a_concurrent_publication_reuses() {
    let _s = serial();
    let raced: Arc<std::sync::OnceLock<Arc<RacedFiles>>> = Arc::new(std::sync::OnceLock::new());
    let slot = raced.clone();
    let a = App::with_files(move |h| {
        let f = Arc::new(RacedFiles {
            inner: NativePackageFiles::new(h.root.join("packages")),
            target: Mutex::new(None),
            removing: (Mutex::new((false, false)), std::sync::Condvar::new()),
        });
        let _ = slot.set(f.clone());
        f
    });
    let files = raced.get().unwrap().clone();
    let spec = Spec::new("raced-pack", "1.0.0");
    let rec = package(a.install(&a.user, &spec));
    let digest = digest_of(&rec);
    a.ready("raced-pack");
    files.arm("raced-pack", &digest);
    // Uninstalled: once its helper is retired, collection removes the version's directory.
    package(a.simple(&a.user, "addons.uninstall", "Uninstall", "raced-pack"));
    files.wait_removing();
    // While that removal is under way, the person installs the same contents again: their
    // publication must not reuse a directory collection is about to remove.
    let rec = package(a.install(&a.user, &spec));
    assert_eq!(digest_of(&rec), digest);
    let rec = a.until("raced-pack", "the reinstall runs", |r| {
        matches!(
            r.availability,
            Some(PackageAvailability::Ready | PackageAvailability::Failed)
        )
    });
    assert_eq!(availability(&rec), "Ready", "{:?}", rec.failure_code);
    a.ready("raced-pack");
    assert!(
        a.h.root
            .join("packages")
            .join("raced-pack")
            .join(&digest)
            .join("fixture.json")
            .exists()
    );
}

#[test]
fn a_re_proposal_of_retired_contents_waits_for_approval_and_activates_once_approved() {
    let _s = serial();
    let a = App::new();
    let spec = Spec::new("repropose-pack", "1.0.0");
    package(a.install(&a.user, &spec));
    a.ready("repropose-pack");
    let first = Spec {
        version: "1.1.0".into(),
        ..spec.clone()
    };
    let (_, d1, _) = propose(&a, &first);
    propose(
        &a,
        &Spec {
            version: "1.2.0".into(),
            ..spec.clone()
        },
    );
    // 1.1.0 was retired by the newer proposal; the agent proposes it again.
    let (p, d, _) = propose(&a, &first);
    assert_eq!(d, d1);
    let dd = d.clone();
    let v = read(&*a.store, move |s| {
        s.packages().package_version("repropose-pack", &dd)
    })
    .unwrap();
    assert_eq!(v.phase, VersionPhase::Staged);
    assert_eq!(v.waits, vec!["ExactVersionApproval".to_owned()]);
    assert_eq!(v.provenance["agentAuthored"], json!(true));
    let svc = a.app().packages().unwrap().service().clone();
    svc.decide_proposal(&a.user, &p.id, 1, &d, true, "yes")
        .unwrap()
        .unwrap();
    a.until("repropose-pack", "the approved proposal activates", |r| {
        r.version.0 == "1.1.0"
    });
}

// ---------------------------------------------------------------------------------------------
// Review round 4: full instance keys, single proposals, abandoned versions, boot cleanup

#[test]
fn host_seams_act_only_for_the_complete_current_instance_key() {
    let _s = serial();
    let a = App::new();
    let rec = package(a.install(&a.user, &Spec::new("keyed-pack", "1.0.0")));
    a.ready("keyed-pack");
    let svc = a.app().packages().unwrap().service().clone();
    let current = read(&*a.store, |s| s.packages().package("keyed-pack")).unwrap();
    let key = |generation: i64, config: i64| HelperKey {
        package_id: "keyed-pack".into(),
        digest: digest_of(&rec),
        state_generation: generation,
        config_revision: config,
    };
    // A stale attempt (an earlier configuration or state generation) changes nothing.
    for stale in [
        key(current.state_generation, current.config_revision + 1),
        key(current.state_generation + 1, current.config_revision),
    ] {
        assert!(
            !svc.startup_failed(&stale, "HelperExited", "stale", 2, true)
                .unwrap()
                .rolled_back
        );
        assert_eq!(availability(&a.record("keyed-pack")), "Ready");
    }
    // The current instance's failure is recorded.
    svc.startup_failed(
        &key(current.state_generation, current.config_revision),
        "HelperExited",
        "current",
        2,
        false,
    )
    .unwrap();
    assert_eq!(availability(&a.record("keyed-pack")), "Failed");
}

#[test]
fn identical_contents_have_one_pending_proposal() {
    let _s = serial();
    let a = App::new();
    let spec = Spec::new("twice-pack", "1.0.0");
    package(a.install(&a.user, &spec));
    let next = Spec {
        version: "1.1.0".into(),
        ..spec.clone()
    };
    let (first, d, source) = propose(&a, &next);
    // Submitting the same contents again is the same proposal, not a second one.
    let body = json!({"kind": "addons.propose", "source": source, "manifestDigest": d, "basePackageId": "twice-pack"});
    let again = match a.domain(&a.agent, "addons.propose", body) {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::NoChange {
                result: OperationResult::AddonsPropose { value },
            } => value,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    assert_eq!(again.id, first.id);
    // The person installs exactly those contents: the one proposal is approved, and nothing
    // left pending can later reject the active version.
    package(a.update(&a.user, &next));
    let pid = first.id.clone();
    let stored = read(&*a.store, move |s| s.packages().proposal(&pid)).unwrap();
    assert_eq!(stored.state, "Approved");
    let svc = a.app().packages().unwrap().service().clone();
    assert!(
        svc.decide_proposal(&a.user, &first.id, stored.revision, &d, false, "no")
            .unwrap()
            .is_err()
    );
    let dd = d.clone();
    let phase = read(&*a.store, move |s| {
        s.packages().package_version("twice-pack", &dd)
    })
    .unwrap()
    .phase;
    assert_eq!(phase, VersionPhase::Installed);
}

#[test]
fn a_displaced_staged_version_without_a_live_proposal_is_collected() {
    let _s = serial();
    let a = App::new();
    let spec = Spec::new("displaced-pack", "1.0.0");
    package(a.install(&a.user, &spec));
    // Two agent bundles in a row: each waits for the person; the second displaces the first.
    let v2 = package(a.update(
        &a.agent,
        &Spec {
            version: "1.1.0".into(),
            ..spec.clone()
        },
    ));
    let d2 = String::from(v2.pending_digest.clone().unwrap());
    let v3 = package(a.update(
        &a.agent,
        &Spec {
            version: "1.2.0".into(),
            ..spec.clone()
        },
    ));
    assert_eq!(v3.pending_version.as_ref().unwrap().0, "1.2.0");
    // Nothing points at 1.1.0 any more and no proposal waits for it: it is retired and its
    // install directory removed (the pending 1.2.0 is kept).
    let dir = a.h.root.join("packages").join("displaced-pack").join(&d2);
    let until = Instant::now() + PATIENCE;
    loop {
        let d = d2.clone();
        let phase = read(&*a.store, move |s| {
            s.packages().package_version("displaced-pack", &d)
        })
        .unwrap()
        .phase;
        if phase == VersionPhase::Removed && !dir.exists() {
            break;
        }
        assert!(
            Instant::now() < until,
            "the abandoned version stays {phase:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let d3 = String::from(v3.pending_digest.clone().unwrap());
    assert!(
        a.h.root
            .join("packages")
            .join("displaced-pack")
            .join(&d3)
            .exists()
    );
}

/// Native install directories that cannot be listed (a collection I/O error at boot).
struct UnlistableFiles(NativePackageFiles);
impl PackageFiles for UnlistableFiles {
    fn publish(
        &self,
        package_id: &str,
        manifest_digest: &str,
        files: &[BundleFile],
        expected: &[InspectedFile],
    ) -> Result<(), PackageFileError> {
        self.0.publish(package_id, manifest_digest, files, expected)
    }
    fn verify(
        &self,
        package_id: &str,
        manifest_digest: &str,
        expected: &[InspectedFile],
    ) -> Result<(), PackageFileError> {
        self.0.verify(package_id, manifest_digest, expected)
    }
    fn path(&self, package_id: &str, manifest_digest: &str, relative: &str) -> std::path::PathBuf {
        self.0.path(package_id, manifest_digest, relative)
    }
    fn directory(&self, package_id: &str, manifest_digest: &str) -> std::path::PathBuf {
        self.0.directory(package_id, manifest_digest)
    }
    fn remove(&self, package_id: &str, manifest_digest: &str) -> Result<(), PackageFileError> {
        self.0.remove(package_id, manifest_digest)
    }
    fn idle_for(&self, package_id: &str, manifest_digest: &str) -> Option<Duration> {
        self.0.idle_for(package_id, manifest_digest)
    }
    fn installed(&self) -> Result<Vec<(String, String)>, PackageFileError> {
        Err(PackageFileError::Io(
            "the install directory cannot be listed".into(),
        ))
    }
    fn clean_staging(&self, older_than: Option<Duration>) -> Result<usize, PackageFileError> {
        self.0.clean_staging(older_than)
    }
}

#[test]
fn a_boot_that_fails_after_the_package_host_started_releases_the_store() {
    let _s = serial();
    let h = Harness::new();
    let files: Arc<dyn PackageFiles> = Arc::new(UnlistableFiles(NativePackageFiles::new(
        h.root.join("packages"),
    )));
    let a = App::unbooted(h, None, Some(files));
    let store = Arc::downgrade(&a.store);
    let booted = a.rt.block_on(boot(a.composition()));
    assert!(booted.is_err(), "the package boot fails");
    drop(booted);
    drop(a);
    // Nothing (no service<->host cycle) keeps the store, and its workspace lock, alive.
    assert!(
        store.upgrade().is_none(),
        "the store outlived a failed boot"
    );
}

fn helper_ready_events(store: &dyn ApplicationStore, package: &str) -> usize {
    let p = package.to_owned();
    read(store, move |s| {
        s.history().page(
            &EventFilter::Aggregate {
                kind: "package".into(),
                id: p,
            },
            0,
            500,
        )
    })
    .events
    .iter()
    .filter(|e| e.kind == "PackageHelperReady")
    .count()
}

#[test]
fn a_spent_restart_budget_never_starts_its_key_again_before_the_failure_is_recorded() {
    let _s = serial();
    let a = App::new();
    package(a.install(
        &a.user,
        &Spec {
            mode: "exit-after-ready".into(),
            ..Spec::new("spent-pack", "1.0.0")
        },
    ));
    // The second (last budgeted) instance is running: from now on recording its failure has
    // to wait for the database, while reconciliation keeps reading the old registry.
    let until = Instant::now() + PATIENCE;
    while helper_ready_events(&*a.store, "spent-pack") < 2 {
        assert!(Instant::now() < until, "the second instance never started");
        std::thread::sleep(Duration::from_millis(5));
    }
    let lock = rusqlite::Connection::open(a.h.root.join("workspace.sqlite")).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let host = a.app().packages().unwrap().clone();
    // The second instance exits and its failure is accounted (in memory) while the durable
    // record cannot be written yet. The budget's count stays until that record is written (the
    // instance's `Failed` status lasts only until reconciliation retires it); had the second
    // instance failed before the lock, its logged exit stays instead.
    let until = Instant::now() + Duration::from_secs(20);
    while host.failures("spent-pack") < 2 && a.logged("spent-pack", "HelperExited") < 2 {
        assert!(
            Instant::now() < until,
            "the second instance never failed: {:?}",
            host.instance_status("spent-pack")
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    // Past any restart backoff, a reconciliation pass reads the registry that still says the
    // package should run.
    std::thread::sleep(Duration::from_millis(200));
    let pass = {
        let host = host.clone();
        std::thread::spawn(move || host.reconcile())
    };
    std::thread::sleep(Duration::from_millis(1000));
    lock.execute_batch("COMMIT;").unwrap();
    drop(lock);
    joined(pass, Duration::from_secs(30), "the reconciliation pass");
    a.until("spent-pack", "the failure is recorded", |r| {
        matches!(r.availability, Some(PackageAvailability::Failed))
    });
    std::thread::sleep(Duration::from_secs(1));
    // No third instance was started on the spent budget.
    assert_eq!(helper_ready_events(&*a.store, "spent-pack"), 2);
    assert_eq!(availability(&a.record("spent-pack")), "Failed");
    // And the spent budget itself is forgotten once the key is no longer the intent.
    let host = a.app().packages().unwrap().clone();
    let until = Instant::now() + PATIENCE;
    while host.tracked_budgets() != 0 {
        assert!(Instant::now() < until, "budgets accumulate");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn only_the_current_state_generation_and_its_seed_are_retained() {
    let _s = serial();
    let a = App::new();
    let spec = Spec::new("generations-pack", "1.0.0");
    package(a.install(&a.user, &spec));
    for v in ["1.1.0", "1.2.0", "1.3.0"] {
        package(a.update(
            &a.user,
            &Spec {
                version: v.into(),
                ..spec.clone()
            },
        ));
    }
    let current = read(&*a.store, |s| s.packages().package("generations-pack")).unwrap();
    assert_eq!(current.state_generation, 4);
    let kept: Vec<i64> = (1..=4)
        .filter(|g| {
            let g = *g;
            read(&*a.store, move |s| {
                s.packages().state_generation("generations-pack", g)
            })
            .is_some()
        })
        .collect();
    assert_eq!(kept, vec![3, 4]);
}

// ---------------------------------------------------------------------------------------------
// Review round 5: launches overtaken by shutdown, configuration retries, terminal revocation,
// registry capacity

/// Whether the package's helper was launched: an instance of it is starting, running or not yet
/// retired, or one became ready or its failure was recorded before (durable records).
fn launched(a: &App, package: &str) -> bool {
    !a.app()
        .packages()
        .unwrap()
        .instance_status(package)
        .is_empty()
        || helper_ready_events(&*a.store, package) > 0
        || matches!(
            a.record(package).availability,
            Some(PackageAvailability::Failed)
        )
}

#[test]
fn launches_queued_in_a_reconciliation_pass_never_outlive_shutdown() {
    let _s = serial();
    let a = App::new();
    // A pass busy starting a slow helper...
    package(a.install(
        &a.user,
        &Spec {
            mode: "slow-initialize".into(),
            ..Spec::new("a-slow", "1.0.0")
        },
    ));
    let host = a.app().packages().unwrap().clone();
    // Usually it is still starting (its handshake takes 1.2 s; on a loaded machine this thread
    // may look later, when it is running or, past its handshake bound, failed).
    let until = Instant::now() + PATIENCE;
    while !launched(&a, "a-slow") {
        assert!(Instant::now() < until, "a-slow never started");
        std::thread::sleep(Duration::from_millis(5));
    }
    // ...while two more arrive: the next pass starts them in one loop, the slow one first.
    package(a.install(
        &a.user,
        &Spec {
            mode: "slow-initialize".into(),
            ..Spec::new("m-slow", "1.0.0")
        },
    ));
    package(a.install(&a.user, &Spec::new("z-normal", "1.0.0")));
    // Usually that pass is still starting m-slow (on a loaded machine the installs may take
    // longer than its handshake: then it is already running).
    let until = Instant::now() + PATIENCE;
    while !launched(&a, "m-slow") {
        assert!(Instant::now() < until, "m-slow never started");
        std::thread::sleep(Duration::from_millis(5));
    }
    // Shutdown overtakes the pass; whatever z-normal did before it, nothing launches after it.
    let before = helper_ready_events(&*a.store, "z-normal");
    host.shutdown(Duration::from_millis(200));
    // A pass still under way goes on with z-normal: it must not launch (or is killed at once).
    // Another pass waits for that one to finish (passes are serialized) and does nothing itself.
    host.reconcile();
    assert_eq!(
        helper_ready_events(&*a.store, "z-normal"),
        before,
        "z-normal started after shutdown"
    );
}

#[test]
fn a_corrected_configuration_restarts_a_helper_whose_budget_was_spent() {
    let _s = serial();
    let fail = Marker::absent("reconfig-pack", "fail-on-activate");
    let a = App::new();
    let spec = Spec {
        config_schema: Some(json!({"type": "object"})),
        ..Spec::new("reconfig-pack", "1.0.0")
    };
    let rec = package(a.install(&a.user, &spec));
    a.ready("reconfig-pack");
    // Under its current configuration the helper fails until its budget is spent.
    fail.set();
    let schema = serde_json::to_value(rec.configuration_schema.clone().unwrap()).unwrap();
    let configure = |value: Value| {
        let body = json!({"kind": "addons.configure", "packageId": "reconfig-pack",
            "expectedRevision": a.record("reconfig-pack").revision.get().to_string(),
            "config": {"schema": schema, "value": value}});
        package(a.domain(&a.user, "addons.configure", body))
    };
    configure(json!({"mode": "broken"}));
    a.until("reconfig-pack", "the failure is recorded", |r| {
        matches!(r.availability, Some(PackageAvailability::Failed))
    });
    // A corrected configuration is a new instance: it starts again.
    drop(fail);
    let rec = configure(json!({"mode": "fixed"}));
    assert_ne!(availability(&rec), "Failed");
    a.until("reconfig-pack", "the corrected configuration runs", |r| {
        matches!(r.availability, Some(PackageAvailability::Ready))
    });
    a.ready("reconfig-pack");
}

#[test]
fn a_finished_invocation_can_no_longer_change_state() {
    let _s = serial();
    let a = App::new();
    let mut spec = Spec::new("late-pack", "1.0.0");
    spec.contributions.push("late-state");
    package(a.install(&a.user, &spec));
    a.ready("late-pack");
    let answer = Marker::absent("late-pack", "late-state");
    let before = read(&*a.store, |s| s.packages().package_state("late-pack")).unwrap();
    let h = a
        .invoke("late-pack", "late-state", InvocationKind::Ordinary)
        .unwrap();
    assert!(matches!(
        finished(h.wait(Duration::from_secs(10))),
        InvocationOutcome::Completed { .. }
    ));
    // The helper creates the file, then writes the answer: an empty file is not the answer yet.
    let until = Instant::now() + PATIENCE;
    let late = loop {
        let late = std::fs::read_to_string(&answer.0).unwrap_or_default();
        if !late.is_empty() {
            break late;
        }
        assert!(
            Instant::now() < until,
            "the late compare-and-set never answered"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    // The token was revoked with the terminal answer: the compare-and-set was refused.
    assert_eq!(late, "PermissionDenied");
    let after = read(&*a.store, |s| s.packages().package_state("late-pack")).unwrap();
    assert_eq!(after.revision, before.revision);
}

#[test]
fn the_registry_is_read_whole_and_a_new_identity_beyond_capacity_is_refused() {
    let _s = serial();
    let a = App::new();
    // Sorts after every other identity below.
    package(a.install(&a.user, &Spec::new("zz-last", "1.0.0")));
    a.ready("zz-last");
    // Past capacity (written directly, as an earlier build might have left it).
    let c = rusqlite::Connection::open(a.h.root.join("workspace.sqlite")).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c.execute_batch("BEGIN;").unwrap();
    for n in 0..(MAX_PACKAGES + 4) {
        c.execute(
            "INSERT INTO packages(package_id,revision,enabled,installed,state_generation,config_revision,created_at,updated_at) VALUES(?1,1,0,0,0,0,1,1)",
            [format!("filler-{n:05}")],
        )
        .unwrap();
    }
    c.execute_batch("COMMIT;").unwrap();
    drop(c);
    // The package that sorts last is still in the catalog (read page by page to its end) and
    // keeps running.
    a.app().packages().unwrap().reconcile();
    let mut cursor: Option<String> = None;
    let mut all = Vec::new();
    loop {
        let mut page = json!({"limit": 500});
        if let Some(c) = &cursor {
            page["cursor"] = json!(c);
        }
        let frame = json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": "q-all", "schemaRevision": 2,
            "body": {"kind": "addons.catalog", "page": page}});
        match a
            .app()
            .query(&a.user, "addons.catalog", frame.to_string().as_bytes())
            .unwrap()
        {
            DomainQueryReply::Result {
                result: OperationResult::AddonsCatalog { value },
                ..
            } => {
                all.extend(value.items.0);
                match value.next_cursor {
                    Some(c) => cursor = Some(c.0),
                    None => break,
                }
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(all.len(), MAX_PACKAGES + 5);
    let rec = all
        .into_iter()
        .find(|r| r.id.as_str() == "zz-last")
        .expect("the last identity is listed");
    assert_eq!(availability(&rec), "Ready");
    a.ready("zz-last");
    assert!(
        a.app()
            .packages()
            .unwrap()
            .instance_status("zz-last")
            .iter()
            .any(|(_, s)| *s == InstanceStatus::Ready)
    );
    // A new identity is refused, with the reason.
    let refused_install = a.install(&a.user, &Spec::new("zz-new", "1.0.0"));
    assert_eq!(
        refused(refused_install),
        (
            DomainErrorCode::Conflict,
            Some("package_registry_capacity".into())
        )
    );
}

// ---------------------------------------------------------------------------------------------
// Review round 6: explicit retries, lossless versions

#[test]
fn an_explicit_retry_restarts_a_helper_even_before_reconciliation_saw_its_failure() {
    let _s = serial();
    let fail = Marker::absent("retry-pack", "fail-on-activate");
    let mut a = App::new();
    // One start per budget: the first failure spends it.
    a.restart_limit = Some(1);
    package(a.install(&a.user, &Spec::new("retry-pack", "1.0.0")));
    package(a.install(
        &a.user,
        &Spec {
            mode: "slow-initialize".into(),
            ..Spec::new("z-busy", "1.0.0")
        },
    ));
    a.ready("retry-pack");
    a.ready("z-busy");
    a.shutdown();
    // At the next boot, one reconciliation pass starts retry-pack (which fails at once, spending
    // its budget and recording the failure) and then z-busy (slow), during which the person
    // retries: no pass sees the recorded failure before the retry.
    fail.set();
    let store = a.store.clone();
    let composition = a.composition();
    let booted = std::thread::scope(|scope| {
        let booting = scope.spawn(|| a.rt.block_on(boot(composition)));
        let until = Instant::now() + PATIENCE;
        loop {
            let p = read(&*store, |s| s.packages().package("retry-pack")).unwrap();
            if p.failure.is_some() {
                break;
            }
            assert!(Instant::now() < until, "the failure was never recorded");
            std::thread::sleep(Duration::from_millis(2));
        }
        // The retry, as `addons.enable` commits it: the failure cleared, the registry advanced.
        drop(fail);
        write(&*store, |tx| {
            let mut p = tx.packages().package("retry-pack")?.unwrap();
            p.failure = None;
            let expected = p.revision;
            tx.packages().update_package(&p, expected)?;
            let head = tx.packages().registry_revision()?;
            tx.packages().advance_registry(head)?;
            Ok(())
        })
        .unwrap();
        let until = Instant::now() + Duration::from_secs(60);
        while !booting.is_finished() {
            assert!(Instant::now() < until, "the boot never finished");
            std::thread::sleep(Duration::from_millis(10));
        }
        booting.join().unwrap()
    });
    a.app = Some(booted.expect("boot"));
    // The retried key gets a new budget: the helper starts and the package is Ready.
    a.app().packages().unwrap().reconcile();
    a.ready("retry-pack");
    assert_eq!(availability(&a.record("retry-pack")), "Ready");
}

#[test]
fn a_prerelease_runtime_never_satisfies_a_stable_requirement() {
    use devworks_engine::package_host::discovery::parse_version;
    for (printed, version) in [
        ("10.0.100-preview.1.25120.13", "10.0.100-preview.1.25120.13"),
        (
            "rustc 1.84.0-nightly (90b35a623 2024-11-26)",
            "1.84.0-nightly",
        ),
        ("v20.11.1", "20.11.1"),
        ("8.0", "8.0.0"),
        ("1.2.3+build.5", "1.2.3+build.5"),
        ("cargo 1.82.0 (8f40fc59f 2024-08-21)", "1.82.0"),
        // Not lossless SemVer: kept as printed (it satisfies nothing).
        ("1.2.3.4", "1.2.3.4"),
        ("6.0.3rc1", "6.0.3rc1"),
    ] {
        assert_eq!(
            parse_version(printed).as_deref(),
            Some(version),
            "{printed}"
        );
    }
    let _s = serial();
    let a = App::new();
    let needs = |pkg: &str| Spec {
        runtimes: vec![
            json!({"runtimeId": "dotnet", "versionRequirement": ">=8", "required": true}),
        ],
        ..Spec::new(pkg, "1.0.0")
    };
    a.runtimes.set(std::collections::BTreeMap::from([(
        "dotnet".to_owned(),
        RuntimeObservation::Found(parse_version("10.0.100-preview.1").unwrap()),
    )]));
    let rec = package(a.install(&a.user, &needs("preview-pack")));
    assert_eq!(availability(&rec), "MissingDependency");
    let q = &rec.prerequisites.unwrap().0[0];
    assert_eq!(
        (
            format!("{:?}", q.state.clone().unwrap()),
            q.detected_version.as_ref().unwrap().0.as_str()
        ),
        ("Incompatible".into(), "10.0.100-preview.1")
    );
}

#[test]
fn invocation_deadline_and_handle_do_not_wait_for_a_busy_history_writer() {
    let _s = serial();
    let a = App::new();
    package(a.install(&a.user, &Spec::new("history-bound-pack", "1.0.0")));
    a.ready("history-bound-pack");
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    gate.arm(IdKind::Event);
    let held = gate.clone();
    a.store
        .submit_write(
            WriteOptions::default(),
            write_job(
                move |_| {
                    held.pass(IdKind::Event);
                    Ok(())
                },
                |_| {},
            ),
        )
        .unwrap();
    assert!(gate.entered(Duration::from_secs(10)));
    let host = a.app().packages().unwrap().clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let result = host.invoke(InvokeRequest {
            package_id: "history-bound-pack".into(), contribution_id: "history-bound-pack/wait".into(),
            scope: InvocationScope::Workspace,
            input: serde_json::from_value(json!({"artifactId": "i", "versionId": "v", "sha256": "e".repeat(64), "byteLength": "1", "mediaType": "a/b"})).unwrap(),
            operation_id: id("op-history-bound"), deadline: Duration::from_millis(200), allowed_operations: vec![], kind: InvocationKind::Ordinary,
        });
        let _ = tx.send(result);
    });
    let handle = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("history must not delay the invocation handle")
        .unwrap();
    assert!(matches!(
        finished(handle.wait(Duration::from_secs(2))),
        InvocationOutcome::CancelledBeforeEffect
    ));
    assert!(
        !gate.returned(),
        "deadline enforced while history is still blocked"
    );
    gate.release();
    thread.join().unwrap();
}

#[test]
fn saved_project_overrides_are_readable_at_revision_three_after_restart() {
    let _s = serial();
    let mut a = App::new();
    package(a.install(&a.user, &Spec::new("overrides-pack", "1.0.0")));
    let configured = package(a.domain(&a.user, "addons.configure", json!({"kind": "addons.configure", "packageId": "overrides-pack", "expectedRevision": a.record("overrides-pack").revision.get().to_string(), "config": {"schema": host_schema_ref(PROJECT_ENABLEMENT_SCHEMA).unwrap(), "value": {"disabledProjects": ["project-a", "project-b"]}}})));
    assert!(
        configured.disabled_projects.is_none(),
        "revision two stays unchanged"
    );
    a.shutdown();
    a.boot();
    let frame = json!({"protocol": "1.0-draft.1", "mode": "Query", "queryId": "saved-overrides", "schemaRevision": 3, "body": {"kind": "addons.catalog", "page": {"limit": 100}}});
    let reply = a
        .app()
        .query(&a.user, "addons.catalog", frame.to_string().as_bytes())
        .unwrap();
    let DomainQueryReply::Result {
        result: OperationResult::AddonsCatalog { value },
        ..
    } = reply
    else {
        panic!("{reply:?}");
    };
    let record = value
        .items
        .0
        .iter()
        .find(|p| p.id.as_str() == "overrides-pack")
        .unwrap();
    let projects: Vec<_> = record
        .disabled_projects
        .as_ref()
        .unwrap()
        .0
        .iter()
        .map(|p| p.as_str())
        .collect();
    assert_eq!(projects, ["project-a", "project-b"]);
}

#[test]
fn saved_configuration_is_readable_at_revision_three_after_restart() {
    let _s = serial();
    let mut a = App::new();
    let spec = Spec {
        config_schema: Some(
            json!({"type": "object", "properties": {"threads": {"type": "integer"}, "mode": {"type": "string"}}}),
        ),
        ..Spec::new("saved-config-pack", "1.0.0")
    };
    let rec = package(a.install(&a.user, &spec));
    let schema = serde_json::to_value(rec.configuration_schema.unwrap()).unwrap();
    let configured = package(a.domain(&a.user, "addons.configure", json!({"kind": "addons.configure", "packageId": "saved-config-pack", "expectedRevision": a.record("saved-config-pack").revision.get().to_string(), "config": {"schema": schema, "value": {"threads": 3, "mode": "fast"}}})));
    assert!(
        configured.configuration.is_none(),
        "revision two stays unchanged"
    );
    a.shutdown();
    a.boot();
    let frame = json!({"protocol": "1.0-draft.1", "mode": "Query", "queryId": "saved-config", "schemaRevision": 3, "body": {"kind": "addons.catalog", "page": {"limit": 100}}});
    let reply = a
        .app()
        .query(&a.user, "addons.catalog", frame.to_string().as_bytes())
        .unwrap();
    let DomainQueryReply::Result {
        result: OperationResult::AddonsCatalog { value },
        ..
    } = reply
    else {
        panic!("{reply:?}");
    };
    let record = value
        .items
        .0
        .iter()
        .find(|p| p.id.as_str() == "saved-config-pack")
        .unwrap();
    let config = record.configuration.as_ref().unwrap();
    assert_eq!(serde_json::to_value(&config.schema).unwrap(), schema);
    assert_eq!(config.value.0, json!({"threads": 3, "mode": "fast"}));
}

#[test]
fn a_blocked_index_fetch_never_stalls_unrelated_package_reconciliation() {
    let _s = serial();
    let mut a = App::unbooted(Harness::new(), None, None);
    set_index_sources(
        &*a.store,
        json!([{"location": "https://packs.example/devworks-index.json", "publicKey": hex(key(7).verifying_key().as_bytes())}]),
    );
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    gate.arm(IdKind::Operation);
    *a.transport.1.lock().unwrap() = Some(gate.clone());
    a.boot();
    assert!(
        gate.entered(Duration::from_secs(10)),
        "startup index fetch began"
    );
    package(a.install(&a.user, &Spec::new("local-during-fetch", "1.0.0")));
    let handle = a
        .invoke("local-during-fetch", "echo", InvocationKind::Ordinary)
        .expect("the local helper starts while index IO is blocked");
    assert!(matches!(
        finished(handle.wait(Duration::from_secs(10))),
        InvocationOutcome::Completed { .. }
    ));
    assert!(!gate.returned(), "fetch is still blocked");
    a.shutdown();
    assert!(!gate.returned(), "shutdown does not wait for the fetch");
    gate.release();
}

#[test]
fn abandoning_a_runtime_releases_idle_discovery_workers() {
    let _s = serial();
    let mut a = App::new();
    // Let the initial empty-index observation finish and both periodic workers become idle.
    std::thread::sleep(Duration::from_millis(100));
    let weak = Arc::downgrade(a.app().packages().unwrap());
    drop(a.app.take().unwrap());
    let until = Instant::now() + Duration::from_secs(3);
    while weak.upgrade().is_some() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        weak.upgrade().is_none(),
        "abandoned workers must release the host and its store"
    );
}

#[test]
fn restart_backoff_wakes_reconciliation_before_a_long_periodic_tick() {
    let _s = serial();
    let mut a = App::unbooted(Harness::new(), None, None);
    a.host_tick = Some(Duration::from_secs(60));
    a.retry_backoff = Some(Duration::from_secs(2));
    a.boot();
    package(a.install(&a.user, &Spec::new("retry-wakeup-pack", "1.0.0")));
    a.ready("retry-wakeup-pack");
    let first = a
        .invoke("retry-wakeup-pack", "crash", InvocationKind::Ordinary)
        .unwrap();
    let old_session = first.session_id().to_owned();
    assert!(matches!(
        first.wait(Duration::from_secs(10)),
        Some(InvokeOutcome::Unknown(_))
    ));
    let next = a
        .invoke("retry-wakeup-pack", "echo", InvocationKind::Ordinary)
        .expect("retry must not wait for the60second maintenance tick");
    assert_ne!(next.session_id(), old_session);
    assert!(matches!(
        finished(next.wait(Duration::from_secs(10))),
        InvocationOutcome::Completed { .. }
    ));
}

struct SubmissionCounter {
    inner: Arc<dyn ApplicationStore>,
    submitted: AtomicUsize,
}
impl ApplicationStore for SubmissionCounter {
    fn info(&self) -> StoreInfo {
        self.inner.info()
    }
    fn submit_write(
        &self,
        options: WriteOptions,
        job: Box<dyn WriteJob>,
    ) -> Result<(), StorageError> {
        self.inner.submit_write(options, job)?;
        self.submitted.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn read(
        &self,
        options: &ReadOptions,
        work: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.inner.read(options, work)
    }
}
#[test]
fn queued_package_state_writes_are_refused_after_completion_or_cancellation() {
    let _s = serial();
    for cancel in [false, true] {
        let mut a = App::unbooted(Harness::new(), None, None);
        let store = Arc::new(SubmissionCounter {
            inner: a.store.clone(),
            submitted: AtomicUsize::new(0),
        });
        a.store = store.clone();
        a.boot();
        let mut spec = Spec::new("queued-state-pack", "1.0.0");
        spec.contributions.push("queued-state");
        package(a.install(&a.user, &spec));
        a.ready("queued-state-pack");
        let ready = Marker::absent("queued-state-pack", "cas-ready");
        let go = Marker::absent("queued-state-pack", "cas-go");
        let finish = Marker::absent("queued-state-pack", "cas-finish");
        let result = Marker::absent("queued-state-pack", "cas-result");
        let before = read(&*a.store, |s| {
            s.packages().package_state("queued-state-pack")
        })
        .unwrap();
        let h = a
            .invoke(
                "queued-state-pack",
                "queued-state",
                InvocationKind::Ordinary,
            )
            .unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while !ready.0.exists() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        }
        // Drain invocation attribution before holding the writer; the helper has not sent CAS.
        write(&*a.store, |_| Ok(())).unwrap();
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        gate.arm(IdKind::Event);
        let held = gate.clone();
        a.store
            .submit_write(
                WriteOptions::default(),
                write_job(
                    move |_| {
                        held.pass(IdKind::Event);
                        Ok(())
                    },
                    |_| {},
                ),
            )
            .unwrap();
        assert!(gate.entered(Duration::from_secs(10)));
        let submitted = store.submitted.load(Ordering::SeqCst);
        std::fs::write(&go.0, "go").unwrap();
        while store.submitted.load(Ordering::SeqCst) == submitted {
            assert!(Instant::now() < until, "CAS was not queued");
            std::thread::sleep(Duration::from_millis(5));
        }
        if cancel {
            h.cancel();
        } else {
            std::fs::write(&finish.0, "finish").unwrap();
        }
        assert!(
            h.wait(Duration::from_secs(3)).is_some(),
            "the invocation must end while the writer is held"
        );
        gate.release();
        while std::fs::read_to_string(&result.0)
            .unwrap_or_default()
            .is_empty()
        {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            std::fs::read_to_string(&result.0).unwrap(),
            "PermissionDenied"
        );
        let after = read(&*a.store, |s| {
            s.packages().package_state("queued-state-pack")
        })
        .unwrap();
        assert_eq!(after.revision, before.revision);
    }
}

#[test]
fn a_late_index_result_cannot_restore_removed_sources_or_delete_current_ones() {
    use devworks_core::ports::packages::{IndexSource, IndexVerifier};
    let _s = serial();
    let a = App::new();
    let service = a.app().packages().unwrap().service();
    let source = |name: &str| IndexSource {
        location: format!("https://packs.example/{name}.json"),
        public_key: hex(key(7).verifying_key().as_bytes()),
    };
    let old = source("old");
    let current = source("current");
    let spec = Spec::new("source-pack", "1.0.0");
    let (bytes, digest) = bundle(&spec);
    let index = write_index(
        &json!({"publisherId":"acme","generatedAtMs":1,"entries":[entry(&spec,&bytes,&digest,"https://packs.example/source-pack.dwpkg")]}),
        &key(7),
    );
    let old_result = BundleIndexVerifier.verify(&old, &index).unwrap();
    let current_result = BundleIndexVerifier.verify(&current, &index).unwrap();
    set_index_sources(&*a.store, json!([old]));
    // A's fetch started under this configuration; B replaces it before A completes.
    set_index_sources(&*a.store, json!([current]));
    service.record_index(&current, Ok(current_result)).unwrap();
    service.record_index(&old, Ok(old_result)).unwrap();
    let indexes = read(&*a.store, |s| s.packages().indexes());
    assert_eq!(indexes.len(), 1);
    assert_eq!(
        indexes[0].location, current.location,
        "late A must not restore the removed source"
    );
    assert_eq!(
        a.record("source-pack").available.unwrap().0[0].source.0,
        current.location
    );
}

#[test]
fn busy_staged_updates_do_not_delay_unrelated_helper_startup() {
    let _s = serial();
    let mut a = App::unbooted(Harness::new(), None, None);
    a.host_tick = Some(Duration::from_secs(60));
    a.boot();
    let spec = Spec::new("staged-busy-pack", "1.0.0");
    package(a.install(&a.user, &spec));
    a.ready("staged-busy-pack");
    let running = a
        .invoke("staged-busy-pack", "wait", InvocationKind::Ordinary)
        .unwrap();
    let next = Spec {
        version: "1.1.0".into(),
        ..spec
    };
    let staged = package(a.update(&a.user, &next));
    assert_eq!(waits(&staged), vec!["Quiescence"]);
    // Give the due retry a chance to start waiting while this invocation remains live.
    std::thread::sleep(Duration::from_millis(200));
    package(a.install(&a.user, &Spec::new("unrelated-staged-pack", "1.0.0")));
    let handle = a
        .invoke("unrelated-staged-pack", "echo", InvocationKind::Ordinary)
        .expect("a staged update must not stall unrelated helper admission");
    assert!(matches!(
        finished(handle.wait(Duration::from_secs(3))),
        InvocationOutcome::Completed { .. }
    ));
    assert!(running.wait(Duration::from_millis(10)).is_none());
    running.cancel();
    a.until(
        "staged-busy-pack",
        "the scheduled retry activates without waiting for the long periodic tick",
        |r| r.version.0 == "1.1.0",
    );
}

struct HeldRuntimeLaunch(Arc<Gate>);
impl devworks_core::ports::process::ProcessHost for HeldRuntimeLaunch {
    fn platform(&self) -> &'static str {
        NativeProcessHost.platform()
    }
    fn environment(&self) -> Vec<(String, String)> {
        NativeProcessHost.environment()
    }
    fn spawn(
        &self,
        spec: &devworks_core::ports::process::ProcessSpec,
    ) -> Result<
        Box<dyn devworks_core::ports::process::ProcessTree>,
        devworks_core::ports::process::ProcessError,
    > {
        self.0.pass(IdKind::Operation);
        NativeProcessHost.spawn(spec)
    }
    fn probe(
        &self,
        identity: &devworks_core::ports::process::NativeIdentity,
    ) -> devworks_core::ports::process::Probe {
        NativeProcessHost.probe(identity)
    }
    fn terminate(
        &self,
        identity: &devworks_core::ports::process::NativeIdentity,
    ) -> devworks_core::ports::process::Probe {
        NativeProcessHost.terminate(identity)
    }
}
#[test]
fn a_runtime_probe_launched_after_shutdown_is_stopped_immediately() {
    let _s = serial();
    let h = Harness::new();
    let dir = h.root.join("late-runtime");
    std::fs::create_dir_all(&dir).unwrap();
    fake(&dir, "dotnet", "echo 8.0.4", "echo 8.0.4");
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let mut a = App::unbooted(h, Some(dir.display().to_string()), None);
    a.process_host = Some(Arc::new(HeldRuntimeLaunch(gate.clone())));
    a.probe_timeout = Some(Duration::from_secs(8));
    a.boot();
    fake(
        &dir,
        "dotnet",
        "/bin/sleep 10; echo 9.0.0",
        "\"%SystemRoot%\\System32\\ping.exe\" -n 11 127.0.0.1 >nul\r\necho 9.0.0",
    );
    gate.arm(IdKind::Operation);
    let host = a.app().packages().unwrap().clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let probe = std::thread::spawn(move || {
        let _ = tx.send(host.refresh_runtimes());
    });
    assert!(gate.entered(Duration::from_secs(5)));
    a.shutdown();
    gate.release();
    let outcome = rx.recv_timeout(Duration::from_secs(2));
    // On failure, let the original bounded probe finish so no child/test thread leaks.
    probe.join().unwrap();
    assert!(
        outcome.is_ok(),
        "a late runtime launch must be cancelled without waiting its probe timeout"
    );
    let unsettled = read(&*a.store, |s| {
        s.resources()
            .processes(&ProcessFilter::Unsettled, None, 100)
    });
    assert!(unsettled.is_empty(), "{unsettled:?}");
    assert_eq!(
        devworks_core::ports::packages::RuntimeProbe::runtime(&*a.runtimes, "dotnet").as_deref(),
        Some("8.0.4"),
        "late outcomes must not replace the runtime cache"
    );
}

#[path = "packages/approvals.rs"]
mod approvals;
