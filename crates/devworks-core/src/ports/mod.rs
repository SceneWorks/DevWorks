//! Replaceable boundaries used by the gateway (ARCH-08). Production adapters implement them
//! (SQLite storage in `devworks-adapters`; host authority is [`crate::authority::HostAuthority`]
//! over the storage ports); tests use small deterministic substitutes.
//!
//! Every port is `Send + Sync` and object-safe: ARCH-03 runs gateway calls off the UI thread,
//! and command use cases execute on the storage write owner's thread, so the gateway hands them
//! across threads. Core stays runtime-agnostic (no async runtime dependency).
pub mod storage;

use crate::context::InvocationContext;
use crate::fingerprint::Fingerprint;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::OperationInput;
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::{
    ActorId, Counter, DomainFault, Id, OperationId, ProjectId, ReadStamp, Receipt,
};
use crate::registry::{EffectClass, ModeBinding, OperationDescriptor, SchemaRevision};
use storage::{ReadSnapshot, StorageError, WriteTransaction};

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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthorityFence {
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
/// Command use cases. `decide` runs synchronously on the sole write owner inside the command's
/// transaction, after the receipt check and before the receipt is recorded: its reads are the
/// current authoritative state and its writes (domain changes, history, outbox intents) commit
/// atomically with the receipt or not at all. It must not await, call the network, run
/// inference or invoke package/UI callbacks.
pub trait CommandHandler: Send + Sync {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure>;
}
