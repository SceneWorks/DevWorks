//! `BrowserBackend`: owned browser sessions behind `browser.sessions`, `.inspect`, `.navigate`,
//! `.input`, `.upload`, `.screenshot`, `.console` and `.network` (DW-047; UIQA-01..04),
//! registered through [`crate::services::UseCases::with_browser`]. The native UI, agent tools
//! and MCP reach the same use cases through the gateway; there is no second implementation.
//!
//! # Ownership
//!
//! A session is an owned resource of the principal that created it. Create (a journaled
//! command) records the session and its launch intent; the engine's `browser_effects` consumer
//! then creates an empty owned profile directory, launches the browser **through the process
//! owner** ([`ProcessSupervisor`]: shell/process preflight, durable launch record, owned process
//! tree, bounded cancellation, boot recovery), connects over the Chrome DevTools Protocol and
//! applies the session policy (downloads denied, permissions denied, dialogs answered, popups
//! registered). Close stops the tree through the process owner and removes the profile only once
//! the tree is verified gone; an unverifiable stop leaves the session `Uncertain` with its
//! profile kept. A Close accepted while the session is still starting never settles it: the
//! session keeps `Starting` with the close request, and its Create (or the Close, once no
//! launch holds the session, or boot recovery) stops whatever was launched first. Every session
//! runs in its own profile, so concurrent sessions share no cookies, storage or cache (UIQA-04).
//! Boot recovery settles sessions of earlier boots from what process recovery verified (a
//! pending close request settles `Closed`), and removes profile roots no live session owns.
//!
//! # Authority
//!
//! The gateway's [`crate::ports::AuthorityPort`] applies role, tool selection and scope (code
//! reviewers are refused every browser operation: browser reads are not verified reads, and
//! they never mutate). On top of it, a session is visible only within its scope (its project,
//! or for a workspace session its owner, the user and the host), and only its owner, the user
//! or the host may drive or close it. Page content never grants authority: the host decides
//! from host records only.
//!
//! # Commands, receipts and unknown outcomes
//!
//! Navigation, input and upload are external effects: `decide` validates the session (state,
//! expected revision) and the referenced target, frame, document and snapshot node against the
//! live view, then records an operation and a delivery ordered per session, and answers
//! `Accepted`. The effect runs after commit under the shared operation journal (`Dispatching`
//! is committed before it starts). A stale reference found at execution is a deterministic
//! failure with nothing sent; a request sent without a response (timeout, transport loss) is
//! `OutcomeUnknown`: it is never re-executed, the session records it, and further navigation,
//! input and upload are refused (`OutcomeUnknown`, reconcile) until it is reconciled. A replayed
//! request key returns the original receipt and never redispatches. Typed results are read with
//! [`BrowserService::completion`].
//!
//! # Evidence
//!
//! Accessibility snapshots, screenshots, console and network notification batches and action
//! observations are artifacts of the session's scope (DW-007), linked to the creating work root
//! and assignment and attributed to the session's principal and agent session (UIQA-03). They are
//! bounded and redacted; no header, cookie, body or page script result is captured.
//!
//! # Honest limits
//!
//! The DevTools channel is the browser's private pipe (`--remote-debugging-pipe`, anonymous
//! pipes the process owner hands over); the browser opens no DevTools port. Out-of-process
//! (cross-site) frames attach as flattened child sessions: they are inspected, driven and
//! revisioned like same-process frames, and clicks into them are placed through their owner
//! element's content box (a rotated or skewed frame owner is not supported). Worktree-bound
//! invocations resolve their physical checkout and current SHA/dirty state through the Git
//! service; sessions retain their start source and each observation retains its own source.
//! Unbound invocations record no repository revision.
pub mod cdp;
pub mod live;

use crate::authority::destructive::{FilesystemResolver, ProtectedRoots};
use crate::authority::redact::Redactor;
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, browser as ops};
use crate::ports::browser::{
    BrowserAsset, BrowserPlatform, Profile, Qualification, STAGED_NAME_MAX_BYTES,
};
use crate::ports::storage::{
    ApplicationStore, ArtifactScope, BrowserChannel, BrowserFilter, BrowserPhase, MAX_PAGE,
    NewEvent, NewOperation, NewQueueEntry, OperationRecord, ReadOptions, ReadSnapshot,
    StorageError, StoredBrowserObservation, StoredBrowserSession, WriteOptions, WriteTransaction,
    read_snapshot, run_write,
};
use crate::ports::{
    Clock, CommandHandler, Decided, HandlerFailure, IdKind, IdSource, Preparation, Prepared,
    QueryAnswer, QueryHandler,
};
use crate::protocol::data::DataScope;
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{
    BrowserInput, BrowserLocation, BrowserNode, BrowserObservation, BrowserSession,
    BrowserSessionAction, BrowserSnapshot, BrowserTarget, ItemState,
};
use crate::protocol::results::{
    DomainCommandReply, DomainDecision, OperationCompletion, OperationResult,
};
use crate::protocol::support::{BoundedList, BoundedText, Page};
use crate::protocol::{
    CommandReply, CorrelationId, DomainErrorCode, DomainFault, EventId, Id, OperationId,
    OperationState, Protocol, RetryAdvice, Revision, RevisionDimension,
};
use crate::services::artifacts::ArtifactService;
use crate::services::artifacts::evidence::{Evidence, EvidenceFailure, safe_name};
use crate::services::cursor::CursorBinding;
use crate::services::diagnostics::HealthCheck;
use crate::services::process::{
    LaunchPolicy, LaunchRefusal, LaunchRequest, OwnedProcess, ProcessSupervisor, StopOutcome,
};
use crate::services::sessions::{
    Failure, Refusal, counter, decided, project_visible, query_fault, require_project, rev,
};
use live::{ActionError, Bounds, Live, post_dispatch};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// The operations [`BrowserService`] is registered for.
pub const OPERATIONS: [&str; 8] = [
    "browser.sessions",
    "browser.inspect",
    "browser.navigate",
    "browser.input",
    "browser.upload",
    "browser.screenshot",
    "browser.console",
    "browser.network",
];
/// History aggregate of a session's lifecycle (`aggregate_revision` = session revision).
pub const SESSION_AGGREGATE: &str = "browser_session";
/// History aggregate of one requested action (`aggregate_id` = its operation).
pub const ACTION_AGGREGATE: &str = "browser_action";
/// The durable delivery consumer that executes journaled browser effects.
pub const EFFECT_CONSUMER: &str = "browser_effects";
/// Journal capabilities.
pub const CREATE: &str = "browser.create";
pub const CLOSE: &str = "browser.close";
pub const NAVIGATE: &str = "browser.navigate";
pub const INPUT: &str = "browser.input";
pub const UPLOAD: &str = "browser.upload";
/// Fault target kinds.
pub const SESSION_STATE: &str = "browser_session_state";
pub const SESSION_OWNER: &str = "browser_session_owner";
pub const ASSET: &str = "browser_asset";

/// Bounds of browser work.
#[derive(Clone, Copy, Debug)]
pub struct BrowserConfig {
    /// Launch until the DevTools endpoint answers.
    pub launch_timeout: Duration,
    /// One protocol request.
    pub call_timeout: Duration,
    /// Waiting for a navigation's load.
    pub navigation_timeout: Duration,
    /// How long an input waits for its observable effects.
    pub settle: Duration,
    /// Cooperative stop period of a browser tree, within `close_bound`.
    pub close_grace: Duration,
    pub close_bound: Duration,
    /// Largest file one upload or artifact page materializes.
    pub stage_max_bytes: u64,
    /// How long an action waits for the session's previous action to settle before it is
    /// retried later (nothing is sent meanwhile).
    pub order_bound: Duration,
}
impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            launch_timeout: Duration::from_secs(20),
            call_timeout: Duration::from_secs(10),
            navigation_timeout: Duration::from_secs(20),
            settle: Duration::from_millis(300),
            close_grace: Duration::from_secs(2),
            close_bound: Duration::from_secs(10),
            stage_max_bytes: 256 * 1024 * 1024,
            order_bound: Duration::from_secs(120),
        }
    }
}
impl BrowserConfig {
    fn bounds(&self) -> Bounds {
        Bounds {
            call: self.call_timeout,
            navigation: self.navigation_timeout,
            settle: self.settle,
        }
    }
}

/// Observed outcome of one journaled browser effect (the engine maps it onto its effect
/// result).
#[derive(Clone, Debug, PartialEq)]
pub enum EffectOutcome {
    /// The effect happened; the value carries the typed result (`{"result": OperationResult}`).
    Completed(Value),
    /// Deterministic failure; nothing (or nothing more) to retry. The text is a JSON fault.
    Failed(String),
    /// Evidence that nothing happened; safe to retry.
    NotApplied(String),
    /// The effect may or may not have happened.
    Unknown(String),
}

/// Launch flags of every owned browser (headless, empty profile, the private DevTools pipe and
/// no DevTools port, no first-run, sync, background networking, extensions, keychain or prompts).
pub fn launch_args(profile: &Profile) -> Vec<String> {
    vec![
        "--headless=new".into(),
        "--remote-debugging-pipe".into(),
        format!("--user-data-dir={}", profile.user_data.display()),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-background-networking".into(),
        "--disable-component-update".into(),
        "--disable-sync".into(),
        "--disable-crash-reporter".into(),
        "--disable-breakpad".into(),
        "--disable-default-apps".into(),
        "--disable-extensions".into(),
        "--disable-features=Translate,MediaRouter,OptimizationHints".into(),
        "--metrics-recording-only".into(),
        "--password-store=basic".into(),
        "--use-mock-keychain".into(),
        "--deny-permission-prompts".into(),
        "--mute-audio".into(),
        "about:blank".into(),
    ]
}

/// One session's live state and the flag that interrupts its in-flight action.
struct Slot {
    closing: AtomicBool,
    live: Mutex<Option<Live>>,
    /// The version of the last target list recorded; held while a list is written, so lists
    /// are recorded in version order whatever order their writers reach the store.
    recorded: Mutex<u64>,
}

/// What `decide` may check without waiting for an in-flight action: the live target, frame,
/// document and snapshot revisions, refreshed whenever the live state changes.
#[derive(Clone, Debug, Default)]
struct View {
    profile: Option<Profile>,
    targets: HashMap<String, TargetView>,
}
#[derive(Clone, Debug)]
struct TargetView {
    frames: Vec<String>,
    document_revision: i64,
    crashed: bool,
    snapshots: Vec<(i64, String, i64, HashSet<String>)>,
}
fn view_of(live: &Live) -> View {
    View {
        profile: Some(live.profile.clone()),
        targets: live
            .targets
            .iter()
            .filter(|t| !t.closed)
            .map(|t| {
                (
                    t.id.clone(),
                    TargetView {
                        frames: t.frames.iter().map(|f| f.host.clone()).collect(),
                        document_revision: t.document_revision,
                        crashed: t.crashed,
                        snapshots: t
                            .snapshots
                            .iter()
                            .map(|s| {
                                (
                                    s.revision,
                                    s.frame.clone(),
                                    s.document_revision,
                                    s.nodes.keys().cloned().collect(),
                                )
                            })
                            .collect(),
                    },
                )
            })
            .collect(),
    }
}

fn stale(kind: &'static str) -> Refusal {
    Refusal::new(DomainErrorCode::SourceChanged, kind, None)
        .retry(RetryAdvice::RefreshAndNewRequest)
}

/// A reference that names another session than the one the request selected.
fn other_session(t: &BrowserTarget, session: &Id) -> Result<(), Refusal> {
    if &t.session_id != session {
        return Err(Refusal::new(
            DomainErrorCode::InvalidArgument,
            "browser_target_session",
            Some(t.session_id.clone()),
        ));
    }
    Ok(())
}

impl View {
    fn target(&self, t: &BrowserTarget) -> Result<(), Refusal> {
        let Some(v) = self.targets.get(t.target_id.as_str()) else {
            return Err(stale("browser_target"));
        };
        if v.crashed {
            return Err(stale("browser_target"));
        }
        if !v.frames.iter().any(|f| f == t.frame_id.as_str()) {
            return Err(stale("browser_frame"));
        }
        if v.document_revision != t.document_revision.get() {
            return Err(stale("browser_document"));
        }
        Ok(())
    }
    fn node(&self, n: &BrowserNode) -> Result<(), Refusal> {
        self.target(&n.target)?;
        let v = &self.targets[n.target.target_id.as_str()];
        match v
            .snapshots
            .iter()
            .find(|s| s.0 == n.snapshot_revision.get())
        {
            Some((_, frame, doc, nodes))
                if *doc == v.document_revision
                    && frame == n.target.frame_id.as_str()
                    && nodes.contains(n.node_id.as_str()) =>
            {
                Ok(())
            }
            Some(_) => Err(stale("browser_node")),
            None => Err(stale("browser_snapshot")),
        }
    }
}

/// What `prepare` staged for an upload or an artifact page.
#[derive(Debug)]
enum Staged {
    /// Materialized files (absolute paths) in the batch directory of the session root.
    Files {
        session: Id,
        profile: Profile,
        batch: String,
        paths: Vec<String>,
        tested_repository: Option<Value>,
        binding: Option<Value>,
    },
    Source {
        tested_repository: Option<Value>,
        binding: Option<Value>,
    },
    /// A file could not be granted; `decide` records the refusal.
    Refused(Refusal),
}

fn staged_source(command: &AdmittedCommand) -> (Option<Value>, Option<Value>) {
    match command.prepared::<Staged>() {
        Some(
            Staged::Source {
                tested_repository,
                binding,
            }
            | Staged::Files {
                tested_repository,
                binding,
                ..
            },
        ) => (tested_repository.clone(), binding.clone()),
        _ => (None, None),
    }
}

/// The browser domain service.
pub struct BrowserService {
    store: Arc<dyn ApplicationStore>,
    platform: Arc<dyn BrowserPlatform>,
    artifacts: Arc<ArtifactService>,
    processes: Arc<ProcessSupervisor>,
    fs: Arc<dyn FilesystemResolver>,
    app_data_root: Option<String>,
    ids: Arc<dyn IdSource>,
    clock: Arc<dyn Clock>,
    config: BrowserConfig,
    git_source: Option<Arc<crate::services::git::GitService>>,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    views: Mutex<HashMap<String, View>>,
    assets: RwLock<Vec<BrowserAsset>>,
    /// Assets verified by a launch handshake in this boot: product and protocol.
    verified: Mutex<HashMap<String, (String, String)>>,
    /// The latest launch failure per asset in this boot.
    launch_failures: Mutex<HashMap<String, String>>,
}

fn owner_kind(k: PrincipalKind) -> &'static str {
    match k {
        PrincipalKind::Human => "Human",
        PrincipalKind::Agent => "Agent",
        PrincipalKind::Package => "Package",
        PrincipalKind::McpClient => "McpClient",
        PrincipalKind::Schedule => "Schedule",
        PrincipalKind::Host => "Host",
    }
}
fn kind_of(s: &str) -> PrincipalKind {
    match s {
        "Human" => PrincipalKind::Human,
        "Package" => PrincipalKind::Package,
        "McpClient" => PrincipalKind::McpClient,
        "Schedule" => PrincipalKind::Schedule,
        "Host" => PrincipalKind::Host,
        _ => PrincipalKind::Agent,
    }
}

/// The wire state of a durable phase: `Partial` while starting, `Healthy` when ready,
/// `Disabled` once closed, `Failed` after a failure or crash, and `Unavailable` until a
/// requested stop is verified (including a cancelled cleanup that needs another Close).
pub fn wire_state(p: BrowserPhase) -> ItemState {
    match p {
        BrowserPhase::Starting => ItemState::Partial,
        BrowserPhase::Ready => ItemState::Healthy,
        BrowserPhase::Closed => ItemState::Disabled,
        BrowserPhase::Failed => ItemState::Failed,
        BrowserPhase::Stopping | BrowserPhase::Uncertain => ItemState::Unavailable,
    }
}

/// The unsettled launch record of a session's create operation: the tree it launched when the
/// session itself does not name it.
fn launch_record<R: crate::ports::storage::ResourceRead + ?Sized>(
    r: &R,
    create: &OperationId,
) -> Result<Option<crate::ports::storage::StoredProcess>, StorageError> {
    let mut after: Option<Id> = None;
    loop {
        let page = r.processes(
            &crate::ports::storage::ProcessFilter::Unsettled,
            after.as_ref(),
            100,
        )?;
        if let Some(p) = page
            .iter()
            .find(|p| p.operation_id.as_ref() == Some(create))
        {
            return Ok(Some(p.clone()));
        }
        if page.len() < 100 {
            return Ok(None);
        }
        after = page.last().map(|p| p.id.clone());
    }
}

/// The wire record of a session. `processOwnerId` is the owned process tree once launched, and
/// the journaled Create operation that will own it before that.
pub fn session_record(s: &StoredBrowserSession) -> BrowserSession {
    BrowserSession {
        id: s.id.clone(),
        revision: rev(s.revision),
        state: wire_state(s.phase),
        target_ids: BoundedList(
            s.targets
                .iter()
                .filter_map(|t| Id::new(t.clone()).ok())
                .collect(),
        ),
        tested_repository: s
            .tested_repository
            .clone()
            .map(serde_json::from_value)
            .transpose()
            .expect("validated stored browser source"),
        process_owner_id: s
            .process_id
            .clone()
            .unwrap_or_else(|| s.create_operation_id.0.clone()),
    }
}
fn session_page(s: &StoredBrowserSession) -> Page<BrowserSession> {
    Page {
        items: BoundedList(vec![session_record(s)]),
        next_cursor: None,
        has_more: false,
    }
}

/// A recorded tree by its process ID (the process owner resolves its identity).
fn by_id(id: Id) -> OwnedProcess {
    OwnedProcess {
        id,
        identity: crate::ports::process::NativeIdentity {
            pid: 0,
            birth: String::new(),
            group: None,
        },
        lease: None,
    }
}

fn detail(s: &str) -> String {
    Redactor::new().redact_text(s).chars().take(1000).collect()
}

/// A deterministic failure as a JSON fault description (rebuilt by [`BrowserService::completion`]).
fn failed(code: DomainErrorCode, kind: &str, why: &str) -> EffectOutcome {
    EffectOutcome::Failed(
        json!({"code": format!("{code:?}"), "kind": kind, "detail": detail(why)}).to_string(),
    )
}

/// Whether `ctx` may see `s`: a project session within its project, a workspace session by its
/// owner, the user and the host.
fn visible(ctx: &InvocationContext, s: &StoredBrowserSession) -> bool {
    match &s.project_id {
        Some(p) => project_visible(ctx, Some(p)),
        None => {
            ctx.project_id().is_none()
                && (matches!(
                    ctx.principal_kind(),
                    PrincipalKind::Human | PrincipalKind::Host
                ) || ctx.principal() == &s.owner)
        }
    }
}
/// Whether `ctx` may drive or close `s`: its owner, the user or the host.
fn may_drive(ctx: &InvocationContext, s: &StoredBrowserSession) -> bool {
    matches!(
        ctx.principal_kind(),
        PrincipalKind::Human | PrincipalKind::Host
    ) || ctx.principal() == &s.owner
}

fn session_of(
    read: &dyn crate::ports::storage::BrowserRead,
    ctx: &InvocationContext,
    id: &Id,
) -> Result<StoredBrowserSession, Failure> {
    match read.browser_session(id)? {
        Some(s) if visible(ctx, &s) => Ok(s),
        _ => Err(Refusal::new(
            DomainErrorCode::NotFound,
            "browser_session",
            Some(id.clone()),
        )
        .into()),
    }
}

fn record_event(
    tx: &mut dyn WriteTransaction,
    actor: &crate::protocol::ActorId,
    s: &StoredBrowserSession,
    operation: Option<&OperationId>,
    aggregate: (&str, &str, i64),
    kind: &str,
    payload: Value,
) -> Result<EventId, StorageError> {
    let id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id: id.clone(),
        actor: actor.clone(),
        project_id: s.project_id.clone(),
        root_id: s.root_id.clone(),
        operation_id: operation.cloned(),
        aggregate_kind: aggregate.0.into(),
        aggregate_id: aggregate.1.into(),
        aggregate_revision: aggregate.2,
        kind: kind.into(),
        payload,
    })?;
    Ok(id)
}

impl BrowserService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        platform: Arc<dyn BrowserPlatform>,
        artifacts: Arc<ArtifactService>,
        processes: Arc<ProcessSupervisor>,
        fs: Arc<dyn FilesystemResolver>,
        app_data_root: Option<String>,
        ids: Arc<dyn IdSource>,
        clock: Arc<dyn Clock>,
        config: BrowserConfig,
    ) -> Self {
        let assets = platform.assets();
        Self {
            store,
            platform,
            artifacts,
            processes,
            fs,
            app_data_root,
            ids,
            clock,
            config,
            git_source: None,
            slots: Mutex::new(HashMap::new()),
            views: Mutex::new(HashMap::new()),
            assets: RwLock::new(assets),
            verified: Mutex::new(HashMap::new()),
            launch_failures: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_git_source(
        mut self,
        source: Option<Arc<crate::services::git::GitService>>,
    ) -> Self {
        self.git_source = source;
        self
    }
    fn tested_source(
        &self,
        ctx: &InvocationContext,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<Option<Value>, Failure> {
        if ctx.worktree().is_none() {
            return Ok(None);
        }
        self.git_source
            .as_ref()
            .ok_or_else(|| {
                Refusal::new(DomainErrorCode::UnsupportedCapability, "git_source", None)
            })?
            .observe_source(ctx, stop)
    }
    fn binding_source(
        &self,
        binding: Option<&Value>,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<Option<Value>, Failure> {
        let Some(binding) = binding else {
            return Ok(None);
        };
        let binding: crate::services::git::context::SourceBinding =
            serde_json::from_value(binding.clone())
                .map_err(|_| StorageError::InvalidRecord("browser source binding".into()))?;
        self.tested_source(&binding.context(&*self.store)?, stop)
    }
    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }

    /// Re-reads the browser assets of this target (discovery reads the filesystem only).
    pub fn refresh_assets(&self) {
        let assets = self.platform.assets();
        *self.assets.write().unwrap_or_else(|p| p.into_inner()) = assets;
    }
    /// The browser assets of this target as last discovered.
    pub fn assets(&self) -> Vec<BrowserAsset> {
        self.assets
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    fn asset(&self, id: &str) -> Option<BrowserAsset> {
        self.assets().into_iter().find(|a| a.id == id)
    }

    fn slot(&self, id: &Id) -> Arc<Slot> {
        self.slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(id.as_str().to_owned())
            .or_insert_with(|| {
                Arc::new(Slot {
                    closing: AtomicBool::new(false),
                    recorded: Mutex::new(0),
                    live: Mutex::new(None),
                })
            })
            .clone()
    }
    fn existing_slot(&self, id: &Id) -> Option<Arc<Slot>> {
        self.slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id.as_str())
            .cloned()
    }
    fn forget(&self, id: &Id) {
        self.slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id.as_str());
        self.views
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id.as_str());
    }
    fn publish_view(&self, live: &Live) {
        self.views
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(live.session.as_str().to_owned(), view_of(live));
    }
    fn view(&self, id: &Id) -> Option<View> {
        self.views
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id.as_str())
            .cloned()
    }

    /// Sessions this boot holds a running browser for.
    pub fn live_sessions(&self) -> Vec<Id> {
        let mut ids: Vec<Id> = self
            .views
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .filter_map(|k| Id::new(k.clone()).ok())
            .collect();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        ids
    }

    /// The host context a journaled effect runs under: the session's principal and lineage.
    fn host_context(&self, s: &StoredBrowserSession, op: &OperationId) -> InvocationContext {
        let info = self.store.info();
        let mut ctx = InvocationContext::new(
            s.owner.clone(),
            kind_of(&s.owner_kind),
            op.0.clone(),
            Id::new("browser-effects").expect("namespace"),
            info.store_id,
            counter(info.restore_epoch),
            rev(1),
        )
        .with_lineage(
            s.project_id.clone(),
            s.root_id.clone(),
            s.assignment_id.clone(),
        );
        if let Some(a) = &s.agent_session_id {
            ctx = ctx.with_session(a.clone());
        }
        ctx
    }

    fn read_session(&self, id: &Id) -> Result<Option<StoredBrowserSession>, StorageError> {
        let id = id.clone();
        read_snapshot(&*self.store, &ReadOptions::default(), move |s| {
            s.browser().browser_session(&id)
        })
    }

    /// Applies `change` to the stored session in its own write (compare-and-set, retried on a
    /// concurrent change), recording one host-attributed lifecycle event. `change` returns the
    /// event to record, or `None` to leave the session unchanged.
    fn update<F>(&self, id: &Id, change: F) -> Result<Option<StoredBrowserSession>, StorageError>
    where
        F: Fn(&mut StoredBrowserSession) -> Option<(&'static str, Value)> + Send + Sync + 'static,
    {
        let change = Arc::new(change);
        for _ in 0..4 {
            let (id, change) = (id.clone(), change.clone());
            let r = run_write(&*self.store, WriteOptions::default(), move |tx| {
                let Some(current) = tx.browser().browser_session(&id)? else {
                    return Ok(None);
                };
                if current.phase.settled() {
                    return Ok(Some(current));
                }
                let mut next = current.clone();
                let Some((kind, payload)) = change(&mut next) else {
                    return Ok(Some(current));
                };
                next.revision = current.revision + 1;
                tx.browser()
                    .update_browser_session(&next, current.revision)?;
                let host = tx.store().host_actor.clone();
                let mut payload = payload;
                if let Value::Object(m) = &mut payload {
                    m.insert("state".into(), json!(next.phase.as_str()));
                    m.insert("ownerId".into(), json!(next.owner.0.as_str()));
                }
                record_event(
                    tx,
                    &host,
                    &next,
                    None,
                    (SESSION_AGGREGATE, next.id.as_str(), next.revision),
                    kind,
                    payload,
                )?;
                Ok(Some(next))
            });
            match r {
                Err(StorageError::LeaseLost { .. }) => continue,
                other => return other,
            }
        }
        Err(StorageError::Busy)
    }

    fn fail_session(&self, id: &Id, why: &str, phase: BrowserPhase) {
        self.settle(id, why, phase, None);
    }

    /// Records `phase` (`Closed`, `Failed` or `Uncertain`) on a session that may still own a
    /// browser, naming `process` as its tree when the session names none yet (an unverified
    /// stop stays associated with the tree a later Close or recovery must stop). A session
    /// whose Close was accepted settles `Closed` rather than `Failed`: its browser is gone
    /// either way, and the Close is what was asked for.
    fn settle(&self, id: &Id, why: &str, phase: BrowserPhase, process: Option<&Id>) {
        let why = why.to_owned();
        let process = process.cloned();
        let _ = self.update(id, move |s| {
            if !s.phase.live() && s.phase != BrowserPhase::Uncertain {
                return None;
            }
            let (phase, why) = match phase {
                BrowserPhase::Failed if s.close_operation_id.is_some() => (
                    BrowserPhase::Closed,
                    detail(&format!("closed as requested; {why}")),
                ),
                _ => (phase, detail(&why)),
            };
            s.phase = phase;
            s.detail = Some(why.clone());
            if s.process_id.is_none() {
                s.process_id = process.clone();
            }
            Some((
                match phase {
                    BrowserPhase::Uncertain => "BrowserSessionUncertain",
                    BrowserPhase::Closed => "BrowserSessionClosed",
                    _ => "BrowserSessionFailed",
                },
                json!({"detail": why}),
            ))
        });
    }

    // -----------------------------------------------------------------------------------------
    // Lifecycle effects

    /// Executes one journaled browser effect (called by the engine's effect consumer after
    /// `Dispatching` was committed). It performs the effect at most once.
    pub fn execute(&self, op: &OperationRecord) -> EffectOutcome {
        let Some(session) = op
            .payload
            .get("sessionId")
            .and_then(Value::as_str)
            .and_then(|s| Id::new(s).ok())
        else {
            return failed(
                DomainErrorCode::InvalidArgument,
                "operation",
                "no session in the operation",
            );
        };
        match op.capability.as_str() {
            CREATE => self.run_create(&session, op),
            CLOSE => self.run_close(&session, op),
            NAVIGATE | INPUT | UPLOAD => self.run_action(&session, op),
            other => failed(DomainErrorCode::UnsupportedCapability, "operation", other),
        }
    }

    /// Observes an operation whose outcome is unknown without performing it. A browser action's
    /// effect cannot be established after the fact (page state is not a receipt), so it stays
    /// unknown for explicit reconciliation.
    pub fn observe(&self, op: &OperationRecord) -> EffectOutcome {
        EffectOutcome::Unknown(format!(
            "{} cannot be observed after the fact; reconcile it explicitly",
            op.capability
        ))
    }

    fn run_create(&self, id: &Id, op: &OperationRecord) -> EffectOutcome {
        let slot = self.slot(id);
        let mut guard = slot.live.lock().unwrap_or_else(|p| p.into_inner());
        let s = match self.read_session(id) {
            Ok(Some(s)) => s,
            Ok(None) => {
                return failed(
                    DomainErrorCode::NotFound,
                    "browser_session",
                    "unknown session",
                );
            }
            Err(e) => return EffectOutcome::NotApplied(e.to_string()),
        };
        if s.phase != BrowserPhase::Starting || guard.is_some() {
            if guard.is_none() {
                drop(guard);
                self.forget(id);
            }
            return failed(
                DomainErrorCode::Conflict,
                SESSION_STATE,
                "the session was closed before its browser was launched",
            );
        }
        // Its Close was accepted before anything was launched: nothing starts, and the close
        // settles here, under the slot, so no launch can begin meanwhile.
        if s.close_operation_id.is_some() {
            let closed = self.finish_close(id, &s, "closed before its browser was launched");
            drop(guard);
            return match closed {
                Ok(()) => failed(
                    DomainErrorCode::Conflict,
                    SESSION_STATE,
                    "the session was closed before its browser was launched",
                ),
                Err(e) => EffectOutcome::NotApplied(e.to_string()),
            };
        }
        if !self.owner_active(&s) {
            let why = "the session's owner no longer has authority";
            self.fail_session(id, why, BrowserPhase::Failed);
            drop(guard);
            self.forget(id);
            return failed(DomainErrorCode::PermissionDenied, SESSION_OWNER, why);
        }
        let Some(asset) = self
            .asset(&s.asset_id)
            .filter(|a| a.qualification == Qualification::Present)
        else {
            let why = format!(
                "browser asset {} is not available on this target",
                s.asset_id
            );
            self.fail_session(id, &why, BrowserPhase::Failed);
            drop(guard);
            self.forget(id);
            return failed(DomainErrorCode::UnsupportedTarget, ASSET, &why);
        };
        let profile = match self.platform.create_profile(id) {
            Ok(p) => p,
            Err(e) => {
                let why = format!("the profile could not be created: {e}");
                self.fail_session(id, &why, BrowserPhase::Failed);
                drop(guard);
                self.forget(id);
                return failed(DomainErrorCode::ExternalFailure, "browser_profile", &why);
            }
        };
        let spec = self
            .platform
            .command(&asset, &profile, &launch_args(&profile));
        let ctx = self.host_context(&s, &op.id);
        let roots = match ProtectedRoots::read(&*self.store, vec![], self.app_data_root.clone()) {
            Ok(r) => r,
            Err(e) => {
                let _ = self.platform.remove_profile(id);
                self.fail_session(id, &e.to_string(), BrowserPhase::Failed);
                drop(guard);
                self.forget(id);
                return EffectOutcome::NotApplied(e.to_string());
            }
        };
        let policy = LaunchPolicy {
            fs: &*self.fs,
            roots: &roots,
            protected_branches: &[],
        };
        let launched = self.processes.launch(
            &ctx,
            LaunchRequest {
                spec,
                operation_id: Some(op.id.clone()),
                lease: None,
                approved_hold: false,
            },
            &policy,
        );
        let process = match launched {
            Ok(p) => p,
            Err(LaunchRefusal::ProjectClosed) => {
                // The project was archived or deleted: no browser starts, and the asset is fine.
                let why = "the session's project is archived or deleted".to_owned();
                let _ = self.platform.remove_profile(id);
                self.fail_session(id, &why, BrowserPhase::Failed);
                drop(guard);
                self.forget(id);
                return failed(DomainErrorCode::Conflict, "archived_project", &why);
            }
            Err(r) => {
                let why = match r {
                    LaunchRefusal::ProjectClosed => unreachable!("handled above"),
                    LaunchRefusal::Blocked(_) => {
                        "the launch was blocked by the process preflight".to_owned()
                    }
                    LaunchRefusal::NeedsInput(_) => "the launch needs an approval".to_owned(),
                    LaunchRefusal::GitPushOwnerRequired => {
                        "Git pushes must use the owned Git service".to_owned()
                    }
                    LaunchRefusal::Resource(f) => format!("the launch was refused: {f:?}"),
                    LaunchRefusal::Spawn(e) => format!("the browser could not be started: {e}"),
                    LaunchRefusal::Storage(e) => format!("the launch could not be recorded: {e}"),
                };
                let _ = self.platform.remove_profile(id);
                self.note_launch_failure(&s.asset_id, &why);
                self.fail_session(id, &why, BrowserPhase::Failed);
                drop(guard);
                self.forget(id);
                return failed(DomainErrorCode::ExternalFailure, ASSET, &why);
            }
        };
        let pid = process.id.clone();
        let recorded = self.update(id, move |s| {
            s.process_id = Some(pid.clone());
            Some(("BrowserProcessLaunched", json!({"processId": pid.as_str()})))
        });
        // A Close accepted while the browser was launching (the session kept `Starting` with
        // the request): the tree is stopped now, and only a verified stop settles `Closed`.
        if let Ok(Some(r)) = &recorded
            && r.close_operation_id.is_some()
        {
            let why = "closed while the browser was starting";
            self.teardown(id, &process, None, why, BrowserPhase::Closed);
            drop(guard);
            return failed(DomainErrorCode::Conflict, SESSION_STATE, why);
        }
        let control = self.processes.take_control(&process);
        let transport = match self.platform.connect(&profile, control) {
            Ok(t) => t,
            Err(e) => {
                let why = format!("the browser's DevTools pipe is not available: {e}");
                self.note_launch_failure(&s.asset_id, &why);
                self.teardown(id, &process, None, &why, BrowserPhase::Failed);
                return failed(DomainErrorCode::ExternalFailure, ASSET, &why);
            }
        };
        let mut live = Live::new(
            id.clone(),
            profile.clone(),
            process.clone(),
            cdp::Cdp::new(transport),
        );
        if let Err(e) = live.setup(&self.config.bounds(), self.config.launch_timeout) {
            let why = format!("the browser session could not be configured: {e}");
            self.note_launch_failure(&s.asset_id, &why);
            self.teardown(id, &process, Some(live), &why, BrowserPhase::Failed);
            return failed(DomainErrorCode::ExternalFailure, ASSET, &why);
        }
        self.verified
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                s.asset_id.clone(),
                (live.product.clone(), live.protocol.clone()),
            );
        let (targets, policy, product, protocol) = (
            live.open_targets(),
            live.policy.clone(),
            live.product.clone(),
            live.protocol.clone(),
        );
        live.targets_changed = false;
        let ready = self.update(id, move |s| {
            if s.phase != BrowserPhase::Starting || s.close_operation_id.is_some() {
                return None;
            }
            s.phase = BrowserPhase::Ready;
            s.targets = targets.clone();
            Some((
                "BrowserSessionReady",
                json!({"product": product, "protocol": protocol, "policy": policy, "targets": targets}),
            ))
        });
        match ready {
            Ok(Some(r)) if r.phase == BrowserPhase::Ready => {
                self.publish_view(&live);
                *guard = Some(live);
                completed(OperationResult::BrowserSessions {
                    value: session_page(&r),
                })
            }
            // Close was accepted while the browser started: stop it now. The slot stays held
            // until the session settled, so the Close sees the outcome, never a half-stop.
            Ok(Some(_)) => {
                let why = "closed while the browser was starting";
                self.teardown(id, &process, Some(live), why, BrowserPhase::Closed);
                drop(guard);
                failed(DomainErrorCode::Conflict, SESSION_STATE, why)
            }
            Ok(None) | Err(_) => {
                let why = "the ready session could not be recorded";
                self.teardown(id, &process, Some(live), why, BrowserPhase::Failed);
                drop(guard);
                EffectOutcome::Unknown(why.into())
            }
        }
    }

    fn note_launch_failure(&self, asset: &str, why: &str) {
        self.launch_failures
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(asset.to_owned(), detail(why));
    }

    /// Stops a session's browser through the process owner and settles the record: `then`
    /// (Closed or Failed) with its profile removed once the tree is verified gone, `Uncertain`
    /// with the profile kept otherwise. Returns the phase recorded.
    fn teardown(
        &self,
        id: &Id,
        process: &OwnedProcess,
        live: Option<Live>,
        why: &str,
        then: BrowserPhase,
    ) -> BrowserPhase {
        if let Some(mut l) = live {
            // What the page reported until now stays evidence after the browser is gone.
            let _ = l.pump(Duration::from_millis(20), &self.config.bounds());
            self.flush_notices(id, &mut l);
            let _ = l
                .cdp
                .call(None, "Browser.close", json!({}), Duration::from_secs(1));
            l.cdp.close();
        }
        let stop = self
            .processes
            .cancel(process, self.config.close_grace, self.config.close_bound);
        let outcome = match stop {
            Ok(o) => o,
            Err(f) => f.outcome,
        };
        let (phase, why) = match outcome {
            StopOutcome::Reaped { .. } => match self.platform.remove_profile(id) {
                Ok(()) => (then, why.to_owned()),
                Err(e) => (then, format!("{why}; the profile was not removed: {e}")),
            },
            StopOutcome::Uncertain(u) => (
                BrowserPhase::Uncertain,
                format!("{why}; the browser could not be verified stopped: {u}"),
            ),
        };
        self.settle(id, &why, phase, Some(&process.id));
        self.forget(id);
        phase
    }

    /// Finishes the Close of a session no live connection holds (called with its slot held,
    /// so no launch starts meanwhile). The tree it launched, if any (the one it names, or the
    /// one its create operation's launch record names), is stopped through the process owner
    /// first: only a verified stop settles `Closed` and removes the profile, an unverified one
    /// leaves it `Uncertain`. With no tree, it closes and its profile is removed.
    fn finish_close(
        &self,
        id: &Id,
        s: &StoredBrowserSession,
        why: &str,
    ) -> Result<(), StorageError> {
        match self.launched_process(s)? {
            Some(p) => {
                self.teardown(id, &by_id(p.id), None, why, BrowserPhase::Closed);
            }
            None => {
                let _ = self.platform.remove_profile(id);
                self.fail_session(id, why, BrowserPhase::Closed);
                self.forget(id);
            }
        }
        Ok(())
    }

    /// Publishes the buffered console and network notifications of `live` as observations.
    fn flush_notices(&self, id: &Id, live: &mut Live) {
        let Ok(Some(mut s)) = self.read_session(id) else {
            return;
        };
        // Teardown is unbound resource maintenance, not a UI-test invocation. Never start
        // Git reads while closing, and never substitute the session's old start revision.
        s.tested_repository = None;
        let dropped = std::mem::take(&mut live.cdp.dropped) > 0;
        for channel in [BrowserChannel::Console, BrowserChannel::Network] {
            let buffer = match channel {
                BrowserChannel::Network => &mut live.network,
                _ => &mut live.console,
            };
            if buffer.is_empty() {
                continue;
            }
            // The browser is going away: one bounded observation per batch, best effort.
            while !buffer.is_empty() {
                let batch = buffer.take();
                let (doc, truncated) = batch.document(id, channel.as_str(), dropped);
                if self
                    .publish_observation(&s, channel, &doc, truncated, None)
                    .is_err()
                {
                    break;
                }
            }
        }
    }

    /// Whether the session's owner may still act: a person or the host always; another
    /// principal only while its host binding is active (a revoked agent's queued actions are
    /// never performed).
    fn owner_active(&self, s: &StoredBrowserSession) -> bool {
        if matches!(
            kind_of(&s.owner_kind),
            PrincipalKind::Human | PrincipalKind::Host
        ) {
            return true;
        }
        let owner = s.owner.clone();
        matches!(
            read_snapshot(&*self.store, &ReadOptions::default(), move |r| {
                r.authority().principal(&owner)
            }),
            Ok(Some(b)) if b.active
        )
    }

    fn run_close(&self, id: &Id, _op: &OperationRecord) -> EffectOutcome {
        let slot = self.slot(id);
        slot.closing.store(true, Ordering::SeqCst);
        let mut guard = match slot.live.try_lock() {
            Ok(g) => g,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                // An action holds the connection and may wait out a call bound: the browser's
                // tree is stopped first, through the process owner and never the connection,
                // which ends that call; the connection is then free to take.
                if let Ok(Some(s)) = self.read_session(id)
                    && let Some(pid) = s.process_id.clone()
                {
                    let _ =
                        self.processes
                            .cancel(&by_id(pid), Duration::ZERO, self.config.close_bound);
                }
                slot.live.lock().unwrap_or_else(|p| p.into_inner())
            }
        };
        let s = match self.read_session(id) {
            Ok(Some(s)) => s,
            Ok(None) => {
                return failed(
                    DomainErrorCode::NotFound,
                    "browser_session",
                    "unknown session",
                );
            }
            Err(e) => {
                slot.closing.store(false, Ordering::SeqCst);
                return EffectOutcome::NotApplied(e.to_string());
            }
        };
        // The slot stays held until the session settled: a Create that has not run yet then
        // finds it closed and launches nothing.
        match guard.take() {
            Some(l) => {
                let process = l.process.clone();
                self.teardown(id, &process, Some(l), "closed", BrowserPhase::Closed);
            }
            // Settled meanwhile (its stop was verified): nothing of it remains to stop.
            None if s.phase.settled() => {
                let _ = self.platform.remove_profile(id);
                self.forget(id);
            }
            // No live connection: a session still starting (its Create has not run, or ended
            // without settling) or one whose earlier stop was uncertain. Its tree, if any, is
            // stopped first.
            None => {
                let why = if s.process_id.is_some() {
                    "closed"
                } else {
                    "closed before its browser was launched"
                };
                if let Err(e) = self.finish_close(id, &s, why) {
                    slot.closing.store(false, Ordering::SeqCst);
                    return EffectOutcome::NotApplied(e.to_string());
                }
            }
        }
        drop(guard);
        match self.read_session(id) {
            Ok(Some(s)) => completed(OperationResult::BrowserSessions {
                value: session_page(&s),
            }),
            Ok(None) => failed(
                DomainErrorCode::NotFound,
                "browser_session",
                "unknown session",
            ),
            Err(e) => EffectOutcome::Unknown(e.to_string()),
        }
    }

    /// A crashed or exited browser: settle the session `Failed` and release what it owned. A
    /// session whose Close is under way (its stop may be what ended the connection) settles
    /// `Closed`, whichever path observes the loss first.
    fn crashed(&self, id: &Id, live: Live, why: &str) {
        let process = live.process.clone();
        let closing = self
            .existing_slot(id)
            .is_some_and(|s| s.closing.load(Ordering::SeqCst));
        let (why, then) = if closing {
            ("closed", BrowserPhase::Closed)
        } else {
            (why, BrowserPhase::Failed)
        };
        self.teardown(id, &process, Some(live), why, then);
    }

    // -----------------------------------------------------------------------------------------
    // Actions

    /// The state of the action `op` is ordered behind (its recorded predecessor), if any.
    fn predecessor_state(
        &self,
        op: &OperationRecord,
    ) -> Result<Option<Option<OperationState>>, StorageError> {
        let Some(before) = op
            .payload
            .get("after")
            .and_then(Value::as_str)
            .and_then(|o| Id::new(o).ok())
            .map(OperationId)
        else {
            return Ok(None);
        };
        read_snapshot(&*self.store, &ReadOptions::default(), move |s| {
            Ok(Some(s.operations().operation(&before)?.map(|o| o.state)))
        })
    }

    /// Whether `op` is ordered behind an action that is still queued or running. The
    /// dispatcher waits for this to clear before dispatching `op`, holding no worker capacity
    /// (see [`Self::order_bound`]); a read failure counts as settled, so the execution check
    /// decides.
    pub fn predecessor_pending(&self, op: &OperationRecord) -> bool {
        matches!(
            self.predecessor_state(op),
            Ok(Some(Some(
                OperationState::IntentRecorded
                    | OperationState::RetryPending
                    | OperationState::Preflighted
                    | OperationState::Dispatching
            )))
        )
    }

    /// How long an action waits for its predecessor before it is retried.
    pub fn order_bound(&self) -> Duration {
        self.config.order_bound
    }

    /// Runs actions in commit order: the session's previous action must have settled (the
    /// dispatcher waited for it without holding capacity). A predecessor whose outcome is
    /// unknown stops this one (nothing is sent after an uncertain navigation or submission);
    /// one still pending (the wait ran out) is retried, never overtaken.
    fn after_predecessor(&self, op: &OperationRecord) -> Result<(), EffectOutcome> {
        match self
            .predecessor_state(op)
            .map_err(|e| EffectOutcome::NotApplied(e.to_string()))?
        {
            None
            | Some(
                None
                | Some(
                    OperationState::ObservedComplete
                    | OperationState::Failed
                    | OperationState::CancelledBeforeDispatch,
                ),
            ) => Ok(()),
            Some(Some(OperationState::OutcomeUnknown | OperationState::NeedsInput)) => Err(failed(
                DomainErrorCode::Conflict,
                "browser_action_order",
                "an earlier action of this session has an unknown outcome",
            )),
            Some(Some(_)) => Err(EffectOutcome::NotApplied(
                "an earlier action of this session is still pending".into(),
            )),
        }
    }

    fn run_action(&self, id: &Id, op: &OperationRecord) -> EffectOutcome {
        if let Err(o) = self.after_predecessor(op) {
            return o;
        }
        match self.read_session(id) {
            Ok(Some(s)) if !self.owner_active(&s) => {
                return failed(
                    DomainErrorCode::PermissionDenied,
                    SESSION_OWNER,
                    "the session's owner no longer has authority",
                );
            }
            // Not just the predecessor: any unreconciled uncertain action of the session holds
            // every later one (a failed predecessor in between does not release it).
            Ok(Some(s)) => match self.unresolved_uncertain(&s, &op.id) {
                Ok(Some(u)) => {
                    return failed(
                        DomainErrorCode::Conflict,
                        "browser_action_order",
                        &format!(
                            "action {} of this session has an unknown outcome",
                            u.0.as_str()
                        ),
                    );
                }
                Ok(None) => {}
                Err(e) => return EffectOutcome::NotApplied(e.to_string()),
            },
            Ok(None) => {}
            Err(e) => return EffectOutcome::NotApplied(e.to_string()),
        }
        let Some(slot) = self.existing_slot(id) else {
            return failed(
                DomainErrorCode::Conflict,
                SESSION_STATE,
                "the session has no running browser",
            );
        };
        if slot.closing.load(Ordering::SeqCst) {
            return failed(
                DomainErrorCode::Conflict,
                SESSION_STATE,
                "the session is closing",
            );
        }
        let source = match self.binding_source(
            op.payload.get("sourceBinding").filter(|v| !v.is_null()),
            &|| slot.closing.load(Ordering::SeqCst),
        ) {
            Ok(source) => source,
            Err(_) => {
                return failed(
                    DomainErrorCode::SourceChanged,
                    "git_source",
                    "the tested worktree could not be observed before dispatch",
                );
            }
        };
        let mut observed_operation = op.clone();
        observed_operation.payload["testedRepository"] = source.unwrap_or(Value::Null);
        let op = &observed_operation;
        let mut guard = slot.live.lock().unwrap_or_else(|p| p.into_inner());
        let Some(live) = guard.as_mut() else {
            return failed(
                DomainErrorCode::Conflict,
                SESSION_STATE,
                "the session has no running browser",
            );
        };
        let bounds = self.config.bounds();
        // Notifications that arrived since the last action move the live revisions first.
        if let Err(e) = live.pump(Duration::from_millis(20), &bounds) {
            let l = guard.take().expect("live");
            drop(guard);
            self.crashed(id, l, &format!("the browser connection was lost: {e}"));
            return failed(
                DomainErrorCode::Conflict,
                SESSION_STATE,
                "the browser is no longer running",
            );
        }
        live.prune_closed();
        let closing = || slot.closing.load(Ordering::SeqCst);
        let outcome = match op.capability.as_str() {
            NAVIGATE => self.navigate(live, op, &bounds, &closing),
            INPUT => self.input(live, op, &bounds),
            _ => self.upload(live, op, &bounds),
        };
        self.publish_view(live);
        let changed = std::mem::take(&mut live.targets_changed);
        let targets = live.open_targets();
        let version = live.targets_version;
        let lost = live.cdp.closed();
        if lost {
            let l = guard.take().expect("live");
            drop(guard);
            self.crashed(id, l, "the browser connection was lost during an action");
        } else {
            drop(guard);
            if changed {
                self.record_targets(id, version, targets);
            }
        }
        match outcome {
            Ok(v) => EffectOutcome::Completed(v),
            Err(ActionError::Stale { kind, detail: d }) => {
                failed(DomainErrorCode::SourceChanged, kind, &d)
            }
            Err(ActionError::Refused { kind, detail: d }) => {
                failed(DomainErrorCode::InvalidArgument, kind, &d)
            }
            Err(ActionError::Before(e)) => {
                failed(DomainErrorCode::ExternalFailure, "browser", &e.to_string())
            }
            Err(ActionError::Navigation(e)) => failed(
                DomainErrorCode::ExternalFailure,
                "browser_navigation",
                &format!("navigation failed: {e}"),
            ),
            Err(ActionError::Closing) => {
                self.mark_uncertain(id, &op.id);
                EffectOutcome::Unknown("the session closed while the action was in flight".into())
            }
            Err(ActionError::After(e)) => {
                self.mark_uncertain(id, &op.id);
                EffectOutcome::Unknown(format!(
                    "the action was sent but its outcome is unknown: {e}"
                ))
            }
        }
    }

    /// The session's recorded uncertain action other than `own`, while its outcome is still
    /// not established (unknown, awaiting input, or being recorded).
    fn unresolved_uncertain(
        &self,
        s: &StoredBrowserSession,
        own: &OperationId,
    ) -> Result<Option<OperationId>, StorageError> {
        let Some(u) = s.uncertain_operation_id.clone().filter(|u| u != own) else {
            return Ok(None);
        };
        let q = u.clone();
        let state = read_snapshot(&*self.store, &ReadOptions::default(), move |r| {
            Ok(r.operations().operation(&q)?.map(|o| o.state))
        })?;
        Ok(unresolved(state).then_some(u))
    }

    fn mark_uncertain(&self, id: &Id, op: &OperationId) {
        let op = op.clone();
        let _ = self.update(id, move |s| {
            s.uncertain_operation_id = Some(op.clone());
            Some((
                "BrowserActionUncertain",
                json!({"operationId": op.0.as_str()}),
            ))
        });
    }

    /// Records the session's registered targets; if the write fails, the live session keeps
    /// its change flag so the next pass records them again.
    #[doc(hidden)]
    pub fn record_targets(&self, id: &Id, version: u64, targets: Vec<String>) {
        let Some(slot) = self.existing_slot(id) else {
            return;
        };
        let written = {
            let mut recorded = slot.recorded.lock().unwrap_or_else(|p| p.into_inner());
            // A list captured before the one already recorded is stale: never written over it.
            if version <= *recorded {
                return;
            }
            let written = self.update(id, move |s| {
                if s.targets == targets {
                    return None;
                }
                s.targets = targets.clone();
                Some(("BrowserTargetsChanged", json!({"targets": targets})))
            });
            if written.is_ok() {
                *recorded = version;
            }
            written
        };
        if written.is_err()
            && let Some(slot) = self.existing_slot(id)
            && let Some(live) = slot.live.lock().unwrap_or_else(|p| p.into_inner()).as_mut()
        {
            live.targets_changed = true;
        }
    }

    fn session_evidence(
        &self,
        s: &StoredBrowserSession,
        title: String,
        media_type: &str,
        mut source: Value,
    ) -> Evidence {
        if let Some(repository) = &s.tested_repository {
            source["testedRepository"] = repository.clone();
        }
        Evidence {
            scope: match &s.project_id {
                Some(p) => ArtifactScope::Project(p.clone()),
                None => ArtifactScope::Workspace,
            },
            producer: s.owner.clone(),
            principal_kind: s.owner_kind.clone(),
            agent_session: s.agent_session_id.clone(),
            root: s.root_id.clone(),
            assignment: s.assignment_id.clone(),
            title,
            media_type: media_type.into(),
            tags: vec!["browser".into()],
            source,
        }
    }

    /// Publishes one observation document and its index row together.
    fn publish_observation(
        &self,
        s: &StoredBrowserSession,
        channel: BrowserChannel,
        bytes: &[u8],
        truncated: bool,
        operation: Option<&OperationId>,
    ) -> Result<(ArtifactRef, i64), EvidenceFailure> {
        let evidence = self.session_evidence(
            s,
            format!("Browser {} observation", channel.as_str().to_lowercase()),
            "application/json",
            json!({
                "kind": format!("Browser{}", channel.as_str()),
                "browserSessionId": s.id.as_str(),
                "operationId": operation.map(|o| o.0.as_str()),
                "truncated": truncated,
            }),
        );
        let id = s.id.clone();
        let operation = operation.cloned();
        let tested_repository = s.tested_repository.clone();
        self.artifacts
            .publish_evidence(&*self.store, evidence, bytes, move |tx, r| {
                let Some(current) = tx.browser().browser_session(&id)? else {
                    return Err(StorageError::NotFound {
                        kind: "browser_session",
                        id: id.as_str().to_owned(),
                    });
                };
                let sequence = tx.browser().next_observation_sequence(&id)?;
                let recorded_at_ms = tx.now_ms();
                tx.browser()
                    .insert_browser_observation(&StoredBrowserObservation {
                        session_id: id.clone(),
                        sequence,
                        channel,
                        artifact_id: r.artifact_id.clone(),
                        version_id: r.version_id.clone(),
                        truncated,
                        recorded_at_ms,
                        tested_repository: tested_repository.clone(),
                    })?;
                let host = tx.store().host_actor.clone();
                let aggregate = match &operation {
                    Some(o) => (ACTION_AGGREGATE, o.0.as_str().to_owned(), 2),
                    None => (SESSION_AGGREGATE, id.as_str().to_owned(), current.revision),
                };
                record_event(
                    tx,
                    &host,
                    &current,
                    operation.as_ref(),
                    (aggregate.0, &aggregate.1, aggregate.2),
                    "BrowserObservationPublished",
                    json!({
                        "browserSessionId": id.as_str(), "sequence": sequence,
                        "channel": channel.as_str(), "artifactId": r.artifact_id.as_str(),
                        "versionId": r.version_id.as_str(), "truncated": truncated,
                    }),
                )?;
                Ok(sequence)
            })
    }

    fn navigate(
        &self,
        live: &mut Live,
        op: &OperationRecord,
        bounds: &Bounds,
        closing: &dyn Fn() -> bool,
    ) -> Result<Value, ActionError> {
        let input: ops::NavigateInput = serde_json::from_value(op.payload["input"].clone())
            .map_err(|e| ActionError::Refused {
                kind: "operation",
                detail: e.to_string(),
            })?;
        let url = match &input.location {
            BrowserLocation::Remote { url } => url.0.clone(),
            BrowserLocation::Blank {} => "about:blank".into(),
            // An artifact page is served from the session's artifact origin (never a `file:`
            // URL, which could reach every other local file), from its staged copy.
            BrowserLocation::Artifact { artifact } => {
                let path = op.payload["staged"][0].as_str().unwrap_or_default();
                let body = self
                    .platform
                    .read_staged(
                        &live.profile,
                        std::path::Path::new(path),
                        live::ARTIFACT_PAGE_LIMIT,
                    )
                    .map_err(|e| ActionError::Refused {
                        kind: "browser_artifact",
                        detail: format!("the artifact page could not be served: {e}"),
                    })?;
                let name = std::path::Path::new(path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "page".into());
                live.serve_artifact(&name, body, &artifact.media_type)
            }
        };
        let i = match &input.target_id {
            Some(t) => live
                .targets
                .iter()
                .position(|x| x.id == t.as_str() && !x.closed && !x.crashed)
                .ok_or_else(|| ActionError::Stale {
                    kind: "browser_target",
                    detail: format!("target {} is not open", t.as_str()),
                })?,
            // Without a target, navigation opens and registers a new one.
            None => live.open_target(bounds)?,
        };
        let opened = input.target_id.is_none();
        live.navigate(i, &url, bounds, closing).map_err(|e| {
            // A target the action opened exists whatever the navigation then did.
            if opened && !matches!(e, ActionError::Navigation(_)) {
                post_dispatch(e)
            } else {
                e
            }
        })?;
        // Frames the new document attached are configured before its snapshot is named.
        live.settle_pending(bounds).map_err(ActionError::After)?;
        let main = live.targets[i].main_frame().to_owned();
        let target = live.targets[i].wire(&live.session, &main);
        let snapshot = self
            .snapshot_of(
                live,
                &target,
                bounds,
                Some(&op.id),
                op.payload
                    .get("testedRepository")
                    .filter(|v| !v.is_null())
                    .cloned(),
            )
            .map_err(post_dispatch)?;
        Ok(json!({"result": OperationResult::BrowserNavigate { value: snapshot }}))
    }

    fn input(
        &self,
        live: &mut Live,
        op: &OperationRecord,
        bounds: &Bounds,
    ) -> Result<Value, ActionError> {
        let input: ops::InputInput =
            serde_json::from_value(op.payload["input"].clone()).map_err(|e| {
                ActionError::Refused {
                    kind: "operation",
                    detail: e.to_string(),
                }
            })?;
        let before_target = match &input.action {
            BrowserInput::Click { node, .. } | BrowserInput::Text { node, .. } => {
                live.resolve_node(node)?.0
            }
            BrowserInput::Key { target, .. } | BrowserInput::Scroll { target, .. } => {
                live.resolve(target)?.0
            }
        };
        let before = live.before(before_target);
        let i = live.input(&input.action, bounds)?;
        let describe = match &input.action {
            BrowserInput::Click { node, button } => {
                json!({"kind": "Click", "node": node.node_id.as_str(), "button": format!("{button:?}")})
            }
            // Typed text is not repeated into evidence (it may be a credential).
            BrowserInput::Text { node, text } => {
                json!({"kind": "Text", "node": node.node_id.as_str(), "characters": text.0.chars().count()})
            }
            BrowserInput::Key { key, modifiers, .. } => {
                json!({"kind": "Key", "key": key.0, "modifiers": modifiers.0.iter().map(|m| format!("{m:?}")).collect::<Vec<_>>()})
            }
            BrowserInput::Scroll { x, y, .. } => json!({"kind": "Scroll", "x": x, "y": y}),
        };
        self.action_observation(live, i, before, describe, op, bounds)
    }

    fn upload(
        &self,
        live: &mut Live,
        op: &OperationRecord,
        bounds: &Bounds,
    ) -> Result<Value, ActionError> {
        let input: ops::UploadInput =
            serde_json::from_value(op.payload["input"].clone()).map_err(|e| {
                ActionError::Refused {
                    kind: "operation",
                    detail: e.to_string(),
                }
            })?;
        let paths: Vec<String> = op.payload["staged"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        // Only the batch the command staged under this session's root is ever handed over.
        let root = live.profile.root.clone();
        if paths.is_empty()
            || paths
                .iter()
                .any(|p| !std::path::Path::new(p).starts_with(&root))
        {
            return Err(ActionError::Refused {
                kind: "browser_upload",
                detail: "the upload has no staged files of this session".into(),
            });
        }
        let (i0, _) = live.resolve_node(&input.node)?;
        let before = live.before(i0);
        let i = live.set_files(&input.node, &paths, bounds)?;
        let describe = json!({
            "kind": "Upload",
            "node": input.node.node_id.as_str(),
            "files": input.files.0.iter().map(|f| json!({"artifactId": f.artifact_id.as_str(), "versionId": f.version_id.as_str(), "sha256": f.sha256.0})).collect::<Vec<_>>(),
        });
        self.action_observation(live, i, before, describe, op, bounds)
    }

    fn action_observation(
        &self,
        live: &mut Live,
        i: usize,
        before: (String, i64, usize, u64),
        describe: Value,
        op: &OperationRecord,
        bounds: &Bounds,
    ) -> Result<Value, ActionError> {
        let doc = live
            .observe_after(i, before, describe, bounds)
            .map_err(ActionError::After)?;
        let mut s = self
            .read_session(&live.session)
            .ok()
            .flatten()
            .ok_or_else(|| {
                ActionError::After(cdp::CdpError::Protocol("session vanished".into()))
            })?;
        s.tested_repository = op
            .payload
            .get("testedRepository")
            .filter(|v| !v.is_null())
            .cloned();
        let (evidence, sequence) = self
            .publish_observation(&s, BrowserChannel::Action, &doc, false, Some(&op.id))
            .map_err(|e| ActionError::After(cdp::CdpError::Protocol(format!("{e:?}"))))?;
        let value = BrowserObservation {
            session_id: live.session.clone(),
            sequence: counter(sequence),
            evidence,
            truncated: false,
            tested_repository: s
                .tested_repository
                .clone()
                .map(serde_json::from_value)
                .transpose()
                .expect("validated browser source"),
        };
        let result = if op.capability == UPLOAD {
            OperationResult::BrowserUpload { value }
        } else {
            OperationResult::BrowserInput { value }
        };
        Ok(json!({ "result": result }))
    }

    fn snapshot_of(
        &self,
        live: &mut Live,
        target: &BrowserTarget,
        bounds: &Bounds,
        operation: Option<&OperationId>,
        tested_repository: Option<Value>,
    ) -> Result<BrowserSnapshot, ActionError> {
        let (wire, url, title, bytes) = live.snapshot(target, bounds)?;
        let mut s = self
            .read_session(&live.session)
            .ok()
            .flatten()
            .ok_or_else(|| {
                ActionError::Before(cdp::CdpError::Protocol("session vanished".into()))
            })?;
        s.tested_repository = tested_repository;
        let evidence = self.session_evidence(
            &s,
            format!("Accessibility snapshot of {}", wire.target_id.as_str()),
            "application/json",
            json!({
                "kind": "BrowserAccessibility",
                "browserSessionId": s.id.as_str(),
                "targetId": wire.target_id.as_str(),
                "frameId": wire.frame_id.as_str(),
                "documentRevision": wire.document_revision.as_str(),
                "operationId": operation.map(|o| o.0.as_str()),
            }),
        );
        let (accessibility, ()) = self
            .artifacts
            .publish_evidence(&*self.store, evidence, &bytes, |_, _| Ok(()))
            .map_err(|e| ActionError::Before(cdp::CdpError::Protocol(format!("{e:?}"))))?;
        let url = BoundedText(if url.is_empty() {
            "about:blank".into()
        } else {
            url.chars().take(4096).collect()
        });
        Ok(BrowserSnapshot {
            target: wire,
            url,
            title: BoundedText(if title.is_empty() {
                "(untitled)".into()
            } else {
                title.chars().take(256).collect()
            }),
            accessibility,
            screenshot: None,
        })
    }

    // -----------------------------------------------------------------------------------------
    // Supervision

    /// One supervision pass: reads notifications of idle sessions, records target changes, and
    /// settles sessions whose browser exited or whose connection dropped (a crash). Busy
    /// sessions are skipped; the next pass sees them.
    pub fn tick(&self) {
        let slots: Vec<(String, Arc<Slot>)> = self
            .slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let bounds = self.config.bounds();
        for (k, slot) in slots {
            let Ok(id) = Id::new(k) else { continue };
            let Ok(mut guard) = slot.live.try_lock() else {
                continue;
            };
            let Some(live) = guard.as_mut() else { continue };
            // The browser's main process exiting ends the session even while helper processes
            // of its tree linger (the teardown stops them).
            let exited = matches!(self.processes.poll(&live.process), Ok(Some(_)))
                || self.processes.root_exited(&live.process);
            let pumped = live.pump(Duration::from_millis(20), &bounds);
            if exited || pumped.is_err() || live.cdp.closed() {
                let l = guard.take().expect("live");
                drop(guard);
                let why = if exited {
                    "the browser exited"
                } else {
                    "the browser connection was lost"
                };
                self.crashed(&id, l, why);
                continue;
            }
            live.prune_closed();
            self.publish_view(live);
            let changed = std::mem::take(&mut live.targets_changed);
            let targets = live.open_targets();
            let version = live.targets_version;
            drop(guard);
            if changed {
                self.record_targets(&id, version, targets);
            }
        }
    }

    /// The owned tree a session launched, if any: the one recorded on the session, or else the
    /// unsettled launch record of its create operation (a crash or a failed write between the
    /// launch and recording its tree leaves no process ID on the session, so a missing one is
    /// never evidence that nothing was launched).
    fn launched_process(
        &self,
        s: &StoredBrowserSession,
    ) -> Result<Option<crate::ports::storage::StoredProcess>, StorageError> {
        let (recorded, create) = (s.process_id.clone(), s.create_operation_id.clone());
        read_snapshot(&*self.store, &ReadOptions::default(), move |r| {
            if let Some(p) = &recorded
                && let Some(found) = r.resources().process(p)?
            {
                return Ok(Some(found));
            }
            launch_record(r.resources(), &create)
        })
    }

    /// Settles the sessions of earlier boots from what process recovery verified (run after
    /// it): a session whose tree is gone is `Closed` (it was stopping, or its Close was requested
    /// while it started) or `Failed` (interrupted), with its profile removed; one whose tree
    /// could not be verified stays `Uncertain` with its profile kept. Then removes every profile root no unsettled session owns.
    pub fn recover(&self) -> Result<BrowserRecovery, StorageError> {
        let boot = self.store.info().boot_id;
        let mut report = BrowserRecovery::default();
        let mut after: Option<Id> = None;
        let mut keep: HashSet<String> = HashSet::new();
        loop {
            let a = after.clone();
            let page = read_snapshot(&*self.store, &ReadOptions::default(), move |s| {
                s.browser()
                    .browser_sessions(&BrowserFilter::Unsettled, a.as_ref(), 100)
            })?;
            let full = page.len() == 100;
            after = page.last().map(|s| s.id.clone());
            for s in page {
                if Some(&s.boot_id) == boot.as_ref() && self.existing_slot(&s.id).is_some() {
                    keep.insert(s.id.as_str().to_owned());
                    continue;
                }
                let process = self.launched_process(&s)?;
                let gone = process.as_ref().is_none_or(|p| {
                    matches!(
                        p.phase,
                        crate::ports::storage::ProcessPhase::Reaped
                            | crate::ports::storage::ProcessPhase::Failed
                    )
                });
                if gone {
                    let removed = self.platform.remove_profile(&s.id);
                    let mut why = "interrupted by an application restart".to_owned();
                    if let Err(e) = removed {
                        why.push_str(&format!("; the profile was not removed: {e}"));
                    }
                    let phase = if s.phase == BrowserPhase::Stopping {
                        BrowserPhase::Closed
                    } else {
                        BrowserPhase::Failed
                    };
                    self.fail_session(&s.id, &why, phase);
                    report.settled += 1;
                } else {
                    keep.insert(s.id.as_str().to_owned());
                    // The tree found through the create operation is recorded on the session
                    // with its uncertainty, in one write: a later Close stops that tree before
                    // anything settles or the profile is removed.
                    let found = process.as_ref().map(|p| p.id.clone());
                    let why =
                        detail("the browser of an earlier boot could not be verified stopped");
                    self.update(&s.id, move |s| {
                        let adopt = s.process_id.is_none() && found.is_some();
                        if !adopt && s.phase == BrowserPhase::Uncertain {
                            return None;
                        }
                        if adopt {
                            s.process_id = found.clone();
                        }
                        if s.phase != BrowserPhase::Uncertain {
                            s.phase = BrowserPhase::Uncertain;
                            s.detail = Some(why.clone());
                        }
                        Some((
                            "BrowserSessionUncertain",
                            json!({
                                "detail": why,
                                "processId": s.process_id.as_ref().map(Id::as_str),
                            }),
                        ))
                    })?;
                    report.uncertain += 1;
                }
            }
            if !full {
                break;
            }
        }
        if let Ok(roots) = self.platform.profiles() {
            for r in roots {
                if !keep.contains(r.as_str()) && self.platform.remove_profile(&r).is_ok() {
                    report.profiles_removed += 1;
                }
            }
        }
        Ok(report)
    }

    /// Closes every running session (application shutdown), each through the process owner
    /// within the remaining bound, and settles its record.
    pub fn shutdown(&self) -> usize {
        let slots: Vec<(String, Arc<Slot>)> = self
            .slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain()
            .collect();
        let mut n = 0;
        for (k, slot) in slots {
            slot.closing.store(true, Ordering::SeqCst);
            let live = slot.live.lock().unwrap_or_else(|p| p.into_inner()).take();
            let Ok(id) = Id::new(k) else { continue };
            if let Some(l) = live {
                let process = l.process.clone();
                self.teardown(
                    &id,
                    &process,
                    Some(l),
                    "closed at application shutdown",
                    BrowserPhase::Closed,
                );
                n += 1;
            }
        }
        n
    }

    /// Per-target qualification of the browser assets and the session summary
    /// (`workspace.diagnostics` health source).
    pub fn health(&self, _now_ms: i64) -> Vec<HealthCheck> {
        use crate::protocol::records::HealthState;
        let verified = self
            .verified
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let failures = self
            .launch_failures
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let target = self.platform.target();
        let mut out = Vec::new();
        for a in self.assets() {
            let component = format!("browser.asset.{}", a.id);
            let c = match (&a.qualification, verified.get(&a.id), failures.get(&a.id)) {
                (Qualification::Present, Some((product, protocol)), _) => HealthCheck::new(
                    &component,
                    HealthState::Healthy,
                    format!(
                        "{} qualified on {target}: {product}, DevTools protocol {protocol}",
                        a.product
                    ),
                ),
                (Qualification::Present, None, Some(why)) => HealthCheck::new(
                    &component,
                    HealthState::Failed,
                    format!(
                        "{} on {target} failed its launch handshake: {why}",
                        a.product
                    ),
                ),
                (Qualification::Present, None, None) => HealthCheck::new(
                    &component,
                    HealthState::Degraded,
                    format!(
                        "{} found on {target}; not yet verified by a launch in this session",
                        a.product
                    ),
                ),
                (Qualification::Missing(why), _, _) => HealthCheck::new(
                    &component,
                    HealthState::Unavailable,
                    format!("{} is not available on {target}: {why}", a.product),
                ),
                (Qualification::Unsupported(why), _, _) => HealthCheck::new(
                    &component,
                    HealthState::Unavailable,
                    format!("{} is not supported on {target}: {why}", a.product),
                ),
                (Qualification::Failed(why), _, _) => HealthCheck::new(
                    &component,
                    HealthState::Failed,
                    format!("{} on {target} cannot be used: {why}", a.product),
                ),
            };
            out.push(c);
        }
        if !self
            .assets()
            .iter()
            .any(|a| a.qualification == Qualification::Present)
        {
            out.push(HealthCheck::new(
                "browser.automation",
                HealthState::Unavailable,
                format!("No compatible browser was found on {target}; browser testing is unavailable until Chrome, Chromium or Edge is installed or a browser path is configured."),
            ));
        }
        out
    }

    // -----------------------------------------------------------------------------------------
    // Unknown outcomes

    /// A person's statement of what an action whose outcome is unknown actually did (for
    /// example after looking at the page): `happened` records it observed complete, otherwise
    /// failed. Recorded as theirs (`BrowserActionResolved`), never inferred; the session accepts
    /// new navigation, input and upload again. Only a person may resolve, and only an operation
    /// that is still `OutcomeUnknown`. Its held delivery is then settled by `queue.reconcile`.
    pub fn resolve_unknown(
        &self,
        ctx: &InvocationContext,
        operation: &OperationId,
        happened: bool,
    ) -> Result<OperationState, ResolveRefusal> {
        if ctx.principal_kind() != PrincipalKind::Human {
            return Err(ResolveRefusal::HumanOnly);
        }
        let (op_id, c) = (operation.clone(), ctx.clone());
        run_write(&*self.store, WriteOptions::default(), move |tx| {
            let Some(op) = tx.operations().operation(&op_id)? else {
                return Ok(Err(ResolveRefusal::NotFound));
            };
            let session = op
                .payload
                .get("sessionId")
                .and_then(Value::as_str)
                .and_then(|s| Id::new(s).ok());
            let Some(session) = session else {
                return Ok(Err(ResolveRefusal::NotFound));
            };
            let Some(s) = tx.browser().browser_session(&session)? else {
                return Ok(Err(ResolveRefusal::NotFound));
            };
            if !op.capability.starts_with("browser.") || !visible(&c, &s) {
                return Ok(Err(ResolveRefusal::NotFound));
            }
            if op.state != OperationState::OutcomeUnknown {
                return Ok(Err(ResolveRefusal::NotUnknown(op.state)));
            }
            let to = if happened {
                OperationState::ObservedComplete
            } else {
                OperationState::Failed
            };
            let statement = json!({
                "resolvedBy": c.principal().0.as_str(),
                "statement": if happened { "the action happened" } else { "the action did not happen" },
            });
            tx.operations()
                .transition(&op.id, op.revision, None, to, Some(statement.clone()))?;
            if !s.phase.settled() && s.uncertain_operation_id.as_ref() == Some(&op.id) {
                let mut next = s.clone();
                next.uncertain_operation_id = None;
                next.revision = s.revision + 1;
                tx.browser().update_browser_session(&next, s.revision)?;
            }
            record_event(
                tx,
                c.principal(),
                &s,
                Some(&op.id),
                (ACTION_AGGREGATE, op.id.0.as_str(), 3),
                "BrowserActionResolved",
                statement,
            )?;
            Ok(Ok(to))
        })
        .map_err(ResolveRefusal::Storage)?
    }

    // -----------------------------------------------------------------------------------------
    // Completion

    /// The typed completion of a browser operation once it settled (`None` while it is still
    /// pending), for the invocation that may see its session.
    pub fn completion(
        &self,
        ctx: &InvocationContext,
        operation: &OperationId,
    ) -> Result<Option<OperationCompletion>, DomainFault> {
        let op_id = operation.clone();
        let read = read_snapshot(&*self.store, &ReadOptions::default(), move |s| {
            let Some(op) = s.operations().operation(&op_id)? else {
                return Ok(None);
            };
            let session = op
                .payload
                .get("sessionId")
                .and_then(Value::as_str)
                .and_then(|i| Id::new(i).ok());
            let session = match session {
                Some(id) => s.browser().browser_session(&id)?,
                None => None,
            };
            Ok(Some((op, session)))
        })
        .map_err(|e| query_fault(e.into(), "operation", self.correlation()))?;
        let not_found = || {
            Refusal::new(
                DomainErrorCode::NotFound,
                "operation",
                Some(operation.0.clone()),
            )
            .fault(self.correlation())
        };
        let Some((op, Some(session))) = read else {
            return Err(not_found());
        };
        if !op.capability.starts_with("browser.") || !visible(ctx, &session) {
            return Err(not_found());
        }
        let revision = rev(op.revision);
        let effect = op.latest_attempt().and_then(|a| {
            a.observations
                .iter()
                .rev()
                .find_map(|o| o.get("effect").cloned())
        });
        Ok(match op.state {
            OperationState::ObservedComplete => {
                let result = effect
                    .as_ref()
                    .and_then(|e| e.pointer("/evidence/result"))
                    .and_then(|r| serde_json::from_value::<OperationResult>(r.clone()).ok());
                match result {
                    Some(result) => Some(OperationCompletion::Succeeded {
                        protocol: Protocol::Draft1,
                        operation_id: op.id,
                        revision,
                        result,
                    }),
                    // Completed by reconciliation: no typed result was observed.
                    None => Some(OperationCompletion::Failed {
                        protocol: Protocol::Draft1,
                        operation_id: op.id,
                        revision,
                        fault: Refusal::new(DomainErrorCode::OutcomeUnknown, "operation", None)
                            .retry(RetryAdvice::ReconcileOperation)
                            .fault(self.correlation()),
                    }),
                }
            }
            OperationState::Failed | OperationState::CancelledBeforeDispatch => {
                let cause: Value = effect
                    .as_ref()
                    .and_then(|e| e.get("cause"))
                    .and_then(Value::as_str)
                    .and_then(|c| serde_json::from_str(c).ok())
                    .unwrap_or(Value::Null);
                let code = cause
                    .get("code")
                    .and_then(Value::as_str)
                    .and_then(|c| serde_json::from_value::<DomainErrorCode>(json!(c)).ok())
                    .unwrap_or(if op.state == OperationState::Failed {
                        DomainErrorCode::ExternalFailure
                    } else {
                        DomainErrorCode::Cancelled
                    });
                let kind = cause
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("browser")
                    .chars()
                    .take(256)
                    .collect::<String>();
                let retry = match code {
                    DomainErrorCode::SourceChanged => RetryAdvice::RefreshAndNewRequest,
                    _ => RetryAdvice::Never,
                };
                let mut fault = crate::services::fault(code, retry, &self.correlation());
                fault.target_kind = Some(BoundedText(kind));
                fault.target_id = Some(session.id.clone());
                Some(OperationCompletion::Failed {
                    protocol: Protocol::Draft1,
                    operation_id: op.id,
                    revision,
                    fault,
                })
            }
            OperationState::OutcomeUnknown => Some(OperationCompletion::OutcomeUnknown {
                protocol: Protocol::Draft1,
                operation_id: op.id,
                revision,
                correlation_id: self.correlation(),
            }),
            _ => None,
        })
    }

    // -----------------------------------------------------------------------------------------
    // Decisions

    fn create(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        scope: &DataScope,
        asset_id: &Id,
        command: &AdmittedCommand,
    ) -> Result<Decided, Failure> {
        let project = match scope {
            DataScope::Project { project_id } => {
                require_project(ctx, project_id)?;
                if !tx.artifacts().project_exists(project_id)? {
                    return Err(Refusal::new(
                        DomainErrorCode::NotFound,
                        "project",
                        Some(project_id.0.clone()),
                    )
                    .into());
                }
                // An archived or deleted project starts no work, a browser session included.
                if tx
                    .runs()
                    .project_lifecycle(project_id)?
                    .is_some_and(|l| crate::services::runs::closed_to_work(&l))
                {
                    return Err(Refusal::new(
                        DomainErrorCode::Conflict,
                        "archived_project",
                        Some(project_id.0.clone()),
                    )
                    .into());
                }
                Some(project_id.clone())
            }
            DataScope::Workspace => {
                let workspace = ctx.project_id().is_none() && ctx.root_id().is_none();
                let authorized = crate::services::artifacts::authorized_scope(tx.authority(), ctx)?;
                if !workspace
                    || authorized != crate::services::artifacts::AuthorizedScope::Workspace
                {
                    return Err(Refusal::new(
                        DomainErrorCode::PermissionDenied,
                        "workspace_scope",
                        None,
                    )
                    .into());
                }
                None
            }
        };
        // A missing or unqualified browser is a visible limitation, never a started session.
        match self.asset(asset_id.as_str()) {
            None => {
                return Err(Refusal::new(
                    DomainErrorCode::UnsupportedTarget,
                    ASSET,
                    Some(asset_id.clone()),
                )
                .into());
            }
            Some(a) if a.qualification != Qualification::Present => {
                return Err(Refusal::new(
                    DomainErrorCode::UnsupportedCapability,
                    ASSET,
                    Some(asset_id.clone()),
                )
                .into());
            }
            Some(_) => {}
        }
        let boot = tx
            .store()
            .boot_id
            .clone()
            .ok_or_else(|| StorageError::IncompatibleSchema("store is read-only".into()))?;
        let id = tx.new_id(IdKind::BrowserSession);
        let op = OperationId(tx.new_id(IdKind::Operation));
        tx.operations().record_intent(NewOperation {
            id: op.clone(),
            root_id: ctx.root_id().cloned(),
            assignment_id: ctx.assignment_id().cloned(),
            capability: CREATE.into(),
            schema_version: 1,
            args_digest: sha256_hex(format!("{}|{}", id.as_str(), asset_id.as_str()).as_bytes()),
            payload: json!({"sessionId": id.as_str(), "assetId": asset_id.as_str()}),
        })?;
        let now = tx.now_ms();
        let session = StoredBrowserSession {
            id: id.clone(),
            revision: 1,
            phase: BrowserPhase::Starting,
            boot_id: boot,
            owner: ctx.principal().clone(),
            owner_kind: owner_kind(ctx.principal_kind()).into(),
            agent_session_id: ctx.session_id().cloned(),
            project_id: project,
            root_id: ctx.root_id().cloned(),
            assignment_id: ctx.assignment_id().cloned(),
            asset_id: asset_id.as_str().to_owned(),
            tested_repository: staged_source(command).0,
            source_binding: staged_source(command).1,
            process_id: None,
            create_operation_id: op.clone(),
            uncertain_operation_id: None,
            last_action_operation_id: None,
            close_operation_id: None,
            targets: Vec::new(),
            observation_sequence: 0,
            detail: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        tx.browser().insert_browser_session(&session)?;
        let event = record_event(
            tx,
            ctx.principal(),
            &session,
            Some(&op),
            (SESSION_AGGREGATE, id.as_str(), 1),
            "BrowserSessionRequested",
            json!({"browserSessionId": id.as_str(), "assetId": asset_id.as_str(), "state": "Starting"}),
        )?;
        self.enqueue(tx, &session, &event, &op, true)?;
        Ok(Decided {
            decision: DomainDecision::Accepted {
                operation_id: op.clone(),
            },
            operation_id: Some(op),
        })
    }

    /// Queues the effect of `op`. Work of a root-scoped session (Create, navigation, input,
    /// upload) carries its root, so the root's run controls gate it; cleanup (`gated` false:
    /// Close) carries none, so a paused, held or stopped root never keeps a browser running.
    /// Its lineage stays on the operation (the closer's root and assignment) and the event.
    fn enqueue(
        &self,
        tx: &mut dyn WriteTransaction,
        s: &StoredBrowserSession,
        event: &EventId,
        op: &OperationId,
        gated: bool,
    ) -> Result<(), StorageError> {
        let id = tx.new_id(IdKind::QueueEntry);
        let now = tx.now_ms();
        tx.queue().enqueue(NewQueueEntry {
            id,
            consumer: EFFECT_CONSUMER.into(),
            event_id: event.clone(),
            root_id: if gated { s.root_id.clone() } else { None },
            operation_id: Some(op.clone()),
            // Unordered: a failed or held action never blocks the session's later ones. Actions
            // keep commit order through their recorded predecessor (`after`).
            partition: None,
            priority: 0,
            ready_at_ms: now,
            payload: json!({"browserSessionId": s.id.as_str()}),
        })?;
        Ok(())
    }

    fn close(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        id: &Id,
        expected: &Revision,
    ) -> Result<Decided, Failure> {
        let s = session_of(tx.browser(), ctx, id)?;
        if !may_drive(ctx, &s) {
            return Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                SESSION_OWNER,
                Some(id.clone()),
            )
            .into());
        }
        if expected.get() != s.revision {
            return Err(conflict(id, expected, s.revision));
        }
        // A cancelled, undispatched Close did not clean up the browser. Allow another
        // request to replace it; a still-pending Close retains its ordinary idempotence.
        let close_pending = match &s.close_operation_id {
            Some(op) => !matches!(
                tx.operations().operation(op)?.map(|op| op.state),
                Some(OperationState::CancelledBeforeDispatch)
            ),
            None => false,
        };
        if matches!(s.phase, BrowserPhase::Closed | BrowserPhase::Failed)
            || (matches!(s.phase, BrowserPhase::Stopping | BrowserPhase::Starting) && close_pending)
        {
            return Ok(Decided {
                decision: DomainDecision::NoChange {
                    result: OperationResult::BrowserSessions {
                        value: session_page(&s),
                    },
                },
                operation_id: None,
            });
        }
        let op = OperationId(tx.new_id(IdKind::Operation));
        let mut next = s.clone();
        next.revision += 1;
        next.close_operation_id = Some(op.clone());
        // A missing process ID is not evidence that nothing was launched (a crash or a lost
        // write between the launch and recording its tree leaves none): the create operation's
        // launch record names the tree then, and it is adopted here.
        if next.process_id.is_none() {
            next.process_id = launch_record(tx.resources(), &s.create_operation_id)?.map(|p| p.id);
        }
        if s.phase == BrowserPhase::Starting {
            // Its Create may be launching right now, and a tree it launches is recorded only
            // after this decision: the session keeps `Starting` with the request and never
            // settles here. The Create stops what it launched once it sees the request (or this
            // Close settles it once no launch holds the session); only a verified stop closes.
        } else if next.process_id.is_none() {
            // A session with no launched tree closes now.
            next.phase = BrowserPhase::Closed;
            next.detail = Some("closed before its browser was launched".into());
        } else {
            // One with a tree is stopping until the process owner verified its stop.
            next.phase = BrowserPhase::Stopping;
        }
        tx.operations().record_intent(NewOperation {
            id: op.clone(),
            root_id: ctx.root_id().cloned(),
            assignment_id: ctx.assignment_id().cloned(),
            capability: CLOSE.into(),
            schema_version: 1,
            args_digest: sha256_hex(format!("{}|{}", id.as_str(), next.revision).as_bytes()),
            payload: json!({"sessionId": id.as_str()}),
        })?;
        tx.browser().update_browser_session(&next, s.revision)?;
        let event = record_event(
            tx,
            ctx.principal(),
            &next,
            Some(&op),
            (SESSION_AGGREGATE, id.as_str(), next.revision),
            "BrowserSessionCloseRequested",
            json!({"browserSessionId": id.as_str(), "state": next.phase.as_str()}),
        )?;
        self.enqueue(tx, &next, &event, &op, false)?;
        Ok(Decided {
            decision: DomainDecision::Accepted {
                operation_id: op.clone(),
            },
            operation_id: Some(op),
        })
    }

    /// The common admission of navigation, input and upload: a visible session the invocation
    /// may drive, `Ready`, at the expected revision, with no unreconciled unknown action.
    fn drivable(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        id: &Id,
        expected: Option<&Revision>,
    ) -> Result<Result<StoredBrowserSession, Decided>, Failure> {
        let s = session_of(tx.browser(), ctx, id)?;
        if !may_drive(ctx, &s) {
            return Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                SESSION_OWNER,
                Some(id.clone()),
            )
            .into());
        }
        if let Some(e) = expected
            && e.get() != s.revision
        {
            return Err(conflict(id, e, s.revision));
        }
        if s.phase != BrowserPhase::Ready {
            return Err(
                Refusal::new(DomainErrorCode::Conflict, SESSION_STATE, Some(id.clone())).into(),
            );
        }
        if let Some(op) = &s.uncertain_operation_id
            && unresolved(tx.operations().operation(op)?.map(|o| o.state))
        {
            // Observe the uncertain navigation/submission before anything else is sent.
            let mut fault = Refusal::new(
                DomainErrorCode::OutcomeUnknown,
                "operation",
                Some(op.0.clone()),
            )
            .retry(RetryAdvice::ReconcileOperation)
            .fault(CorrelationId(tx.new_id(IdKind::Correlation)));
            fault.target_id = Some(op.0.clone());
            return Ok(Err(Decided {
                decision: DomainDecision::Refused { fault },
                operation_id: Some(op.clone()),
            }));
        }
        Ok(Ok(s))
    }

    fn action(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
    ) -> Result<Decided, Failure> {
        let (capability, session_id, expected, input_json) = match command.input() {
            OperationInput::Browser(ops::Input::Navigate(i)) => (
                NAVIGATE,
                i.session_id.clone(),
                Some(i.expected_revision.clone()),
                serde_json::to_value(i),
            ),
            OperationInput::Browser(ops::Input::Input(i)) => (
                INPUT,
                i.session_id.clone(),
                Some(i.expected_revision.clone()),
                serde_json::to_value(i),
            ),
            OperationInput::Browser(ops::Input::Upload(i)) => (
                UPLOAD,
                i.node.target.session_id.clone(),
                None,
                serde_json::to_value(i),
            ),
            _ => {
                return Err(Refusal::new(
                    DomainErrorCode::UnsupportedCapability,
                    "operation",
                    None,
                )
                .into());
            }
        };
        let input_json = input_json.map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
        let s = match self.drivable(tx, ctx, &session_id, expected.as_ref())? {
            Ok(s) => s,
            Err(d) => return Ok(d),
        };
        let view = self.view(&s.id).unwrap_or_default();
        match command.input() {
            OperationInput::Browser(ops::Input::Navigate(i)) => {
                if let Some(t) = &i.target_id
                    && view.targets.get(t.as_str()).is_none_or(|v| v.crashed)
                {
                    return Err(stale("browser_target").into());
                }
            }
            OperationInput::Browser(ops::Input::Input(i)) => match &i.action {
                BrowserInput::Click { node, .. } | BrowserInput::Text { node, .. } => {
                    other_session(&node.target, &s.id)?;
                    view.node(node)?
                }
                BrowserInput::Key { target, .. } | BrowserInput::Scroll { target, .. } => {
                    other_session(target, &s.id)?;
                    view.target(target)?
                }
            },
            OperationInput::Browser(ops::Input::Upload(i)) => view.node(&i.node)?,
            _ => {}
        }
        let staged: Vec<String> = match command.prepared::<Staged>() {
            Some(Staged::Files { session, paths, .. }) if session == &s.id => paths.clone(),
            Some(Staged::Files { .. }) => {
                return Err(
                    Refusal::new(DomainErrorCode::InvalidArgument, "browser_upload", None).into(),
                );
            }
            Some(Staged::Refused(r)) => return Err(r.clone().into()),
            None | Some(Staged::Source { .. }) => Vec::new(),
        };
        let needs_files = matches!(
            command.input(),
            OperationInput::Browser(ops::Input::Upload(_))
        ) || matches!(
            command.input(),
            OperationInput::Browser(ops::Input::Navigate(ops::NavigateInput {
                location: BrowserLocation::Artifact { .. },
                ..
            }))
        );
        if needs_files && staged.is_empty() {
            return Err(
                Refusal::new(DomainErrorCode::InvalidArgument, "browser_upload", None).into(),
            );
        }
        let op = OperationId(tx.new_id(IdKind::Operation));
        tx.operations().record_intent(NewOperation {
            id: op.clone(),
            root_id: ctx.root_id().cloned(),
            assignment_id: ctx.assignment_id().cloned(),
            capability: capability.into(),
            schema_version: 1,
            args_digest: command.fingerprint().as_str().to_owned(),
            payload: json!({
                "sessionId": s.id.as_str(), "input": input_json, "staged": staged,
                "after": s.last_action_operation_id.as_ref().map(|o| o.0.as_str()),
                "sourceBinding": staged_source(command).1,
            }),
        })?;
        tx.browser().set_last_action(&s.id, &op)?;
        let event = record_event(
            tx,
            ctx.principal(),
            &s,
            Some(&op),
            (ACTION_AGGREGATE, op.0.as_str(), 1),
            "BrowserActionRequested",
            json!({"browserSessionId": s.id.as_str(), "capability": capability, "actorId": ctx.principal().0.as_str()}),
        )?;
        self.enqueue(tx, &s, &event, &op, true)?;
        Ok(Decided {
            decision: DomainDecision::Accepted {
                operation_id: op.clone(),
            },
            operation_id: Some(op),
        })
    }

    // -----------------------------------------------------------------------------------------
    // Queries

    fn list(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &ops::SessionsQueryInput,
    ) -> Result<OperationResult, Failure> {
        let project = match &i.scope {
            DataScope::Project { project_id } => {
                require_project(ctx, project_id)?;
                Some(project_id.clone())
            }
            DataScope::Workspace => {
                if ctx.project_id().is_some() {
                    return Err(Refusal::new(
                        DomainErrorCode::PermissionDenied,
                        "workspace_scope",
                        None,
                    )
                    .into());
                }
                None
            }
        };
        let scope_key = project
            .as_ref()
            .map_or("workspace", |p| p.0.as_str())
            .to_owned();
        let binding = CursorBinding::new(ctx, "browser.sessions", &[&scope_key], 1);
        let mut after = match &i.page.cursor {
            Some(c) => {
                Some(Id::new(binding.decode(&c.0)?).map_err(|_| StorageError::CursorExpired)?)
            }
            None => None,
        };
        let limit = (i.page.limit.get() as usize).clamp(1, 500);
        let mut items = Vec::new();
        let mut more = false;
        'pages: loop {
            let page = snapshot.browser().browser_sessions(
                &BrowserFilter::Scope(project.clone()),
                after.as_ref(),
                500,
            )?;
            let full = page.len() == 500;
            for s in page {
                after = Some(s.id.clone());
                if !visible(ctx, &s) {
                    continue;
                }
                if items.len() == limit {
                    more = true;
                    break 'pages;
                }
                items.push(s);
            }
            if !full {
                break;
            }
        }
        let next_cursor = if more {
            items
                .last()
                .map(|s| BoundedText(binding.encode(s.id.as_str())))
        } else {
            None
        };
        Ok(OperationResult::BrowserSessions {
            value: Page {
                items: BoundedList(items.iter().map(session_record).collect()),
                next_cursor,
                has_more: more,
            },
        })
    }

    fn readable(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        id: &Id,
    ) -> Result<StoredBrowserSession, Failure> {
        session_of(snapshot.browser(), ctx, id)
    }

    fn with_live<T>(
        &self,
        id: &Id,
        work: impl FnOnce(&mut Live) -> Result<T, Failure>,
    ) -> Result<T, Failure> {
        let not_running =
            || Refusal::new(DomainErrorCode::Conflict, SESSION_STATE, Some(id.clone()));
        let Some(slot) = self.existing_slot(id) else {
            return Err(not_running().into());
        };
        let mut guard = slot.live.lock().unwrap_or_else(|p| p.into_inner());
        let Some(live) = guard.as_mut() else {
            return Err(not_running().into());
        };
        let bounds = self.config.bounds();
        if live.pump(Duration::from_millis(20), &bounds).is_err() {
            let l = guard.take().expect("live");
            drop(guard);
            self.crashed(id, l, "the browser connection was lost");
            return Err(not_running().into());
        }
        live.prune_closed();
        let r = work(live);
        self.publish_view(live);
        let changed = std::mem::take(&mut live.targets_changed);
        let targets = live.open_targets();
        let version = live.targets_version;
        if live.cdp.closed() {
            let l = guard.take().expect("live");
            drop(guard);
            self.crashed(id, l, "the browser connection was lost");
        } else {
            drop(guard);
            if changed {
                self.record_targets(id, version, targets);
            }
        }
        r
    }

    /// Inspects a target of the live session `s` and publishes the accessibility snapshot as
    /// evidence. Runs in [`QueryHandler::prepare`], never inside the gateway's read snapshot:
    /// it reads and writes the store itself.
    fn inspect(
        &self,
        s: &StoredBrowserSession,
        target: &BrowserTarget,
    ) -> Result<OperationResult, Failure> {
        if s.phase != BrowserPhase::Ready {
            return Err(
                Refusal::new(DomainErrorCode::Conflict, SESSION_STATE, Some(s.id.clone())).into(),
            );
        }
        let bounds = self.config.bounds();
        let value = self.with_live(&s.id, |live| {
            self.snapshot_of(live, target, &bounds, None, s.tested_repository.clone())
                .map_err(|e| action_failure(e, target))
        })?;
        Ok(OperationResult::BrowserInspect { value })
    }

    /// Captures a target of the live session `s` and publishes the image as evidence. Runs in
    /// [`QueryHandler::prepare`], like [`Self::inspect`].
    fn screenshot(
        &self,
        s: &StoredBrowserSession,
        target: &BrowserTarget,
        full_page: bool,
    ) -> Result<OperationResult, Failure> {
        if s.phase != BrowserPhase::Ready {
            return Err(
                Refusal::new(DomainErrorCode::Conflict, SESSION_STATE, Some(s.id.clone())).into(),
            );
        }
        let bounds = self.config.bounds();
        let (bytes, clipped) = self.with_live(&s.id, |live| {
            live.screenshot(target, full_page, &bounds)
                .map_err(|e| action_failure(e, target))
        })?;
        let evidence = self.session_evidence(
            s,
            format!("Screenshot of {}", target.target_id.as_str()),
            "image/png",
            json!({
                "kind": "BrowserScreenshot",
                "browserSessionId": s.id.as_str(),
                "targetId": target.target_id.as_str(),
                "frameId": target.frame_id.as_str(),
                "documentRevision": target.document_revision.as_str(),
                "fullPage": full_page,
                "clipped": clipped,
            }),
        );
        let (value, ()) = self
            .artifacts
            .publish_evidence(&*self.store, evidence, &bytes, |_, _| Ok(()))
            .map_err(evidence_failure)?;
        Ok(OperationResult::BrowserScreenshot { value })
    }

    fn notices(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        id: &Id,
        after: i64,
        page: &crate::protocol::records::PageRequest,
        channel: BrowserChannel,
    ) -> Result<Page<BrowserObservation>, Failure> {
        self.readable(snapshot, ctx, id)?;
        let op_name = match channel {
            BrowserChannel::Network => "browser.network",
            _ => "browser.console",
        };
        let binding = CursorBinding::new(ctx, op_name, &[id.as_str(), &after.to_string()], 1);
        let mut position = after;
        if let Some(c) = &page.cursor {
            position = binding
                .decode(&c.0)?
                .parse::<i64>()
                .map_err(|_| StorageError::CursorExpired)?
                .max(after);
        }
        let limit = (page.limit.get() as usize).clamp(1, MAX_PAGE as usize);
        // The page is read from the gateway's snapshot, taken after this query's preparation
        // published what was pending ([`Self::publish_pending`]): committed sequences form a
        // gap-free prefix (each is allocated and committed by the single writer in turn), so
        // that snapshot also sees every earlier one another query published meanwhile, and a
        // continuation never skips one.
        let mut items = observation_page(snapshot, id, channel, position, limit)?;
        let more = items.len() > limit;
        items.truncate(limit);
        let next_cursor = if more {
            items
                .last()
                .map(|i| BoundedText(binding.encode(&i.sequence.get().to_string())))
        } else {
            None
        };
        Ok(Page {
            items: BoundedList(items),
            next_cursor,
            has_more: more,
        })
    }

    /// Publishes the notifications of `channel` buffered since the last read as one
    /// observation. Runs in [`QueryHandler::prepare`], outside the gateway's read snapshot.
    fn publish_pending(
        &self,
        s: &StoredBrowserSession,
        channel: BrowserChannel,
    ) -> Result<(), Failure> {
        let id = &s.id;
        if s.phase == BrowserPhase::Ready
            && let Some(slot) = self.existing_slot(id)
        {
            let pending = {
                let mut guard = slot.live.lock().unwrap_or_else(|p| p.into_inner());
                match guard.as_mut() {
                    Some(live) => {
                        let _ = live.pump(Duration::from_millis(20), &self.config.bounds());
                        let buffer = match channel {
                            BrowserChannel::Network => &mut live.network,
                            _ => &mut live.console,
                        };
                        (!buffer.is_empty()).then(|| {
                            let batch = buffer.take();
                            let cdp = std::mem::take(&mut live.cdp.dropped);
                            (batch, cdp)
                        })
                    }
                    None => None,
                }
            };
            if let Some((batch, cdp)) = pending {
                let (doc, truncated) = batch.document(id, channel.as_str(), cdp > 0);
                let published = self.publish_observation(s, channel, &doc, truncated, None);
                match published {
                    Ok(_) => {}
                    Err(e) => {
                        // Nothing was recorded: the notifications stay pending for a retry.
                        let mut guard = slot.live.lock().unwrap_or_else(|p| p.into_inner());
                        if let Some(live) = guard.as_mut() {
                            live.cdp.dropped += cdp;
                            match channel {
                                BrowserChannel::Network => live.network.restore(batch),
                                _ => live.console.restore(batch),
                            }
                        }
                        return Err(evidence_failure(e));
                    }
                }
            }
        }
        Ok(())
    }

    /// The live observation [`QueryHandler::prepare`] made for the session `id`, answered once
    /// the session is still readable in the gateway's snapshot.
    fn observed(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        id: &Id,
        observed: Option<&Observed>,
    ) -> Result<OperationResult, Failure> {
        self.readable(snapshot, ctx, id)?;
        match observed {
            Some(Observed::Result(r)) => Ok(r.clone()),
            _ => {
                Err(Refusal::new(DomainErrorCode::Conflict, SESSION_STATE, Some(id.clone())).into())
            }
        }
    }
}

/// What a browser read prepared outside the gateway's read snapshot
/// ([`QueryHandler::prepare`]): the live observation and the evidence published from it. Live
/// observation reads and writes the store (the session, evidence and its index), so it never
/// runs while the query's pooled reader is held.
enum Observed {
    /// An inspection or a screenshot, its evidence published.
    Result(OperationResult),
    /// The channel's pending notifications were published (or none were pending).
    Published,
}

/// One page (plus one row of lookahead) of a session's observations after `position`.
fn observation_page(
    snapshot: &dyn ReadSnapshot,
    id: &Id,
    channel: BrowserChannel,
    position: i64,
    limit: usize,
) -> Result<Vec<BrowserObservation>, StorageError> {
    // One row of lookahead, within the repository's page bound.
    let ask = (limit + 1).min(MAX_PAGE as usize) as u16;
    let mut rows = snapshot
        .browser()
        .browser_observations(id, channel, position, ask)?;
    if rows.len() == limit
        && usize::from(ask) == limit
        && let Some(last) = rows.last().map(|o| o.sequence)
    {
        rows.extend(
            snapshot
                .browser()
                .browser_observations(id, channel, last, 1)?,
        );
    }
    let mut items = Vec::new();
    for o in rows {
        let Some(v) = snapshot.artifacts().version(&o.version_id)? else {
            continue;
        };
        items.push(BrowserObservation {
            session_id: id.clone(),
            sequence: counter(o.sequence),
            evidence: crate::services::artifacts::version_ref(&v),
            truncated: o.truncated,
            tested_repository: o
                .tested_repository
                .map(serde_json::from_value)
                .transpose()
                .map_err(|_| StorageError::InvalidRecord("observation source".into()))?,
        });
    }
    Ok(items)
}

/// The staged file name of the `n`th file: `<n>-<safe name>`, at most
/// [`STAGED_NAME_MAX_BYTES`] UTF-8 bytes. A longer name loses the end of its stem (cut on a
/// character boundary) and keeps a short extension, so the platform never refuses it.
fn staged_name(n: usize, name: &str) -> String {
    let prefix = format!("{n}-");
    let base = safe_name(name);
    let budget = STAGED_NAME_MAX_BYTES - prefix.len();
    if base.len() <= budget {
        return prefix + &base;
    }
    let (stem, ext) = match base.rfind('.') {
        Some(d) if d > 0 && base.len() - d <= 16 => base.split_at(d),
        _ => (base.as_str(), ""),
    };
    let mut end = budget - ext.len();
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    let stem = stem[..end].trim_end_matches(['.', ' ']);
    let stem = if stem.is_empty() { "file" } else { stem };
    format!("{prefix}{stem}{ext}")
}

/// Whether an uncertain action's outcome is still not established.
fn unresolved(state: Option<OperationState>) -> bool {
    matches!(
        state,
        Some(
            OperationState::OutcomeUnknown
                | OperationState::NeedsInput
                | OperationState::Dispatching
        )
    )
}

fn completed(result: OperationResult) -> EffectOutcome {
    EffectOutcome::Completed(json!({"result": result}))
}

fn conflict(id: &Id, expected: &Revision, current: i64) -> Failure {
    Failure::Storage(StorageError::conflict(
        RevisionDimension::Connection,
        "browser_session",
        id.clone(),
        expected.clone(),
        rev(current),
    ))
}

fn evidence_failure(e: EvidenceFailure) -> Failure {
    match e {
        EvidenceFailure::Refused(r) => Refusal::new(r.code, r.target_kind, r.target_id)
            .retry(r.retry)
            .into(),
        EvidenceFailure::Storage(e) => e.into(),
    }
}

/// A query fault for an action error: a stale reference carries the current document revision
/// so a reader can take a fresh snapshot.
fn action_failure(e: ActionError, target: &BrowserTarget) -> Failure {
    match e {
        ActionError::Stale { kind, .. } => {
            let mut r = stale(kind);
            r.target_id = Some(target.target_id.clone());
            r.into()
        }
        ActionError::Refused { kind, .. } => Refusal::new(
            DomainErrorCode::UnsupportedTarget,
            kind,
            Some(target.target_id.clone()),
        )
        .into(),
        ActionError::Closing => Refusal::new(DomainErrorCode::Conflict, SESSION_STATE, None).into(),
        ActionError::Before(_) | ActionError::After(_) | ActionError::Navigation(_) => {
            Refusal::new(DomainErrorCode::ExternalFailure, "browser", None)
                .retry(RetryAdvice::SameCommand)
                .into()
        }
    }
}

/// Why [`BrowserService::resolve_unknown`] refused.
#[derive(Debug)]
pub enum ResolveRefusal {
    /// Only a person states what an uncertain action did.
    HumanOnly,
    NotFound,
    /// The operation's outcome is not unknown (it is pending or already established).
    NotUnknown(OperationState),
    Storage(StorageError),
}

/// What boot recovery of browser sessions did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BrowserRecovery {
    /// Sessions of earlier boots settled (their browser was verified gone).
    pub settled: usize,
    /// Sessions whose browser could not be verified stopped (kept `Uncertain`).
    pub uncertain: usize,
    /// Profile roots removed because no unsettled session owns them.
    pub profiles_removed: usize,
}

impl QueryHandler for BrowserService {
    /// Live reads (inspect, screenshot, and the publication of pending console and network
    /// notifications) run here, before the gateway takes its read snapshot: they read and
    /// write the store themselves, which must never happen while the query holds a pooled
    /// reader (a store with one reader would wait on itself).
    fn prepare(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, DomainFault> {
        let OperationInput::Browser(input) = query.input() else {
            return Ok(None);
        };
        let session = |id: &Id| -> Result<StoredBrowserSession, Failure> {
            let mut session = env.read(|s| Ok(session_of(s.browser(), ctx, id)))??;
            session.tested_repository =
                self.tested_source(ctx, &|| env.stopped(self.clock.now_ms().get()))?;
            Ok(session)
        };
        let observed = match input {
            ops::Input::Inspect(i) => session(&i.target.session_id)
                .and_then(|s| self.inspect(&s, &i.target))
                .map(Observed::Result),
            ops::Input::Screenshot(i) => session(&i.target.session_id)
                .and_then(|s| self.screenshot(&s, &i.target, i.full_page))
                .map(Observed::Result),
            ops::Input::Console(i) => session(&i.session_id)
                .and_then(|s| self.publish_pending(&s, BrowserChannel::Console))
                .map(|()| Observed::Published),
            ops::Input::Network(i) => session(&i.session_id)
                .and_then(|s| self.publish_pending(&s, BrowserChannel::Network))
                .map(|()| Observed::Published),
            _ => return Ok(None),
        };
        observed.map(|o| Some(Prepared::new(o))).map_err(|f| {
            let mut fault = query_fault(f, "browser_session", self.correlation());
            let target = match input {
                ops::Input::Inspect(i) => Some(&i.target),
                ops::Input::Screenshot(i) => Some(&i.target),
                _ => None,
            };
            if fault
                .target_kind
                .as_ref()
                .is_some_and(|k| k.0 == "browser_document")
                && let Some(target) = target
                && let Some(view) = self.view(&target.session_id)
                && let Some(current) = view.targets.get(target.target_id.as_str())
                && !current.crashed
                && current.frames.iter().any(|f| f == target.frame_id.as_str())
            {
                // The target and frame still exist. Return the observed revision so a
                // fresh read needs no navigation or guessed revision.
                fault.expected_revision = Some(target.document_revision.clone());
                fault.current_revision = Some(rev(current.document_revision));
                fault.revision_dimension =
                    Some(crate::protocol::support::RevisionDimension::Source);
            }
            fault
        })
    }

    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let OperationInput::Browser(input) = query.input() else {
            return Err(
                Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None)
                    .fault(self.correlation()),
            );
        };
        let observed = query.prepared::<Observed>();
        let answered = match input {
            ops::Input::SessionsQuery(i) => self.list(snapshot, ctx, i),
            ops::Input::Inspect(i) => self.observed(snapshot, ctx, &i.target.session_id, observed),
            ops::Input::Screenshot(i) => {
                self.observed(snapshot, ctx, &i.target.session_id, observed)
            }
            ops::Input::Console(i) => self
                .notices(
                    snapshot,
                    ctx,
                    &i.session_id,
                    i.after_sequence.get(),
                    &i.page,
                    BrowserChannel::Console,
                )
                .map(|value| OperationResult::BrowserConsole { value }),
            ops::Input::Network(i) => self
                .notices(
                    snapshot,
                    ctx,
                    &i.session_id,
                    i.after_sequence.get(),
                    &i.page,
                    BrowserChannel::Network,
                )
                .map(|value| OperationResult::BrowserNetwork { value }),
            _ => {
                Err(Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None).into())
            }
        };
        let result = answered.map_err(|f| query_fault(f, "browser_session", self.correlation()))?;
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(1)),
            result,
        })
    }
}

impl CommandHandler for BrowserService {
    /// Stages the exact authorized artifact versions an upload (or an artifact page) names into
    /// a new batch directory of the session's owned root. A raw path is never accepted: only
    /// artifacts the invocation may see now, with matching digest and length, are copied.
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        let close = matches!(
            command.input(),
            OperationInput::Browser(ops::Input::SessionsCommand(ops::SessionsCommandInput {
                action: BrowserSessionAction::Close { .. },
                ..
            }))
        );
        if close {
            return Ok(None);
        }
        let tested_repository =
            match self.tested_source(ctx, &|| env.stopped(self.clock.now_ms().get())) {
                Ok(source) => source,
                Err(failure) => {
                    return Ok(Some(Prepared::new(Staged::Refused(match failure {
                        Failure::Refused(r) => r,
                        _ => Refusal::new(DomainErrorCode::SourceChanged, "git_source", None),
                    }))));
                }
            };
        let binding =
            match crate::services::git::context::SourceBinding::prepare(ctx, &*self.store) {
                Ok(binding) => binding,
                Err(StorageError::ScopeDenied) => {
                    return Ok(Some(Prepared::new(Staged::Refused(Refusal::new(
                        DomainErrorCode::SourceChanged,
                        "git_source",
                        None,
                    )))));
                }
                Err(error) => return Err(error),
            }
            .map(serde_json::to_value)
            .transpose()
            .map_err(|_| StorageError::InvalidRecord("browser source binding".into()))?;
        let (session, files): (Id, Vec<ArtifactRef>) = match command.input() {
            OperationInput::Browser(ops::Input::Upload(i)) => {
                (i.node.target.session_id.clone(), i.files.0.clone())
            }
            OperationInput::Browser(ops::Input::Navigate(ops::NavigateInput {
                session_id,
                location: BrowserLocation::Artifact { artifact },
                ..
            })) => (session_id.clone(), vec![artifact.clone()]),
            _ => {
                return Ok(Some(Prepared::new(Staged::Source {
                    tested_repository,
                    binding,
                })));
            }
        };
        if env.stopped(self.clock.now_ms().get()) {
            return Err(StorageError::Cancelled);
        }
        // Nothing is staged into a session the invocation may not drive (decide refuses it).
        let (sid, c) = (session.clone(), ctx.clone());
        let drivable = env.read(move |s| {
            Ok(s.browser().browser_session(&sid)?.is_some_and(|b| {
                visible(&c, &b) && may_drive(&c, &b) && b.phase == BrowserPhase::Ready
            }))
        })?;
        if !drivable {
            return Ok(None);
        }
        let Some(profile) = self.view(&session).and_then(|v| v.profile) else {
            return Ok(Some(Prepared::new(Staged::Refused(Refusal::new(
                DomainErrorCode::Conflict,
                SESSION_STATE,
                Some(session),
            )))));
        };
        let batch = format!("upload-{}", self.ids.next_id(IdKind::SourceHandle).as_str());
        let mut paths = Vec::new();
        for (n, f) in files.iter().enumerate() {
            let read = match self.artifacts.open_exact_version(&*self.store, ctx, f) {
                Ok(r) => r,
                Err(r) => {
                    let _ = self.platform.remove_staged(&profile, &batch);
                    return Ok(Some(Prepared::new(Staged::Refused(Refusal::new(
                        r.code,
                        "browser_upload_file",
                        Some(f.version_id.clone()),
                    )))));
                }
            };
            let name = staged_name(n + 1, &read.name);
            let mut reader = read.reader;
            match self.platform.stage_file(
                &profile,
                &batch,
                &name,
                &mut reader,
                self.config
                    .stage_max_bytes
                    .min(f.byte_length.get().max(0) as u64),
            ) {
                Ok(p) => paths.push(p.to_string_lossy().into_owned()),
                Err(e) => {
                    let _ = self.platform.remove_staged(&profile, &batch);
                    return Err(StorageError::unavailable(
                        crate::ports::storage::Unavailable::Io,
                        e.to_string(),
                    ));
                }
            }
            if env.stopped(self.clock.now_ms().get()) {
                let _ = self.platform.remove_staged(&profile, &batch);
                return Err(StorageError::Cancelled);
            }
        }
        Ok(Some(Prepared::new(Staged::Files {
            session,
            profile,
            batch,
            paths,
            tested_repository,
            binding,
        })))
    }

    /// Removes a staged batch the unit of work did not record (a refusal, a replay, a denial or
    /// a rollback). A recorded batch stays until its session's root is removed at close.
    fn settle(
        &self,
        _ctx: &InvocationContext,
        prepared: Option<&Prepared>,
        reply: Option<&DomainCommandReply>,
        _env: &Preparation<'_>,
    ) {
        let Some(Staged::Files { profile, batch, .. }) = prepared.and_then(Prepared::get::<Staged>)
        else {
            return;
        };
        let recorded = matches!(
            reply,
            Some(CommandReply::Recorded { receipt, replayed: false, .. })
                if matches!(receipt.decision, DomainDecision::Accepted { .. })
        );
        if !recorded {
            let _ = self.platform.remove_staged(profile, batch);
        }
    }

    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        if let Some(Staged::Refused(r)) = command.prepared::<Staged>() {
            return decided(tx, Err(r.clone().into()));
        }
        if ctx.worktree().is_some() {
            let close = matches!(
                command.input(),
                OperationInput::Browser(ops::Input::SessionsCommand(ops::SessionsCommandInput {
                    action: BrowserSessionAction::Close { .. },
                    ..
                }))
            );
            if !close {
                let binding = staged_source(command).1.and_then(|v| {
                    serde_json::from_value::<crate::services::git::context::SourceBinding>(v).ok()
                });
                if !binding
                    .as_ref()
                    .map(|b| b.validate_in(tx))
                    .transpose()?
                    .unwrap_or(false)
                {
                    return decided(
                        tx,
                        Err(
                            Refusal::new(DomainErrorCode::SourceChanged, "git_source", None).into(),
                        ),
                    );
                }
            }
        }
        let outcome = match command.input() {
            OperationInput::Browser(ops::Input::SessionsCommand(i)) => match &i.action {
                BrowserSessionAction::Create {
                    scope,
                    browser_asset_id,
                } => self.create(tx, ctx, scope, browser_asset_id, command),
                BrowserSessionAction::Close {
                    session_id,
                    expected_revision,
                } => self.close(tx, ctx, session_id, expected_revision),
            },
            OperationInput::Browser(
                ops::Input::Navigate(_) | ops::Input::Input(_) | ops::Input::Upload(_),
            ) => self.action(tx, ctx, command),
            _ => {
                Err(Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None).into())
            }
        };
        match outcome {
            Ok(d) => Ok(d),
            Err(f) => decided(tx, Err(f)),
        }
    }
}
