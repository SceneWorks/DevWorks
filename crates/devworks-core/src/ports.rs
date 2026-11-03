//! Replaceable boundaries used by the gateway (ARCH-08). Production adapters implement them in
//! later items (SQLite receipts in the store item, host authority in the authority item); tests
//! use small deterministic substitutes. Ports are synchronous and object-safe: core is
//! runtime-agnostic and the engine runs gateway calls off the UI thread (ARCH-03).
use crate::context::{InvocationContext, ReceiptIdentity};
use crate::fingerprint::Fingerprint;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::OperationInput;
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::{
    CommandId, Counter, DomainFault, Id, OperationId, ReadStamp, Receipt, Revision,
};
use crate::registry::{ModeBinding, OperationDescriptor, SchemaRevision};

/// UTC wall clock in milliseconds.
pub trait Clock {
    fn now_ms(&self) -> Counter;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IdKind {
    Command,
    Correlation,
    Operation,
    Event,
}
/// Opaque identifier allocation. IDs carry no order, authority or path meaning.
pub trait IdSource {
    fn next_id(&self, kind: IdKind) -> Id;
}

/// What the authority port is asked to establish before any receipt access.
pub struct AuthorityRequest<'a> {
    pub descriptor: &'static OperationDescriptor,
    pub binding: &'static ModeBinding,
    pub input: &'a OperationInput,
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
/// Current effective authority: role/scope/tool selection, grants, revocation. Resolved from the
/// host context and host records, never from request fields.
pub trait AuthorityPort {
    fn authorize(
        &self,
        ctx: &InvocationContext,
        request: &AuthorityRequest<'_>,
    ) -> Result<(), AuthorityDenial>;
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
/// A decided command ready to be recorded.
#[derive(Clone, Debug)]
pub struct NewReceipt {
    pub operation: &'static str,
    pub contract: &'static str,
    pub schema_revision: SchemaRevision,
    pub fingerprint: Fingerprint,
    pub command_id: CommandId,
    pub accepted_at_ms: Counter,
    pub operation_id: Option<OperationId>,
    pub decision: DomainDecision,
}
#[derive(Clone, Debug)]
pub enum CommitOutcome {
    Committed {
        committed_sequence: Revision,
    },
    /// A concurrent request with the same identity committed first; nothing new was written.
    AlreadyRecorded(StoredReceipt),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptStoreError {
    /// Nothing was written; the same command may be retried.
    Unavailable,
    /// The commit may have happened but its acknowledgment was lost.
    AcknowledgmentLost,
}
/// Receipt lookup and recording on the sole write owner.
///
/// `commit` is the write boundary: the store implementation commits the receipt together with
/// the use case's domain changes, `CommandDecided` history, consumed grants and outbox intents in
/// one transaction, and enforces `(actorId, scopeKey, requestKey)` uniqueness so a race resolves
/// to [`CommitOutcome::AlreadyRecorded`].
pub trait ReceiptStore {
    fn lookup(
        &self,
        identity: &ReceiptIdentity,
    ) -> Result<Option<StoredReceipt>, ReceiptStoreError>;
    fn commit(
        &self,
        identity: &ReceiptIdentity,
        receipt: NewReceipt,
    ) -> Result<CommitOutcome, ReceiptStoreError>;
}

/// A bounded, scoped read result with its freshness stamp.
#[derive(Clone, Debug)]
pub struct QueryAnswer {
    pub stamp: ReadStamp,
    pub result: OperationResult,
}
/// Read use cases. Queries never mint grants, clear blockers, claim work or consume budget.
pub trait QueryHandler {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
    ) -> Result<QueryAnswer, DomainFault>;
}

/// The use case's decision for a new command (expected revisions, root controls, eligibility and
/// target identity are evaluated by the use case against current state).
#[derive(Clone, Debug)]
pub struct Decided {
    pub decision: DomainDecision,
    /// Present only when asynchronous/effectful work exists.
    pub operation_id: Option<OperationId>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandlerFailure {
    /// Transient storage admission failure before any write.
    StorageUnavailable,
}
pub trait CommandHandler {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
    ) -> Result<Decided, HandlerFailure>;
}
