//! Replaceable boundaries used by the gateway (ARCH-08). Production adapters implement them
//! (SQLite storage in `devworks-adapters`; host authority is [`crate::authority::HostAuthority`]
//! over the storage ports); tests use small deterministic substitutes.
//!
//! Every port is `Send + Sync` and object-safe: ARCH-03 runs gateway calls off the UI thread,
//! and command use cases execute on the storage write owner's thread, so the gateway hands them
//! across threads. Core stays runtime-agnostic (no async runtime dependency).
pub mod browser;
pub mod diagnostics;
pub mod files;
pub mod http;
pub mod indexing;
pub mod os_notifications;
pub mod packages;
pub mod process;
pub mod provider_http;
pub mod retrieval;
pub mod router;
pub mod secrets;
pub mod storage;
pub mod usage;
pub mod web;

use crate::context::InvocationContext;
use crate::fingerprint::Fingerprint;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::OperationInput;
use crate::protocol::results::{DomainCommandReply, DomainDecision, OperationResult};
use crate::protocol::{
    ActorId, Counter, DomainFault, Id, OperationId, ProjectId, ReadStamp, Receipt,
};
use crate::registry::{EffectClass, ModeBinding, OperationDescriptor, SchemaRevision};
use std::any::Any;
use std::fmt;
use std::sync::Arc;
use storage::{ApplicationStore, Cancellation, ReadSnapshot, StorageError, WriteTransaction};

/// UTC wall clock in milliseconds.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> Counter;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IdKind {
    Command,
    Correlation,
    Operation,
    Event,
    Attempt,
    QueueEntry,
    Store,
    Boot,
    Grant,
    Artifact,
    ArtifactVersion,
    /// Host-issued artifact source handles (upload tickets).
    SourceHandle,
    Session,
    Conversation,
    TranscriptEntry,
    TranscriptBatch,
    Link,
    Resource,
    Lease,
    Process,
    Diagnostic,
    /// Owned browser sessions (DW-047).
    BrowserSession,
    /// Router session pins.
    RoutePin,
    Project,
    /// Exact-version package approval proposals.
    Proposal,
    /// Package process sessions (the protocol nonce of one helper instance).
    PackageSession,
    /// Package invocations and their short-lived tokens.
    Invocation,
    /// Repository registrations (discovery).
    Repository,
    /// Index graph generations.
    Generation,
    /// Immutable index views.
    View,
    /// Worktree overlays.
    Overlay,
    /// Knowledge entities, claims, evidence, relations, challenges and writes.
    KnowledgeEntity,
    KnowledgeClaim,
    KnowledgeEvidence,
    KnowledgeRelation,
    KnowledgeChallenge,
    KnowledgeWrite,
    /// Schedules (jobs) and their runs.
    Job,
    JobRun,
    /// Provider connections.
    Connection,
    /// Secret-store credential handles (immutable; a replacement gets a new one).
    Credential,
    /// In-process sign-in flows.
    SignInFlow,
}
/// Opaque identifier allocation. IDs carry no order, authority or path meaning.
pub trait IdSource: Send + Sync {
    fn next_id(&self, kind: IdKind) -> Id;
}

/// What the authority port is asked to establish before any receipt access.
pub struct AuthorityRequest<'a> {
    pub descriptor: &'static OperationDescriptor,
    pub binding: &'static ModeBinding,
    /// The effective effect class: for `ActionDependent` bindings, the class of the action the
    /// typed input selected, classified by core before authority (AD69).
    pub effect: EffectClass,
    /// The effective action label for action-dependent bindings.
    pub action: Option<&'static str>,
    pub input: &'a OperationInput,
    /// Digest of the normalized selected request: an exact-request grant binds to it, so
    /// changed arguments can never reuse a narrowly approved target.
    pub fingerprint: &'a Fingerprint,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthorityDenial {
    PermissionDenied,
    /// The addressed target is not valid for this operation (for example a child assignment
    /// given to a root-only control).
    InvalidTarget,
    /// The target is not visible to this principal; existence is not disclosed.
    NotFound,
    /// Refused by host authority policy. The wire code is `PermissionDenied`; the typed reason
    /// is carried in the query fault and recorded, attributed, for a denied command.
    Policy(crate::authority::DenialReason),
}

/// One use of a host grant (for example a user-minted resume authorization) that authority
/// resolved at `expected_revision`. The command transaction consumes it atomically with the
/// receipt; a grant that changed or was used up in between denies the command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantUse {
    pub grant_id: Id,
    pub expected_revision: i64,
}

/// A host authority fact the authorization depended on. The command unit of work re-checks every
/// fence inside its transaction before the receipt lookup, and a query re-checks them on a fresh
/// snapshot before publishing its result, so a change committed after authorization (a revoked
/// or re-bound principal, a narrowed access policy) fences the request.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AuthorityFence {
    /// Current durable task-branch ownership and repository membership.
    GitWorktree { id: Id, digest: String },
    /// The principal binding revision authority resolved.
    Principal { actor: ActorId, revision: i64 },
    /// Digest of the access-relevant effective policy of the invocation's scope.
    Policy {
        project_id: Option<ProjectId>,
        digest: String,
    },
}

/// What current authority established for one request. For a command it is consumed by the
/// write unit of work: listed grant uses are spent together with the receipt and every fence
/// must still hold.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Authorization {
    grants: Vec<GrantUse>,
    authority_grants: Vec<GrantUse>,
    fences: Vec<AuthorityFence>,
    replay_only: bool,
}
impl Authorization {
    /// Authority held directly by the principal's current role/scope; nothing to consume.
    pub fn direct() -> Self {
        Self::default()
    }
    /// Authority that depends on spending one use of each listed resume grant.
    pub fn with_grants(grants: Vec<GrantUse>) -> Self {
        Self {
            grants,
            ..Self::default()
        }
    }
    /// Adds uses of host-minted authority grants ([`storage::AuthorityLedger`]).
    pub fn spending(mut self, grants: Vec<GrantUse>) -> Self {
        self.authority_grants.extend(grants);
        self
    }
    /// Adds facts that must still hold when the request commits or publishes.
    pub fn fenced_by(mut self, fences: Vec<AuthorityFence>) -> Self {
        self.fences.extend(fences);
        self
    }
    /// Authority sufficient only to replay an existing receipt (for example the grant that
    /// authorized it has since been spent or has expired): a new decision is denied.
    pub fn replay_only(mut self) -> Self {
        self.replay_only = true;
        self
    }
    pub fn is_replay_only(&self) -> bool {
        self.replay_only
    }
    pub fn grants(&self) -> &[GrantUse] {
        &self.grants
    }
    pub fn authority_grants(&self) -> &[GrantUse] {
        &self.authority_grants
    }
    pub fn fences(&self) -> &[AuthorityFence] {
        &self.fences
    }
}

/// Current effective authority: role/scope/tool selection, grants, revocation. Resolved from the
/// host context and host records, never from request fields.
pub trait AuthorityPort: Send + Sync {
    fn authorize(
        &self,
        ctx: &InvocationContext,
        request: &AuthorityRequest<'_>,
    ) -> Result<Authorization, AuthorityDenial>;
}

/// A durable command receipt as stored with its exact contract revision and digest.
#[derive(Clone, Debug)]
pub struct StoredReceipt {
    pub operation: String,
    pub contract: String,
    pub schema_revision: SchemaRevision,
    pub fingerprint: Fingerprint,
    pub receipt: Receipt<DomainDecision>,
}

/// A bounded, scoped read result with its freshness stamp.
#[derive(Clone, Debug)]
pub struct QueryAnswer {
    pub stamp: ReadStamp,
    pub result: OperationResult,
}
/// Read use cases run against one read snapshot. Queries never mint grants, clear blockers,
/// claim work or consume budget.
pub trait QueryHandler: Send + Sync {
    /// Bounded external preparation of an admitted, currently authorized query, outside any read
    /// snapshot and write transaction: for example a host-native external read (the web tools)
    /// and the evidence it records. It never mints a receipt, grant or blocker change; what it
    /// writes (journaled staging, evidence artifacts, host diagnostics) must be complete and
    /// recoverable on its own, because the query's publication can still be denied. `env`
    /// carries the caller's cancellation and deadline. An `Err` is the query's typed fault. The
    /// default prepares nothing.
    fn prepare(
        &self,
        _ctx: &InvocationContext,
        _query: &AdmittedQuery,
        _env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, DomainFault> {
        Ok(None)
    }
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault>;
}

/// The use case's decision for a new command (expected revisions, root controls, eligibility and
/// target identity are evaluated by the use case against current state in the transaction).
#[derive(Clone, Debug)]
pub struct Decided {
    pub decision: DomainDecision,
    /// Present only when asynchronous/effectful work exists.
    pub operation_id: Option<OperationId>,
}
#[derive(Clone, Debug)]
pub enum HandlerFailure {
    /// A storage failure. A [`StorageError::RevisionConflict`] undoes the use case's writes and is
    /// recorded as a durable `RevisionConflict` refusal; any other error rolls back the whole
    /// command with no receipt.
    Storage(StorageError),
}
impl From<StorageError> for HandlerFailure {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}
/// What a use case staged before its unit of work (see [`CommandHandler::prepare`]). Opaque to
/// the gateway; the use case reads it back with [`crate::gateway::AdmittedCommand::prepared`].
#[derive(Clone)]
pub struct Prepared(Arc<dyn Any + Send + Sync>);
impl Prepared {
    pub fn new<T: Any + Send + Sync>(value: T) -> Self {
        Self(Arc::new(value))
    }
    pub fn get<T: Any>(&self) -> Option<&T> {
        self.0.downcast_ref()
    }
}
impl fmt::Debug for Prepared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Prepared(..)")
    }
}

/// What a preparation (and settlement) step may use: read snapshots, small writes limited to the
/// operation journal and the artifact records ([`StagingTx`]), and the command's cancellation
/// signal and deadline. It cannot record receipts, append history or touch authority.
pub struct Preparation<'a> {
    store: &'a dyn ApplicationStore,
    pub cancellation: Option<&'a Cancellation>,
    /// Store-clock deadline (UTC ms) of the command: past it, preparation stops.
    pub deadline_ms: Option<i64>,
}
impl<'a> Preparation<'a> {
    pub fn new(
        store: &'a dyn ApplicationStore,
        cancellation: Option<&'a Cancellation>,
        deadline_ms: Option<i64>,
    ) -> Self {
        Self {
            store,
            cancellation,
            deadline_ms,
        }
    }
    /// Whether the command was cancelled or its deadline passed at `now_ms`.
    pub fn stopped(&self, now_ms: i64) -> bool {
        self.cancellation.is_some_and(Cancellation::is_cancelled)
            || self.deadline_ms.is_some_and(|d| now_ms > d)
    }
    /// The store's identity (boot epoch, host actor).
    pub fn info(&self) -> storage::StoreInfo {
        self.store.info()
    }
    /// Runs `work` against one read snapshot.
    pub fn read<T>(
        &self,
        work: impl FnOnce(&dyn ReadSnapshot) -> Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        storage::read_snapshot(self.store, &storage::ReadOptions::default(), work)
    }
    /// Runs `work` in its own write transaction with only the journal and artifact records.
    pub fn write<T, F>(&self, work: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&mut StagingTx<'_>) -> Result<T, StorageError> + Send + 'static,
    {
        storage::run_write(self.store, storage::WriteOptions::default(), move |tx| {
            work(&mut StagingTx { tx })
        })
    }
}

/// The repositories a preparation write may use.
pub struct StagingTx<'t> {
    tx: &'t mut dyn WriteTransaction,
}
impl StagingTx<'_> {
    pub fn now_ms(&self) -> i64 {
        self.tx.now_ms()
    }
    pub fn new_id(&mut self, kind: IdKind) -> Id {
        self.tx.new_id(kind)
    }
    pub fn store(&self) -> &storage::StoreInfo {
        self.tx.store()
    }
    pub fn operations(&mut self) -> &mut dyn storage::OperationJournal {
        self.tx.operations()
    }
    pub fn artifacts(&mut self) -> &mut dyn storage::ArtifactRepository {
        self.tx.artifacts()
    }
}

/// Command use cases. `decide` runs synchronously on the sole write owner inside the command's
/// transaction, after the receipt check and before the receipt is recorded: its reads are the
/// current authoritative state and its writes (domain changes, history, outbox intents) commit
/// atomically with the receipt or not at all. It must not await, call the network, run
/// inference or invoke package/UI callbacks.
pub trait CommandHandler: Send + Sync {
    /// Bounded preparation outside any write transaction, after admission and current authority
    /// and before the unit of work: for example staging and flushing a file whose metadata
    /// `decide` then publishes (ARCH-03: preparation and observation happen before or after the
    /// transactional handler). It must leave only recoverable, invisible state (journaled intent,
    /// staged or unreferenced files), because the unit of work may still deny, replay or refuse
    /// the command. An `Err` rejects the command without a receipt. The default prepares nothing.
    fn prepare(
        &self,
        _ctx: &InvocationContext,
        _command: &AdmittedCommand,
        _env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        Ok(None)
    }
    /// Runs once the command's outcome is known, outside any transaction, when `prepare`
    /// returned something: `reply` is the reply about to be returned (`None` when the use case
    /// broke its contract and nothing was recorded). It resolves preparation the unit of work did
    /// not commit (a refusal, replay, denial or rollback); it must be idempotent and fenced,
    /// because crash recovery can race it, and it cannot change the reply. The default does
    /// nothing.
    fn settle(
        &self,
        _ctx: &InvocationContext,
        _prepared: Option<&Prepared>,
        _reply: Option<&DomainCommandReply>,
        _env: &Preparation<'_>,
    ) {
    }
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure>;
}
