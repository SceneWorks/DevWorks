//! The shell's one boundary to the application (ARCH-02/03/08).
//!
//! [`Backend`] admits a framed query or command for a host-selected operation through the
//! runtime's gateway — the same validation, authority, receipts and use cases an agent tool
//! reaches. The shell has no store, no SQL and no second implementation of any rule.
//!
//! [`Client`] runs every request off the UI thread (the runtime blocks its caller on the store),
//! on a small worker pool, and hands replies back to the frame loop tagged with the [`Slot`]
//! that asked. A newer request for the same slot supersedes an older one, so a late reply never
//! overwrites fresher state. [`Client::wait_idle`] is the synchronization point tests use
//! (a condition variable, never a sleep).
use devworks_core::gateway::{CommandOutcome, QueryOutcome};
use devworks_core::ports::secrets::Secret;
use devworks_core::protocol::CommandReply;
use devworks_core::protocol::Id;
use devworks_core::settings::view::Health;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Wakes the frame loop from another thread (egui `request_repaint`).
pub type Waker = Box<dyn Fn() + Send + Sync>;

/// The application operations the shell may use.
pub trait Backend: Send + Sync {
    /// Trusted host-only append of the selected coordinator conversation's screen context.
    fn append_coordinator_notice(
        &self,
        _session: &Id,
        _screen: &str,
        _context: &str,
    ) -> Result<(), String> {
        Err("coordinator context notices are unavailable".into())
    }
    fn read_coordinator_notice(
        &self,
        _reference: &devworks_core::protocol::provider::ArtifactRef,
    ) -> Result<String, String> {
        Err("coordinator context is unavailable".into())
    }
    /// Admits a query frame for `operation` with the host-bound user context.
    fn query(&self, operation: &str, frame: &[u8]) -> QueryOutcome;
    /// Only a host that forwards the token to the real gateway may advertise this seam.
    fn supports_query_cancellation(&self) -> bool {
        false
    }
    fn query_cancellable(
        &self,
        _operation: &str,
        _frame: &[u8],
        _options: &devworks_core::ports::storage::ReadOptions,
    ) -> QueryOutcome {
        Err(devworks_core::gateway::GatewayFailure::NoUseCase(
            "cancellable query ingress".into(),
        ))
    }
    /// Admits a command frame for `operation` with the host-bound user context.
    fn command(&self, operation: &str, frame: &[u8]) -> CommandOutcome;
    /// Resolve an accepted process operation to its scoped typed result once it settles.
    fn process_completion(
        &self,
        _operation: &devworks_core::protocol::OperationId,
    ) -> Result<
        Option<devworks_core::protocol::results::OperationCompletion>,
        devworks_core::protocol::DomainFault,
    > {
        Ok(None)
    }
    /// Scoped completion of a journaled desktop input; injection alone is not a test assertion.
    fn desktop_completion(
        &self,
        _operation: &devworks_core::protocol::OperationId,
    ) -> Result<
        Option<devworks_core::protocol::results::OperationCompletion>,
        devworks_core::protocol::DomainFault,
    > {
        Ok(None)
    }
    /// Trusted native user interaction only. Generic commands never select this ingress.
    fn human_decision(&self, _frame: &[u8]) -> CommandOutcome {
        Err(devworks_core::gateway::GatewayFailure::NoUseCase(
            "trusted native human decision ingress".into(),
        ))
    }
    fn confirm_project_purge(&self, _frame: &[u8]) -> CommandOutcome {
        Err(devworks_core::gateway::GatewayFailure::NoUseCase(
            "trusted native project purge confirmation ingress".into(),
        ))
    }
    fn purge_client_publish(
        &self,
        _memory: crate::memory::Memory,
        _live: crate::project_purge::Live,
        _save: bool,
    ) -> Result<Option<Vec<devworks_core::ports::storage::ProjectPurgeClientScope>>, String> {
        Ok(None)
    }
    fn purge_client_acknowledge(
        &self,
        _scope: &devworks_core::ports::storage::ProjectPurgeClientScope,
    ) {
    }
    fn purge_client_freeze(&self, _project: &str, _frozen: bool) -> Result<(), String> {
        Err("Native project purge custody is unavailable".into())
    }
    /// Volatile host-owned response text, authorized against the current session on each read.
    /// This is not a transcript entry or a generic tool operation.
    fn text_preview(
        &self,
        _session: &Id,
    ) -> Result<Option<devworks_engine::providers::live::LiveTurn>, String> {
        Ok(None)
    }
    /// Native window focus is host input, not an agent-controlled command argument.
    fn set_application_focus(&self, _focused: Option<bool>) {}
    /// Cached permission only; never opens an OS prompt on the frame loop.
    fn os_notification_permission(
        &self,
    ) -> devworks_core::ports::os_notifications::OsNotificationPermission {
        devworks_core::ports::os_notifications::OsNotificationPermission::Unavailable(
            "Native notifications are unavailable".into(),
        )
    }
    /// Called off the frame loop only after an explicit native user action.
    fn request_os_notification_permission(
        &self,
    ) -> devworks_core::ports::os_notifications::OsNotificationPermission {
        self.os_notification_permission()
    }
    /// Mints a workspace upload ticket for a file the user picked, dropped or pasted (user
    /// ingress only; the artifact service resolves and verifies it at `artifacts.save`).
    fn mint_workspace_upload(&self, file: &Path) -> Result<Id, String>;
    /// Mints an upload ticket bound to `project` for a file the app wrote for the user (a
    /// feature's plan text): it resolves only for an `artifacts.save` into that project, so
    /// the project's agents can read the artifact.
    fn mint_project_upload(
        &self,
        project: &devworks_core::protocol::ProjectId,
        file: &Path,
    ) -> Result<Id, String>;
    /// Keep-awake runtime health, distinct from the configured value (SET-02).
    fn keep_awake_health(&self) -> Health;
    /// "Queue state may have changed" hints.
    fn hints(&self) -> Option<watch::Receiver<u64>>;
    /// Registers the frame-loop waker hints call.
    fn set_waker(&self, waker: Waker);
    /// A fresh opaque token for request keys and query IDs.
    fn token(&self, prefix: &str) -> String;
    /// A directory for files the UI materializes before attaching them (pasted images).
    fn scratch_dir(&self) -> PathBuf;
    /// Stage native paste bytes under the stored session's scope before exposing a path.
    fn stage_pasted_image(&self, _session: Option<&Id>, _png: &[u8]) -> Result<PathBuf, String> {
        Err("pasted-image custody is not available here".into())
    }
    fn stage_project_text(
        &self,
        _project: Option<&devworks_core::protocol::ProjectId>,
        _extension: &str,
        _bytes: &[u8],
    ) -> Result<PathBuf, String> {
        Err("project text staging is not available here".into())
    }
    /// Reads at most `max` bytes of an `artifacts.read` stream handle through the artifact
    /// service's verified stream (re-authorized, liveness-checked, reader-held).
    fn read_stream(&self, stream: &Id, max: u64) -> Result<Vec<u8>, String>;
    /// Probes package runtimes and refreshes the signed package indexes (blocking; off the UI
    /// thread).
    fn refresh_packages(&self) -> Result<(), String> {
        Err("packages are not available here".into())
    }
    /// Downloads the bundle a verified index offers into the UI staging directory (verified
    /// against the signed digests) and returns its path.
    fn fetch_index_bundle(&self, _package: &str, _digest: &str) -> Result<PathBuf, String> {
        Err("packages are not available here".into())
    }
    /// The index source configuration of the workspace (or a project) section, read for the
    /// form that edits it through `indexing.configure_sources` (none without indexing).
    fn index_sources(
        &self,
        _project: Option<&str>,
    ) -> Result<devworks_core::ports::storage::StoredSourceConfig, String> {
        Err("repository indexing is not available".into())
    }
    /// The native maintenance capabilities a scheduled job may run in this build (only those
    /// with a registered use case; none are advertised otherwise).
    fn native_job_capabilities(&self) -> Vec<devworks_core::services::jobs::NativeCapability> {
        Vec::new()
    }
    /// Human-only retained AES key utility. The returned value is transient and redacted.
    fn generate_database_encryption_key(&self) -> Result<Secret, String> {
        Err("Encryption key generation is unavailable".into())
    }
    /// Human-only purpose-bound database password intake; only its opaque handle enters commands.
    fn stage_database_password(&self, _password: Secret) -> Result<Id, String> {
        Err("Database credential intake is unavailable".into())
    }
    /// Stages the user's provider API key without putting raw bytes in a frame or receipt.
    fn stage_api_key(&self, _key: Secret) -> Result<Id, String> {
        Err("credential entry is not available".into())
    }
    fn stage_mcp_api_key(&self, _key: Secret) -> Result<Id, String> {
        Err("MCP credential entry is not available".into())
    }
    /// Deletes the stored secrets of MCP handles that no saved server references any more.
    fn release_mcp_credentials(&self, _handles: Vec<Id>) -> Result<(), String> {
        Ok(())
    }
    /// Attaches the `code#state` value the user pasted to their open browser sign-in, for the
    /// `CompleteSignIn` command that follows (user ingress only; kept in memory).
    fn submit_sign_in_code(&self, _flow: &Id, _pasted: Secret) -> Result<(), String> {
        Err("sign-in is not available".into())
    }
    /// Writes a settings credential the user typed (a `{"secretRef": ...}` setting such as the web
    /// search key) into the OS secret store and returns its reference (user ingress only).
    fn stage_setting_secret(&self, _value: Secret) -> Result<String, String> {
        Err("credential entry is not available".into())
    }
    /// The secret facility in use, whether credentials are kept for this session only, and
    /// whether it refused its last request (structured, never parsed from a message).
    fn secret_store(&self) -> SecretStoreStatus {
        SecretStoreStatus {
            name: "unavailable".into(),
            session_only: false,
            unavailable: None,
        }
    }
    /// The user's explicit choice to keep credentials in memory for this session only (when
    /// the OS facility is unavailable). Never a silent plaintext disk fallback.
    fn use_session_only_secrets(&self) -> Result<(), String> {
        Err("session-only credentials are not available".into())
    }
}

/// The secret facility as Settings shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SecretStoreStatus {
    /// The facility's name (for example "macOS Keychain").
    pub name: String,
    /// Credentials are kept in memory for this session only.
    pub session_only: bool,
    /// Why the facility refused its last request (locked, missing or denied), until a request
    /// succeeds: staging a key, a sign-in exchange, a refresh or a credential read.
    pub unavailable: Option<String>,
}

/// Who is waiting for a reply. Equal slots supersede each other.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Slot {
    PurgePrepare,
    PurgePage,
    PurgeConfirm,
    PurgeStatus,
    DatabaseList(u64, bool),
    DatabaseObjects(u64, bool),
    DatabaseRelations(u64, bool),
    DatabasePrepare,
    DatabaseCaPrepare,
    DatabaseCa,
    DatabaseCommand(String),
    GraphRead(u64),
    GraphTrust(String),
    Usage(u64, bool),
    UsageOptions(u64),
    Prices(u64, bool),
    PriceProbe(u64),
    PriceOverride,
    PriceRefresh,
    PriceOperation,
    ApprovalsList,
    ApprovalInspect(String),
    ApprovalRefresh(String),
    ApprovalContent(String),
    ApprovalDecision(String),
    ReviewQueue,
    NotificationsList,
    CompletionList,
    NotificationRead(String),
    NotificationChange(String),
    NotificationSettings,
    NotificationSettingsSave,
    OsNotificationPermission,
    SettingsRead,
    ContainerCapabilities,
    SettingsValidate,
    SettingsHealth,
    SettingsSave,
    /// Transient native credential intake; no raw value is serialized with this slot.
    SettingsSecret(String),
    SettingsExplain(String),
    SessionsList,
    SessionStart,
    Transcript(String),
    TextPreview(String, u64),
    Append(String),
    /// An attachment being stored for a draft (`draft`, local attachment ID).
    Attach(String, u64),
    /// A decoded preview for a pending attachment.
    Preview(String, u64),
    /// Deleting an attachment removed from its draft (artifact ID).
    DeleteAttachment(String),
    ArtifactsList,
    ResourcesList,
    /// The workspace's browser sessions.
    BrowserSessions,
    /// The browser sessions of one project (its ID).
    ProjectBrowserSessions(String),
    /// Closing a browser session (its ID).
    BrowserClose(String),
    /// The Add-ons catalog.
    AddonsList,
    /// One package's logs.
    AddonsLogs(String),
    /// The Add-ons screen's one lifecycle command (or install-from-file task) at a time.
    AddonsCommand,
    /// Bundle artifact save before the exact install/update command; retained at close.
    AddonsSave {
        update: Option<String>,
        registry: i64,
        #[serde(default)]
        download: Option<PathBuf>,
    },
    /// The runtime probe and index refresh behind the Add-ons screen.
    AddonsDiscovery,
    /// A page of the router catalog for the composer's selectors (its load generation).
    RouterCatalog(u64),
    /// A conversation whose first route needs its session record (session ID).
    RouteSession(String),
    /// The availability (and pin) of a conversation's selection (conversation key).
    RouteStatus(String),
    /// An explicit route/effort change of a pinned conversation (session ID).
    RoutePin(String),
    /// The first routed request of a conversation (session ID): its selection becomes its pin.
    RouteFirst(String),
    /// A page fetch from Settings > Agent tools.
    WebFetch,
    /// A search from Settings > Agent tools.
    WebSearch,
    /// The recent web activity record.
    WebActivity,
    McpServers(String, Option<String>),
    McpOperation(String),
    McpCapabilities(String, String, Option<String>),
    /// A resource or prompt read from Settings (scope key, server ID).
    McpContent(String, String),
    McpCommand,
    /// Deletes MCP secrets a configuration change left unreferenced.
    McpRelease,
    /// Active and archived projects.
    ProjectsList,
    /// Deleted projects (their tombstones).
    DeletedProjects,
    /// Every registered repository.
    RepositoriesList,
    /// The repositories of one project.
    ProjectRepositories(String),
    ProjectCreate,
    /// An edit, membership or lifecycle command of one project (one at a time per project).
    ProjectCommand(String),
    /// A project's repository base directory setting (read, health, save).
    ProjectSettings(String),
    ProjectSettingsHealth(String),
    ProjectSettingsSave(String),
    /// A repository's README (`true`) or summary text.
    RepositoryText(String, bool),
    /// The artifact content of a repository's README (`true`) or summary.
    RepositoryContent(String, bool),
    RemoveIndex(String),
    /// A repository's index status (repository ID).
    IndexStatus(String),
    /// Index runs (repository ID, or `*` for the selected scope).
    IndexRuns(String),
    /// An index, rebuild or overlay command of one repository (repository ID).
    IndexCommand(String),
    /// A run cancellation (run ID).
    IndexCancel(String),
    /// An overlay retirement (overlay ID).
    IndexRetire(String),
    /// The section configuration save.
    IndexSourcesSave,
    /// A discovery scan.
    IndexDiscover,
    /// A Knowledge listing page (its request generation).
    ConventionsList(String, u64, bool, bool),
    ConventionRead(String, u64),
    ConventionEditor(String, String, u64),
    ConventionVersions(String, u64, bool),
    ConventionResources(String, u64, bool),
    ConventionParents(String, String, u64, bool),
    ConventionCommand(String, String),
    ConventionOperation(String, String),
    ConventionPrepare(String, String),
    ConventionArtifact(String, String),
    ConventionTransfer(String, String),
    KnowledgeList(u64),
    /// One Knowledge entry with its evidence and history (claim ID).
    KnowledgeRead(String),
    /// Knowledge counts and retrieval mode.
    KnowledgeDiagnostics,
    /// The preview of a delete by source (the source).
    KnowledgePreview(String),
    /// Storing an import file as the user's artifact.
    KnowledgeImportSave,
    /// The exact preview of an import.
    KnowledgeImportPreview,
    /// The confirmed import.
    KnowledgeImport,
    /// A Knowledge write (record, revise, tag, lifecycle change, deletion).
    KnowledgeWrite,
    /// The jobs of the selected scope, and a further page of them.
    JobsList,
    /// A further page of the shown jobs list (that list's generation).
    JobsMore(u64),
    /// A further page re-read by a refresh of the shown jobs list (the refresh's generation).
    JobsRefresh(u64),
    /// The jobs of one project, for its run status on All projects.
    ProjectJobs(String),
    /// One job's inspection (its first run-history page) and an older history page (job ID).
    JobInspect(String),
    JobHistory(String),
    /// The job's active run, inspected by its ID (job ID): the latest occurrence may be a
    /// skipped one while the active run continues.
    JobActiveRun(String),
    /// A scoped prompt page of one form generation (None is workspace).
    JobPrompts(u64, Option<String>),
    JobCreate,
    /// A command of one job or its run (one at a time per job).
    JobCommand(String),
    /// Every feature of a project (all pages, read in order).
    WorkFeatures(String),
    /// The complete authoritative workflow, fenced by load generation.
    WorkWorkflow(String, u64),
    WorkWorkflowCommand(String),
    /// A queue snapshot; a lease requests a claim after the read completes.
    AllocationQueue(String, Option<String>, Option<u64>),
    AllocationClaim(String, Option<String>),
    AllocationInspect(String),
    AllocationRelease(String),
    /// Every card (feature task or standalone issue) of a project.
    WorkCards(String),
    /// The project change feed (Activity) of a project.
    WorkActivity(String, Option<String>, u64),
    WorkMessagesHistory(String),
    WorkMessagesInbox(String),
    WorkMessagesEvents(String),
    WorkMessagePost(String),
    WorkMessageAck(String, String),
    /// One work item's current record (readiness included).
    WorkDetail(String),
    WorkGit(String),
    WorkGitInspect(String, String),
    GitInspect(String),
    GitDiff(String),
    GitDiffContent(String),
    /// A change of one work item (one at a time per item).
    WorkCommand(String),
    /// A new feature, task or issue in a project.
    WorkCreate(String),
    /// A prioritization of one queue (`<project>/<queue>`).
    WorkOrder(String),
    /// A plan text stored as an artifact for a feature's pending plan.
    WorkPlanText(String),
    /// Project-owned decisions for requirements dropped by a plan save.
    WorkPlanDrop(String),
    /// A feature's plan draft save (`features.edit_plan`).
    WorkPlan(String),
    /// A root's run controls (inspect) and one Start/Pause/Continue.
    WorkInspect(String),
    WorkRun(String),
    /// The roots and running work of a project an archival would stop (features, issues).
    ArchiveCount(String, bool),
    /// Work opened from elsewhere (a completion card): read as a card (`false`) or a feature.
    WorkOpen(String, bool),
    /// A page of `router.connections` for Settings → Provider connections (load generation).
    Connections(u64),
    /// A page of the full catalog (disabled models included) for the connections panel.
    ConnectionCatalog(u64),
    /// One bounded persistent embedding-space status page.
    EmbeddingSpaces(u64),
    /// One `router.configure` submission from the connections panel (its sequence number).
    Configure(u64),
    /// Settings → Roles and skills: a discovery page (load generation).
    InstructionSkills(u64),
    /// The active record of one role (role item ID).
    InstructionRole(String),
    /// A history page of the selected item (detail generation).
    InstructionHistory(u64),
    /// The text of the shown revision (detail generation).
    InstructionText(u64),
    /// A resources page of the selected skill (detail generation).
    InstructionResources(u64),
    /// Staging a draft's text (change token).
    InstructionPrepare(u64),
    /// The draft text's `artifacts.save` (change token).
    InstructionArtifact(u64),
    /// The `*.draft` / `*.rollback` command (change token).
    InstructionCommand(u64),
    // Migrated screens' slots, after every tagged variant: each serializes as its inner value
    // alone, so persisted commands keep the JSON they had in the single enum.
    #[serde(untagged)]
    Help(crate::help_ops::HelpSlot),
    #[serde(untagged)]
    Watch(crate::watch_ops::WatchSlot),
}
impl Slot {
    /// The screen whose replies this slot waits for.
    pub fn screen(&self) -> crate::screen::ScreenId {
        match self {
            Self::Help(_) => crate::screen::ScreenId::Help,
            Self::Watch(_) => crate::screen::ScreenId::Watchers,
            _ => crate::screen::ScreenId::Legacy,
        }
    }
}

/// A finished request.
pub enum Reply {
    OsNotificationPermission(devworks_core::ports::os_notifications::OsNotificationPermission),
    Query(QueryOutcome),
    Command(CommandOutcome),
    /// A request that ran several steps (attachment storage) or local work (preview decoding).
    Task(Result<TaskOutput, String>),
}

/// The output of a multi-step task.
pub enum TaskOutput {
    /// The credential is already in SecretStore. Value is an opaque reference or null.
    SettingSecret {
        frame: Vec<u8>,
        key: String,
        value: serde_json::Value,
    },
    /// A prepared command frame (attachment, provider or package), not admitted yet.
    Prepared { frame: Vec<u8> },
    /// A downloaded bundle stays owned until its artifact-save command is confirmed.
    PackagePrepared {
        frame: Vec<u8>,
        download: crate::addons::PreparedDownload,
    },
    /// An attachment's `artifacts.save` outcome with the exact frame it submitted (an unknown
    /// outcome is confirmed by resubmitting that frame, never a new one).
    Stored {
        outcome: CommandOutcome,
        frame: Vec<u8>,
    },
    /// A decoded preview image (RGBA, unmultiplied).
    Preview { size: [usize; 2], rgba: Vec<u8> },
    /// An authorized, volatile provider response snapshot.
    TextPreview(Option<devworks_engine::providers::live::LiveTurn>),
    /// A bounded convention editor or transfer task.
    Convention(crate::conventions_ops::TaskResult),
    /// Instruction text read or a staged draft (Roles and skills).
    Instruction(crate::instructions_ops::TaskResult),
    /// Package runtimes and indexes were refreshed.
    Refreshed,
    /// The text of an MCP resource or prompt a person opened in Settings.
    McpContent(String),
}

type Job = Box<dyn FnOnce(&dyn Backend) -> Reply + Send>;

/// Identical resubmissions of a command whose outcome came back unknown, before the shell
/// reports it as not confirmed.
pub const UNKNOWN_RESUBMISSIONS: usize = 3;

/// Whether a command's outcome is not known (a lost acknowledgment).
pub fn is_unknown(outcome: &CommandOutcome) -> bool {
    matches!(outcome, Ok(CommandReply::CommandStatusUnknown { .. }))
}

/// Admits `frame`; while its outcome is unknown, resubmits the **identical** frame (same
/// request key and fingerprint) up to [`UNKNOWN_RESUBMISSIONS`] times. The gateway replays the
/// recorded receipt of a command that committed, and decides one that did not, so a
/// resubmission never duplicates an effect. A fresh request key is never used for a retry.
pub fn resolve_command(backend: &dyn Backend, operation: &str, frame: &[u8]) -> CommandOutcome {
    resolve_dispatched(backend, CommandDispatch::Generic, operation, frame)
}

fn resolve_dispatched(
    backend: &dyn Backend,
    dispatch: CommandDispatch,
    operation: &str,
    frame: &[u8],
) -> CommandOutcome {
    let invoke = || match dispatch {
        CommandDispatch::Generic => backend.command(operation, frame),
        CommandDispatch::HumanDecision => backend.human_decision(frame),
        CommandDispatch::ProjectPurge => backend.confirm_project_purge(frame),
    };
    let mut outcome = invoke();
    for _ in 0..UNKNOWN_RESUBMISSIONS {
        if !is_unknown(&outcome) {
            break;
        }
        outcome = invoke();
    }
    // An older command envelope cannot carry the current terminal representation.
    // Resolve only an explicit receipt hold, under the exact original fingerprint.
    if matches!(
        &outcome,
        Ok(CommandReply::Rejected {
            code: devworks_core::protocol::Rejection::UnsupportedSchema,
            retry: devworks_core::protocol::RetryAdvice::ResolveReceipt,
            ..
        })
    ) && let Ok(decoded) = devworks_core::admission::decode(
        devworks_core::admission::Selection {
            operation,
            mode: devworks_core::registry::Mode::Command,
        },
        frame,
    ) && let devworks_core::admission::CallerKey::Command(request_key) = decoded.key
    {
        let lookup = serde_json::json!({"protocol":"1.0-draft.1","schemaRevision":6,"queryId":backend.token("purged-receipt"),"query":{"kind":"history.command_receipt","requestKey":request_key}}).to_string().into_bytes();
        if let Ok(devworks_core::protocol::results::DomainQueryReply::Result {
            result:
                devworks_core::protocol::results::OperationResult::HistoryCommandReceipt { value },
            ..
        }) = backend.query("history.command_receipt", &lookup)
            && value.request_key == request_key
            && let Some(purged) = value.purged
            && purged.request_digest.as_str() == decoded.fingerprint.as_str()
        {
            return Ok(CommandReply::ReceiptPurged {
                protocol: devworks_core::protocol::Protocol::Draft1,
                request_key,
                outcome: purged,
            });
        }
    }

    outcome
}

/// The host ingress chosen by the native interaction, retained with exact retry frames.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CommandDispatch {
    #[default]
    Generic,
    HumanDecision,
    ProjectPurge,
}

struct PendingQuery {
    operation: &'static str,
    frame: Vec<u8>,
    options: Option<devworks_core::ports::storage::ReadOptions>,
}

struct Pending {
    slot: Slot,
    ticket: u64,
    label: String,
    started: Instant,
    /// A command's operation and exact frame, kept until its outcome is known.
    command: Option<(&'static str, Vec<u8>, CommandDispatch)>,
    query: Option<PendingQuery>,
}

/// Whether the core registry binds `operation` in command mode: every such operation is
/// restorable, so a command the shell adds later is never dropped at relaunch.
fn registered_command(operation: &str) -> Option<&'static str> {
    devworks_core::registry::find(operation)
        .filter(|d| {
            d.modes
                .iter()
                .any(|m| m.mode == devworks_core::registry::Mode::Command)
        })
        .map(|d| d.name)
}

/// A command whose outcome is still not known after its resubmissions.
#[derive(Clone, Debug)]
pub struct Unresolved {
    pub slot: Slot,
    pub label: String,
    operation: &'static str,
    frame: Vec<u8>,
    pub dispatch: CommandDispatch,
}
impl Unresolved {
    pub fn operation(&self) -> &'static str {
        self.operation
    }
    pub fn frame(&self) -> &[u8] {
        &self.frame
    }
    /// One kept across a restart; `None` for an operation the shell does not admit.
    pub fn restored(slot: Slot, label: String, operation: &str, frame: Vec<u8>) -> Option<Self> {
        Self::restored_dispatch(slot, label, operation, frame, CommandDispatch::Generic)
    }
    pub fn restored_dispatch(
        slot: Slot,
        label: String,
        operation: &str,
        frame: Vec<u8>,
        dispatch: CommandDispatch,
    ) -> Option<Self> {
        let operation = registered_command(operation)?;
        if dispatch == CommandDispatch::HumanDecision
            && operation != "approvals.record_human_decision"
        {
            return None;
        }
        if dispatch == CommandDispatch::ProjectPurge && operation != "projects.purge" {
            return None;
        }
        // Restored disk state can resolve an existing receipt, but cannot create
        // a new native Human confirmation. A missing original receipt requires
        // another explicit review; a dispatch flag is never consent.
        let dispatch = if dispatch == CommandDispatch::ProjectPurge {
            CommandDispatch::Generic
        } else {
            dispatch
        };
        Some(Self {
            slot,
            label,
            operation,
            frame,
            dispatch,
        })
    }
}

struct Shared {
    in_flight: Mutex<usize>,
    idle: Condvar,
    /// Worker threads still running (a guard decrements it on exit, panics included).
    alive: Mutex<usize>,
    exited: Condvar,
}

/// Decrements [`Shared::alive`] when a worker thread ends, however it ends.
struct Alive(Arc<Shared>);
impl Drop for Alive {
    fn drop(&mut self) {
        let mut n = self.0.alive.lock().unwrap_or_else(|p| p.into_inner());
        *n = n.saturating_sub(1);
        self.0.exited.notify_all();
    }
}

/// Longest wait for request workers when the client is dropped. A request stuck in the
/// operating system (for example a read that never returns) must not keep the app from
/// exiting: after this bound its thread is detached and ends with the process.
pub const WORKER_JOIN_BOUND: Duration = Duration::from_secs(15);

/// A long-running request as the status surface shows it (UI-06).
#[derive(Clone, Debug)]
pub struct Activity {
    pub label: String,
    pub elapsed: Duration,
}

/// Runs requests on worker threads and returns their replies to the frame loop.
pub struct Client {
    purge_reviewing: bool,
    backend: Arc<dyn Backend>,
    jobs: Sender<(u64, Job)>,
    /// Network-bound tasks, served by their own workers (see [`Client::network_task`]).
    network_jobs: Sender<(u64, Job)>,
    replies: Receiver<(u64, Reply)>,
    reply_tx: Sender<(u64, Reply)>,
    pending: Vec<Pending>,
    latest: HashMap<Slot, u64>,
    next: u64,
    shared: Arc<Shared>,
    waker: Arc<Mutex<Option<Waker>>>,
    open: bool,
    unresolved: Vec<Unresolved>,
    workers: Vec<std::thread::JoinHandle<()>>,
    join_bound: Duration,
}

/// Worker threads for store queries, commands and local tasks; requests rarely contend, but an
/// attachment upload must not hold up a settings read.
const WORKERS: usize = 3;
/// Worker threads for tasks that wait on the network (package indexes, add-on downloads, MCP
/// servers). They have their own queue, so slow remote calls never hold a store worker.
const NETWORK_WORKERS: usize = 2;

impl Client {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        let (jobs, job_rx) = channel::<(u64, Job)>();
        let (network_jobs, network_rx) = channel::<(u64, Job)>();
        let (reply_tx, replies) = channel::<(u64, Reply)>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let network_rx = Arc::new(Mutex::new(network_rx));
        let shared = Arc::new(Shared {
            in_flight: Mutex::new(0),
            idle: Condvar::new(),
            alive: Mutex::new(0),
            exited: Condvar::new(),
        });
        let waker: Arc<Mutex<Option<Waker>>> = Arc::new(Mutex::new(None));
        let mut workers = Vec::with_capacity(WORKERS + NETWORK_WORKERS);
        let pools = std::iter::repeat_n(("request", &job_rx), WORKERS).chain(std::iter::repeat_n(
            ("network", &network_rx),
            NETWORK_WORKERS,
        ));
        for (n, (pool, queue)) in pools.enumerate() {
            let (rx, tx, be, sh, wk) = (
                queue.clone(),
                reply_tx.clone(),
                backend.clone(),
                shared.clone(),
                waker.clone(),
            );
            *shared.alive.lock().unwrap_or_else(|p| p.into_inner()) += 1;
            let guard = Alive(shared.clone());
            let spawned = std::thread::Builder::new()
                .name(format!("devworks-ui-{pool}-{n}"))
                .spawn(move || {
                    let _alive = guard;
                    loop {
                        let next = rx.lock().unwrap_or_else(|p| p.into_inner()).recv();
                        let Ok((ticket, job)) = next else { return };
                        let reply =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&*be)))
                                .unwrap_or_else(|_| {
                                    Reply::Task(Err("the request panicked".into()))
                                });
                        let _ = tx.send((ticket, reply));
                        {
                            let mut n = sh.in_flight.lock().unwrap_or_else(|p| p.into_inner());
                            *n = n.saturating_sub(1);
                            sh.idle.notify_all();
                        }
                        if let Some(w) = wk.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
                            w();
                        }
                    }
                });
            // A worker that could not start already dropped its guard with the closure.
            if let Ok(handle) = spawned {
                workers.push(handle);
            }
        }
        Self {
            purge_reviewing: false,
            backend,
            jobs,
            network_jobs,
            replies,
            reply_tx,
            pending: Vec::new(),
            latest: HashMap::new(),
            next: 0,
            shared,
            waker,
            open: true,
            unresolved: Vec::new(),
            workers,
            join_bound: WORKER_JOIN_BOUND,
        }
    }

    /// Changes how long dropping the client waits for its workers (tests).
    pub fn set_join_bound(&mut self, bound: Duration) {
        self.join_bound = bound;
    }

    pub fn set_purge_reviewing(&mut self, reviewing: bool) {
        self.purge_reviewing = reviewing;
    }
    pub fn backend(&self) -> &Arc<dyn Backend> {
        &self.backend
    }

    /// Registers the frame-loop waker for replies and runtime hints.
    pub fn set_waker(&self, make: impl Fn() -> Waker) {
        *self.waker.lock().unwrap_or_else(|p| p.into_inner()) = Some(make());
        self.backend.set_waker(make());
    }

    /// Whether new requests are admitted (closed once shutdown begins).
    pub fn is_open(&self) -> bool {
        self.open
    }
    /// Stops admitting new requests (shutdown: Quiescing).
    pub fn close(&mut self) {
        self.open = false;
    }

    fn submit(
        &mut self,
        slot: Slot,
        label: &str,
        job: Job,
        command: Option<(&'static str, Vec<u8>, CommandDispatch)>,
    ) -> Option<u64> {
        self.submit_to(false, slot, label, job, command)
    }

    fn submit_to(
        &mut self,
        network: bool,
        slot: Slot,
        label: &str,
        job: Job,
        command: Option<(&'static str, Vec<u8>, CommandDispatch)>,
    ) -> Option<u64> {
        if !self.open {
            return None;
        }
        self.next += 1;
        let ticket = self.next;
        self.latest.insert(slot.clone(), ticket);
        self.pending.push(Pending {
            slot,
            ticket,
            label: label.to_owned(),
            started: Instant::now(),
            command,
            query: None,
        });
        *self
            .shared
            .in_flight
            .lock()
            .unwrap_or_else(|p| p.into_inner()) += 1;
        let queue = if network {
            &self.network_jobs
        } else {
            &self.jobs
        };
        if queue.send((ticket, job)).is_err() {
            // No worker could start: report the request as failed rather than leave it pending.
            let mut n = self
                .shared
                .in_flight
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            *n = n.saturating_sub(1);
            let _ = self.reply_tx.send((
                ticket,
                Reply::Task(Err("no request worker is running".into())),
            ));
        }
        Some(ticket)
    }

    /// Submits a query; `false` when the client is closed and nothing was submitted.
    pub fn query(
        &mut self,
        slot: Slot,
        label: &str,
        operation: &'static str,
        frame: Vec<u8>,
    ) -> bool {
        let kept = frame.clone();
        let ticket = self.submit(
            slot,
            label,
            Box::new(move |b| Reply::Query(b.query(operation, &frame))),
            None,
        );
        if let Some(ticket) = ticket
            && let Some(p) = self.pending.iter_mut().find(|p| p.ticket == ticket)
        {
            p.query = Some(PendingQuery {
                operation,
                frame: kept,
                options: None,
            });
        }
        ticket.is_some()
    }

    pub fn query_cancellable(
        &mut self,
        slot: Slot,
        label: &str,
        operation: &'static str,
        frame: Vec<u8>,
        options: devworks_core::ports::storage::ReadOptions,
    ) -> bool {
        let kept = frame.clone();
        let kept_options = options.clone();
        let ticket = self.submit(
            slot,
            label,
            Box::new(move |b| Reply::Query(b.query_cancellable(operation, &frame, &options))),
            None,
        );
        if let Some(ticket) = ticket
            && let Some(p) = self.pending.iter_mut().find(|p| p.ticket == ticket)
        {
            p.query = Some(PendingQuery {
                operation,
                frame: kept,
                options: Some(kept_options),
            });
        }
        ticket.is_some()
    }

    /// Admits a command through [`resolve_command`]. If its outcome is still unknown, the exact
    /// frame is kept as [`Unresolved`] until [`Self::resubmit`] confirms it.
    ///
    /// Returns whether it was submitted now (`false`: the client is closed, and the command is
    /// kept unresolved).
    pub fn command(
        &mut self,
        slot: Slot,
        label: &str,
        operation: &'static str,
        frame: Vec<u8>,
    ) -> bool {
        self.dispatched_command(slot, label, operation, frame, CommandDispatch::Generic)
    }

    /// Called only by the native approval decision controls, never generic command dispatch.
    pub fn human_decision(&mut self, slot: Slot, label: &str, frame: Vec<u8>) -> bool {
        self.dispatched_command(
            slot,
            label,
            "approvals.record_human_decision",
            frame,
            CommandDispatch::HumanDecision,
        )
    }

    /// Called only after the user reviews and confirms the exact purge manifest.
    pub fn confirm_project_purge(&mut self, slot: Slot, label: &str, frame: Vec<u8>) -> bool {
        self.dispatched_command(
            slot,
            label,
            "projects.purge",
            frame,
            CommandDispatch::ProjectPurge,
        )
    }

    fn dispatched_command(
        &mut self,
        slot: Slot,
        label: &str,
        operation: &'static str,
        frame: Vec<u8>,
        dispatch: CommandDispatch,
    ) -> bool {
        if self.purge_reviewing && !matches!(operation, "projects.prepare_purge" | "projects.purge")
        {
            return false;
        }
        // A slot holding an unresolved command keeps it until `resubmit_unresolved` settles
        // that exact frame: a new request would replace it, and could commit a second time.
        if self.unresolved.iter().any(|u| u.slot == slot) {
            return false;
        }
        let kept = frame.clone();
        let job: Job =
            Box::new(move |b| Reply::Command(resolve_dispatched(b, dispatch, operation, &frame)));
        if self
            .submit(
                slot.clone(),
                label,
                job,
                Some((operation, kept.clone(), dispatch)),
            )
            .is_some()
        {
            self.unresolved.retain(|u| u.slot != slot);
            return true;
        } else if !self.unresolved.iter().any(|u| u.slot == slot) {
            // Closed: nothing ran, so an unknown outcome stays unresolved (never dropped).
            self.unresolved.push(Unresolved {
                slot,
                label: label.to_owned(),
                operation,
                frame: kept,
                dispatch,
            });
        }
        false
    }

    /// Keeps an unresolved command restored from a previous run.
    pub fn keep_unresolved(&mut self, u: Unresolved) {
        self.unresolved.retain(|k| k.slot != u.slot);
        self.unresolved.push(u);
    }

    /// Commands whose outcome is still not known.
    pub fn unresolved(&self) -> &[Unresolved] {
        &self.unresolved
    }

    /// Every command whose outcome is not known: the unresolved ones and those still running
    /// (newest per slot). A close keeps these, so the next launch confirms each exact frame.
    pub fn unconfirmed(&self) -> Vec<Unresolved> {
        let mut out = self.unresolved.clone();
        for p in self.pending.iter().rev() {
            if let Some((operation, frame, dispatch)) = &p.command
                && !out.iter().any(|u| u.slot == p.slot)
            {
                out.push(Unresolved {
                    slot: p.slot.clone(),
                    label: p.label.clone(),
                    operation,
                    frame: frame.clone(),
                    dispatch: *dispatch,
                });
            }
        }
        out
    }

    /// Resubmits every unresolved command with its identical frame.
    pub fn resubmit_unresolved(&mut self) {
        for u in std::mem::take(&mut self.unresolved) {
            self.dispatched_command(u.slot, &u.label, u.operation, u.frame, u.dispatch);
        }
    }

    /// Explicit native permission interaction, off the GUI frame loop.
    pub fn request_notification_permission(&mut self) -> bool {
        self.submit(
            Slot::OsNotificationPermission,
            "Requesting native notification permission",
            Box::new(|b| Reply::OsNotificationPermission(b.request_os_notification_permission())),
            None,
        )
        .is_some()
    }

    /// Runs a multi-step task (for example: mint a ticket, hash the file, save the artifact).
    pub fn task(
        &mut self,
        slot: Slot,
        label: &str,
        task: impl FnOnce(&dyn Backend) -> Result<TaskOutput, String> + Send + 'static,
    ) -> bool {
        if self.purge_reviewing {
            return false;
        }
        self.submit(slot, label, Box::new(move |b| Reply::Task(task(b))), None)
            .is_some()
    }

    /// A [`Self::task`] that waits on the network. It runs on the network workers, so however
    /// long it takes, store queries and commands keep their own workers.
    pub fn network_task(
        &mut self,
        slot: Slot,
        label: &str,
        task: impl FnOnce(&dyn Backend) -> Result<TaskOutput, String> + Send + 'static,
    ) -> bool {
        if self.purge_reviewing {
            return false;
        }
        self.submit_to(
            true,
            slot,
            label,
            Box::new(move |b| Reply::Task(task(b))),
            None,
        )
        .is_some()
    }

    /// Replies that arrived since the last call, for the slots still waiting for them, each
    /// with its ticket. Handling one reply can submit a newer request for the slot of a later
    /// reply in the same batch; check [`Self::is_superseded`] before handling each one.
    pub fn poll(&mut self) -> Vec<(Slot, u64, Reply)> {
        let mut out = Vec::new();
        while let Ok((ticket, reply)) = self.replies.try_recv() {
            let Some(at) = self.pending.iter().position(|p| p.ticket == ticket) else {
                continue;
            };
            let p = self.pending.remove(at);
            // Unknown, or no definite outcome at all (no worker ran it): kept unresolved.
            let decided = matches!(&reply, Reply::Command(outcome) if !is_unknown(outcome));
            if let Some((operation, frame, dispatch)) = &p.command
                && !decided
            {
                self.unresolved.push(Unresolved {
                    slot: p.slot.clone(),
                    label: p.label.clone(),
                    operation,
                    frame: frame.clone(),
                    dispatch: *dispatch,
                });
            }
            // Superseded by a newer request for the same slot: drop the stale reply.
            if self.latest.get(&p.slot) == Some(&ticket) {
                self.latest.remove(&p.slot);
                out.push((p.slot, ticket, reply));
            }
        }
        out
    }

    /// Fence replies already in flight when a project is removed. Commands retain their
    /// exact original frames and receipt outcomes; later queries get fresh tickets.
    pub fn invalidate_project_queries(
        &mut self,
        _scope: &devworks_core::ports::storage::ProjectPurgeClientScope,
    ) {
        // Workspace lists can include the removed project without naming it. Re-read every
        // outstanding data query at a fresh snapshot, retaining command receipts unchanged.
        let queries = self
            .pending
            .iter()
            .filter(|p| self.latest.get(&p.slot) == Some(&p.ticket))
            .filter_map(|p| {
                p.query
                    .as_ref()
                    .filter(|q| !q.operation.starts_with("projects.purge_"))
                    .map(|q| {
                        (
                            p.slot.clone(),
                            p.label.clone(),
                            q.operation,
                            q.frame.clone(),
                            q.options.clone(),
                        )
                    })
            })
            .collect::<Vec<_>>();
        for (slot, label, op, frame, options) in queries {
            if let Some(options) = options {
                self.query_cancellable(slot, &label, op, frame, options);
            } else {
                self.query(slot, &label, op, frame);
            }
        }
    }

    pub fn has_local_tasks(&self) -> bool {
        self.pending
            .iter()
            .any(|p| p.command.is_none() && p.query.is_none())
    }

    /// Whether a request for `slot` newer than `ticket` was submitted after `ticket`'s reply
    /// was polled, so that reply is stale.
    pub fn is_superseded(&self, slot: &Slot, ticket: u64) -> bool {
        self.latest.get(slot).is_some_and(|newer| *newer > ticket)
    }

    /// Whether a request for `slot` is waiting for its reply.
    pub fn is_pending(&self, slot: &Slot) -> bool {
        self.latest.contains_key(slot)
    }

    /// Requests in flight, oldest first (status surface).
    pub fn activities(&self) -> Vec<Activity> {
        self.pending
            .iter()
            .filter(|p| self.latest.get(&p.slot) == Some(&p.ticket))
            .map(|p| Activity {
                label: p.label.clone(),
                elapsed: p.started.elapsed(),
            })
            .collect()
    }

    /// Blocks until every submitted request has a reply waiting in [`Self::poll`], or `bound`
    /// passes. Returns whether it became idle.
    pub fn wait_idle(&self, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        let mut n = self
            .shared
            .in_flight
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        while *n > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            n = self
                .shared
                .idle
                .wait_timeout(n, left)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        true
    }
}

/// Stops the workers and waits for them, so the backend (and with it the runtime and its store)
/// is released before the caller continues: a closed window leaves nothing running.
impl Drop for Client {
    fn drop(&mut self) {
        let (closed, _) = channel();
        drop(std::mem::replace(&mut self.jobs, closed));
        let (closed, _) = channel();
        drop(std::mem::replace(&mut self.network_jobs, closed));
        // Wait (bounded) for every worker to end; join only then, so no join can block.
        let deadline = Instant::now() + self.join_bound;
        let mut alive = self.shared.alive.lock().unwrap_or_else(|p| p.into_inner());
        while *alive > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            alive = self
                .shared
                .exited
                .wait_timeout(alive, left)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        let all_ended = *alive == 0;
        drop(alive);
        if all_ended {
            for w in self.workers.drain(..) {
                let _ = w.join();
            }
        } else {
            // A stuck worker is detached; it holds its backend until the process exits.
            self.workers.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every operation the core registry binds in command mode survives a relaunch, and a
    /// query-only or unknown operation does not. The shell cannot send a command that is
    /// missing from the registry, so no source scan is needed.
    #[test]
    fn every_registered_command_is_restorable() {
        let mut commands = 0;
        for d in devworks_core::registry::all() {
            let is_command = d
                .modes
                .iter()
                .any(|m| m.mode == devworks_core::registry::Mode::Command);
            let restored = Unresolved::restored(Slot::SettingsSave, "l".into(), d.name, Vec::new());
            assert_eq!(
                restored.is_some(),
                is_command,
                "{} restorable must match its command mode",
                d.name
            );
            commands += usize::from(is_command);
        }
        assert!(commands > 0);
        for op in [
            "roles.draft",
            "skills.draft",
            "roles.rollback",
            "skills.rollback",
            "agents.post",
            "agents.ack",
            "schemas.configure",
            "schemas.refresh",
            "schemas.remove_index",
        ] {
            assert!(
                Unresolved::restored(Slot::SettingsSave, "l".into(), op, Vec::new()).is_some(),
                "{op}"
            );
        }
        assert!(
            Unresolved::restored(
                Slot::SettingsSave,
                "l".into(),
                "no.such_operation",
                Vec::new()
            )
            .is_none()
        );
    }
}

#[cfg(test)]
mod human_dispatch_tests {
    use super::*;
    use devworks_core::gateway::GatewayFailure;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct NoHuman {
        generic: AtomicUsize,
        queries: AtomicUsize,
        frames: std::sync::Mutex<Vec<Vec<u8>>>,
        command_reply:
            std::sync::Mutex<Option<devworks_core::protocol::results::DomainCommandReply>>,
        query_reply: std::sync::Mutex<Option<devworks_core::protocol::results::DomainQueryReply>>,
    }
    impl Backend for NoHuman {
        fn query(&self, op: &str, _: &[u8]) -> QueryOutcome {
            self.queries.fetch_add(1, Ordering::SeqCst);
            self.query_reply
                .lock()
                .unwrap()
                .clone()
                .map(Ok)
                .unwrap_or_else(|| Err(GatewayFailure::NoUseCase(op.into())))
        }
        fn command(&self, op: &str, frame: &[u8]) -> CommandOutcome {
            self.generic.fetch_add(1, Ordering::SeqCst);
            self.frames.lock().unwrap().push(frame.to_vec());
            self.command_reply
                .lock()
                .unwrap()
                .clone()
                .map(Ok)
                .unwrap_or_else(|| Err(GatewayFailure::NoUseCase(op.into())))
        }
        fn mint_workspace_upload(&self, _: &Path) -> Result<Id, String> {
            Err("unavailable".into())
        }
        fn mint_project_upload(
            &self,
            _: &devworks_core::protocol::ProjectId,
            _: &Path,
        ) -> Result<Id, String> {
            Err("unavailable".into())
        }
        fn keep_awake_health(&self) -> Health {
            Health::Available
        }
        fn hints(&self) -> Option<watch::Receiver<u64>> {
            None
        }
        fn set_waker(&self, _: Waker) {}
        fn token(&self, prefix: &str) -> String {
            prefix.into()
        }
        fn scratch_dir(&self) -> PathBuf {
            PathBuf::new()
        }
        fn read_stream(&self, _: &Id, _: u64) -> Result<Vec<u8>, String> {
            Err("unavailable".into())
        }
    }
    #[test]
    fn slow_network_tasks_never_hold_the_worker_a_transcript_read_needs() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let started = Arc::new(AtomicUsize::new(0));
        let mut client = Client::new(Arc::new(NoHuman::default()));
        for slot in [Slot::AddonsDiscovery, Slot::McpRelease, Slot::McpCommand] {
            let (gate, started) = (gate.clone(), started.clone());
            assert!(client.network_task(slot, "Slow network call", move |_| {
                started.fetch_add(1, Ordering::SeqCst);
                let mut open = gate.0.lock().unwrap();
                while !*open {
                    open = gate.1.wait(open).unwrap();
                }
                Ok(TaskOutput::Refreshed)
            }));
        }
        assert!(client.query(
            Slot::Transcript("session-a".into()),
            "Reading the transcript",
            "sessions.transcript",
            b"frame".to_vec(),
        ));
        // The read is answered while every slow task is still waiting on its network call.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut answered = false;
        while !answered && Instant::now() < deadline {
            answered = client.poll().into_iter().any(|(slot, _, reply)| {
                slot == Slot::Transcript("session-a".into()) && matches!(reply, Reply::Query(_))
            });
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(answered, "the transcript read waited behind network tasks");
        assert!(started.load(Ordering::SeqCst) >= 1);
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        assert!(client.wait_idle(Duration::from_secs(10)));
        assert_eq!(started.load(Ordering::SeqCst), 3);
    }
    #[test]
    fn unsupported_human_ingress_never_falls_back_to_generic_commands() {
        let b = NoHuman::default();
        assert!(matches!(
            resolve_dispatched(
                &b,
                CommandDispatch::HumanDecision,
                "approvals.record_human_decision",
                b"exact"
            ),
            Err(GatewayFailure::NoUseCase(_))
        ));
        assert_eq!(b.generic.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn unsupported_purge_confirmation_never_falls_back_to_generic_commands() {
        let b = NoHuman::default();
        assert!(matches!(
            resolve_dispatched(
                &b,
                CommandDispatch::ProjectPurge,
                "projects.purge",
                b"exact"
            ),
            Err(GatewayFailure::NoUseCase(_))
        ));
        assert_eq!(b.generic.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn restored_purge_frames_preserve_the_original_frame_without_minting_human_consent() {
        let restored = Unresolved::restored_dispatch(
            Slot::ProjectCommand("project-a".into()),
            "purge".into(),
            "projects.purge",
            b"original exact frame".to_vec(),
            CommandDispatch::ProjectPurge,
        )
        .unwrap();
        assert_eq!(restored.dispatch, CommandDispatch::Generic);
        assert_eq!(restored.operation(), "projects.purge");
        assert_eq!(restored.frame(), b"original exact frame");
        assert!(
            Unresolved::restored_dispatch(
                Slot::SettingsSave,
                "wrong purpose".into(),
                "settings.save_revision",
                vec![],
                CommandDispatch::ProjectPurge
            )
            .is_none()
        );
        let backend = NoHuman::default();
        let _ = resolve_dispatched(
            &backend,
            restored.dispatch,
            restored.operation(),
            restored.frame(),
        );
        assert_eq!(backend.generic.load(Ordering::SeqCst), 1);
        assert_eq!(
            backend.frames.lock().unwrap().as_slice(),
            &[b"original exact frame".to_vec()]
        );
    }
    #[test]
    fn restart_keeps_the_explicit_dispatch_and_never_upgrades_old_generic_frames() {
        let legacy = Unresolved::restored(
            Slot::ApprovalDecision("p".into()),
            "old".into(),
            "approvals.record_human_decision",
            b"original".to_vec(),
        )
        .unwrap();
        assert_eq!(legacy.dispatch, CommandDispatch::Generic);
        let native = Unresolved::restored_dispatch(
            Slot::ApprovalDecision("p".into()),
            "native".into(),
            "approvals.record_human_decision",
            b"original".to_vec(),
            CommandDispatch::HumanDecision,
        )
        .unwrap();
        assert_eq!(native.dispatch, CommandDispatch::HumanDecision);
        assert_eq!(native.frame(), b"original");
        assert!(
            Unresolved::restored_dispatch(
                Slot::SettingsSave,
                "invalid".into(),
                "settings.save_revision",
                vec![],
                CommandDispatch::HumanDecision
            )
            .is_none()
        );
        let stored = serde_json::json!({"slot":"SettingsSave","label":"old","operation":"settings.save_revision","frame":[1,2]});
        let old: crate::memory::PendingCommand = serde_json::from_value(stored).unwrap();
        assert_eq!(old.dispatch, CommandDispatch::Generic);
    }
    fn purged_backend() -> (NoHuman, Vec<u8>) {
        let b = NoHuman::default();
        let frame=serde_json::json!({"protocol":"1.0-draft.1","mode":"Command","requestKey":"original-key","body":{"kind":"projects.create","name":"private original name","description":"","repositoryIds":[]}}).to_string().into_bytes();
        let decoded = devworks_core::admission::decode(
            devworks_core::admission::Selection {
                operation: "projects.create",
                mode: devworks_core::registry::Mode::Command,
            },
            &frame,
        )
        .unwrap();
        *b.command_reply.lock().unwrap()=Some(serde_json::from_value(serde_json::json!({"kind":"Rejected","protocol":"1.0-draft.1","requestKey":"original-key","code":"UnsupportedSchema","retry":"ResolveReceipt","correlationId":"correlation-a"})).unwrap());
        *b.query_reply.lock().unwrap()=Some(serde_json::from_value(serde_json::json!({"kind":"Result","protocol":"1.0-draft.1","queryId":"purged-receipt","stamp":{"storeId":"store-a","restoreEpoch":"0","sequence":"9","policyRevision":"1","scopeRevision":"1"},"result":{"kind":"history.command_receipt","value":{"requestKey":"original-key","commandId":"committed-command","committedSequence":"8","state":"Recorded","purged":{"commandId":"committed-command","committedSequence":"8","acceptedAtMs":"100","requestDigest":decoded.fingerprint.as_str(),"originalDecision":"Applied","purgeId":"purge-a"}}}})).unwrap());
        (b, frame)
    }
    #[test]
    fn purged_legacy_command_resolves_once_without_resubmission_or_new_identity() {
        let (b, frame) = purged_backend();
        assert!(
            matches!(resolve_command(&b,"projects.create",&frame),Ok(CommandReply::ReceiptPurged{request_key,outcome,..}) if request_key.0.as_str()=="original-key" && outcome.command_id.0.as_str()=="committed-command")
        );
        assert_eq!(b.generic.load(Ordering::SeqCst), 1);
        assert_eq!(b.queries.load(Ordering::SeqCst), 1);
        assert_eq!(*b.frames.lock().unwrap(), [frame]);
    }
    #[test]
    fn purged_lookup_for_another_key_or_intent_cannot_replace_the_original_outcome() {
        for another_key in [false, true] {
            let (b, frame) = purged_backend();
            if let Some(devworks_core::protocol::results::DomainQueryReply::Result {
                result:
                    devworks_core::protocol::results::OperationResult::HistoryCommandReceipt { value },
                ..
            }) = b.query_reply.lock().unwrap().as_mut()
            {
                if another_key {
                    value.request_key =
                        devworks_core::protocol::RequestKey(Id::new("another-key").unwrap());
                } else {
                    value.purged.as_mut().unwrap().request_digest =
                        devworks_core::protocol::provider::Digest::try_from("b".repeat(64))
                            .unwrap();
                }
            }
            assert!(matches!(
                resolve_command(&b, "projects.create", &frame),
                Ok(CommandReply::Rejected {
                    code: devworks_core::protocol::Rejection::UnsupportedSchema,
                    ..
                })
            ));
            assert_eq!(b.generic.load(Ordering::SeqCst), 1);
            assert_eq!(b.queries.load(Ordering::SeqCst), 1);
            assert_eq!(*b.frames.lock().unwrap(), [frame]);
        }
    }
}
