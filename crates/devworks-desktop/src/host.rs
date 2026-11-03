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
use devworks_adapters::fs::NativeFilesystem;
use devworks_adapters::fs::artifacts::NativeArtifactFiles;
use devworks_adapters::fs::sources::{HostArtifactSources, IssueRefusal};
use devworks_adapters::paths::FsPathProbe;
use devworks_adapters::power::NativePower;
use devworks_adapters::process::NativeProcessHost;
use devworks_adapters::storage::sqlite::{SqliteConfig, SqliteStore};
use devworks_core::authority::HostRoles;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::gateway::{CommandOutcome, QueryOutcome};
use devworks_core::ports::storage::{ApplicationStore, StorageError, WriteOptions, run_write};
use devworks_core::ports::{Clock, IdKind, IdSource};
use devworks_core::protocol::{ActorId, Counter, Id, Revision};
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::history::HistoryService;
use devworks_core::services::process::ShutdownReport;
use devworks_core::services::resources::ResourceService;
use devworks_core::services::sessions::SessionService;
use devworks_core::settings::service::SettingsService;
use devworks_core::settings::view::Health;
use devworks_engine::{BootError, BootReport, Composition, Runtime, RuntimeConfig, boot};
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

/// What the composition root is configured with.
#[derive(Clone, Debug)]
pub struct DesktopConfig {
    pub data_root: PathBuf,
    /// The user's display name for attribution.
    pub user_name: String,
    /// The application build recorded with applied migrations.
    pub app_build: String,
    /// Shared bound of the owned-process stop at shutdown (AD20 default 30 s).
    pub shutdown_bound: Duration,
}
impl DesktopConfig {
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
            user_name: user_display_name(),
            app_build: concat!("devworks-desktop/", env!("CARGO_PKG_VERSION")).to_owned(),
            shutdown_bound: Duration::from_secs(30),
        }
    }
}

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
    ctx: InvocationContext,
    sources: Arc<HostArtifactSources>,
    data_root: PathBuf,
    ids: Arc<HostIds>,
    waker: Arc<Mutex<Option<Waker>>>,
    /// The same authority the runtime admits with (stream handles are re-authorized by it).
    authority: Arc<StoreAuthority>,
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
}

/// Composes the services over `store` and boots (A3). Must be awaited on a Tokio runtime with
/// timers; the tasks boot starts run there.
async fn compose(
    config: &DesktopConfig,
    store: Arc<dyn ApplicationStore>,
    clock: Arc<dyn Clock>,
    ids: Arc<HostIds>,
) -> Result<Host, StartError> {
    let id_source: Arc<dyn IdSource> = ids.clone();
    let sources = Arc::new(HostArtifactSources::new(clock.clone(), id_source.clone()));
    let artifacts = Arc::new(ArtifactService::new(
        Arc::new(NativeArtifactFiles::new(
            config.data_root.clone(),
            config.data_root.join("cache"),
        )),
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
    // The Home coordinator agent is bound by the Home chat item (DW-16); until it exists a
    // human's Home session is conducted by its creator, and no reply is ever fabricated.
    let use_cases = UseCases::new()
        .with_queue()
        .with_settings(settings)
        .with_sessions(Arc::new(SessionService::new(
            id_source.clone(),
            store.clone(),
        )))
        .with_history(Arc::new(HistoryService::new(id_source.clone())));
    let authority = Arc::new(StoreAuthority {
        store: store.clone(),
        clock: clock.clone(),
    });
    let mut runtime_config = RuntimeConfig::new(ids.next_id(IdKind::Boot));
    runtime_config.shutdown_bound = config.shutdown_bound;
    let composition = Composition {
        store: store.clone(),
        clock: clock.clone(),
        ids: id_source.clone(),
        authority: authority.clone(),
        artifacts,
        resources: Arc::new(ResourceService::new(id_source.clone(), clock.clone())),
        process_host: Arc::new(NativeProcessHost),
        power: Arc::new(NativePower),
        use_cases,
        consumers: Vec::new(),
        config: runtime_config,
    };
    // Recovery and every required consumer start here, before any request below is admitted.
    let runtime = boot(composition).await.map_err(StartError::Boot)?;
    // Host bookkeeping after recovery: the local user is the human principal of the UI.
    let user = ActorId(Id::new(USER_ACTOR).expect("valid actor"));
    let (registered, name) = (user.clone(), config.user_name.clone());
    let st = store.clone();
    tokio::task::spawn_blocking(move || {
        run_write(&*st, WriteOptions::default(), move |tx| {
            tx.actors()
                .register(&registered, PrincipalKind::Human, &name)
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
    Ok(Host {
        runtime: Arc::new(runtime),
        ctx,
        sources,
        data_root: config.data_root.clone(),
        ids,
        waker: Arc::new(Mutex::new(None)),
        authority,
    })
}

/// How the engine thread ended.
#[derive(Debug)]
pub struct ShutdownSummary {
    /// The owned-process stop and resource reconciliation ([`Runtime::shutdown`]).
    pub processes: ShutdownReport,
}

/// The running engine: its thread, its Tokio runtime and the booted [`Host`].
pub struct Engine {
    host: Arc<Host>,
    stop: Option<oneshot::Sender<()>>,
    done: mpsc::Receiver<ShutdownSummary>,
    finished: Option<ShutdownSummary>,
    thread: Option<JoinHandle<()>>,
}

impl Engine {
    /// Opens the store and boots on a dedicated engine thread; returns once recovery finished
    /// and the runtime admits requests (or why it could not).
    pub fn start(config: DesktopConfig) -> Result<Self, StartError> {
        let (booted_tx, booted_rx) = mpsc::channel::<Result<Arc<Host>, StartError>>();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<ShutdownSummary>();
        let thread = std::thread::Builder::new()
            .name("devworks-engine".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                {
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
                    let store: Arc<dyn ApplicationStore> = match opened {
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
        Ok(Self {
            host,
            stop: Some(stop_tx),
            done: done_rx,
            finished: None,
            thread: Some(thread),
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

/// The engine's Quiescing step pauses nothing yet: run controls (`runs.*`, DW-9) are not part of
/// this build. When they land, `begin_quiesce` asks the run-control service to pause every
/// active work root (recording the shutdown hold that makes them need an explicit Continue at
/// the next boot) and `quiesced` reports when those pauses committed; the shell already waits
/// for both, within its bound, before it persists and shuts the runtime down.
impl crate::shell::HostControl for Engine {
    fn begin_quiesce(&mut self) {}
    fn quiesced(&mut self) -> bool {
        true
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
    fn query(&self, operation: &str, frame: &[u8]) -> QueryOutcome {
        self.host.runtime.query(&self.host.ctx, operation, frame)
    }
    fn command(&self, operation: &str, frame: &[u8]) -> CommandOutcome {
        self.host.runtime.command(&self.host.ctx, operation, frame)
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
}
