//! The engine's one boot entry point: [`boot`] recovers durable state in a fixed order, starts
//! every required consumer and artifact collection, and only then returns the [`Runtime`]
//! through which commands and queries are admitted. See the crate documentation ("Boot
//! sequence") for the order and why each step sits where it does.
use crate::dispatch::{self, Consumer, Dispatcher, DispatcherConfig, RecoveryReport};
use crate::replies::reply_consumer;
use devworks_core::context::InvocationContext;
use devworks_core::gateway::{CommandOutcome, Gateway, QueryOutcome};
use devworks_core::ports::files::FileFault;
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

/// The operation whose applied decision may leave blobs pending physical removal.
const RELEASES_BLOBS: &str = "artifacts.delete";

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
}
impl RuntimeConfig {
    pub fn new(owner: Id) -> Self {
        Self {
            dispatcher: DispatcherConfig::new(owner),
            collect_interval: Duration::from_secs(15 * 60),
            reply_max_in_flight: None,
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
/// 4. starts delivery ([`Dispatcher::run`]);
/// 5. starts artifact collection: a pass now, then periodically and after every applied
///    `artifacts.delete`;
/// 6. returns the handle through which commands are admitted.
///
/// Must be awaited on a Tokio runtime with timers; the tasks it starts run there.
pub async fn boot(composition: Composition) -> Result<Runtime, BootError> {
    let Composition {
        store,
        clock,
        ids,
        authority,
        artifacts,
        use_cases,
        mut consumers,
        config,
    } = composition;
    let use_cases = use_cases.with_artifacts(artifacts.clone());
    let mut replies = reply_consumer(
        Arc::new(ReceiptReplies::new(artifacts.clone())),
        store.clone(),
    );
    replies.max_in_flight = config.reply_max_in_flight;
    consumers.insert(0, replies);
    let dispatcher = Dispatcher::new(store.clone(), clock, consumers, config.dispatcher)
        .map_err(BootError::Composition)?;

    let queue = dispatcher.recover().await.map_err(BootError::Queue)?;
    let (svc, st) = (artifacts.clone(), store.clone());
    let recovered = tokio::task::spawn_blocking(move || svc.recover(&*st))
        .await
        .unwrap_or_else(|_| Err(panicked("artifact recovery")))
        .map_err(BootError::Artifacts)?;

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
    Ok(Runtime {
        store,
        ids,
        authority,
        artifacts,
        use_cases,
        dispatcher,
        collector,
        report: BootReport {
            queue,
            artifacts: recovered,
        },
        tasks: Mutex::new(vec![delivery, collection]),
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
    /// acknowledgment) wakes delivery for the reply intent its receipt committed, and an
    /// applied `artifacts.delete` starts a collection pass for the blobs it released.
    pub fn command_with(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
        options: WriteOptions,
    ) -> CommandOutcome {
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
        }
        if released && operation == RELEASES_BLOBS {
            self.collector.trigger.notify_one();
        }
        outcome
    }

    /// Stops admission of new deliveries and collection, then waits for the delivery loop, the
    /// collector and every in-flight delivery to finish (an asynchronous handler gets its
    /// consumer's stop grace).
    pub async fn shutdown(&self) {
        self.stop();
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap_or_else(|p| p.into_inner()));
        for task in tasks {
            let _ = task.await;
        }
        self.dispatcher.idle().await;
    }

    fn stop(&self) {
        self.dispatcher.stop();
        self.collector.stop();
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop();
    }
}
