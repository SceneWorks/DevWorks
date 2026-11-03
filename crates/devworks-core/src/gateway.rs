//! `ApplicationGateway`: the single entry point for UI actions, agent tools, schedules, package
//! host calls and MCP requests (ARCH-02). There is no second admission path: a typed caller
//! serializes its request and submits it here like any other adapter.
//!
//! Command order (shared-operation-contracts.md "Command admission and receipts"):
//! 1. pure admission ([`crate::admission`]) including effective-action classification;
//! 2. current authority, which returns the [`Authorization`] the transaction will consume;
//! 3. receipt identity from the host context;
//! 4. **one write unit of work on the sole write owner** (`BEGIN IMMEDIATE`): store/restore-epoch
//!    and principal check → receipt lookup (equal fingerprint replays, a different fingerprint is
//!    `IdempotencyConflict`, an incompatible stored record is a visible hold) → the use case's
//!    current-state reads/CAS and decision against transaction-scoped repositories → grant
//!    consumption → `CommandDecided` history → receipt → the receipt's reply publication intent
//!    ([`crate::services::artifacts::replies`]). Domain changes, outbox intents, consumed
//!    grants, history and receipt commit together or not at all;
//! 5. `Recorded` only after the commit is confirmed; a lost acknowledgment is
//!    `CommandStatusUnknown`, resolved by resubmitting the same command/key (receipt lookup).
//!
//! **Schema revisions.** Admission resolves the negotiated revision before authority; an
//! unregistered one is refused there (`UnsupportedSchema`) and never reaches the store. A result
//! is published and a receipt recorded under that revision's projection, and the receipt stores
//! its revision: a replay returns it under the same projection. The same key under another
//! registered revision is a different intent (`IdempotencyConflict`; the fingerprint binds the
//! revision), while a receipt under a revision the operation no longer registers is an
//! incompatible record (visible hold, never replayed).
//!
//! Malformed, unsupported and denied requests stop before the store, so they never mint durable
//! work or a receipt record. A command refused by authority policy records one attributed
//! `AuthorityDenied` history event (keyed by the rejection's correlation ID) so its typed reason
//! is visible; a refused query carries the reason in its fault (`authority:<DenialReason>`). Because lookup and decision run in one serialized transaction, a
//! duplicate that loses a race finds the winner's receipt and its handler never runs.
use crate::admission::{self, AdmissionError, CallerKey, Refusal, Selection};
use crate::authority::AuthorityCapability;
use crate::context::{InvocationContext, PrincipalKind, ReceiptIdentity};
use crate::fingerprint::{self, Fingerprint};
use crate::operations::OperationInput;
use crate::ports::storage::{
    ActorRegistry, ArtifactRepository, AuthorityGrant, AuthorityLedger, AuthorityRead,
    CommandLedger, CommandLedgerRead, ConfigurationRepository, DurableQueue, EventFilter,
    EventPage, EventRecord, GrantLedger, GrantRead, GrantRecord, HistoryLog, HistoryRead,
    LedgerEntry, NewAuthorityGrant, NewEvent, NewGrant, NewPrincipalBinding, OperationJournal,
    PackageHandlerLedger, PrincipalBinding, ReadOptions, ReceiptReply, RegisteredRepository,
    RevisionConflict, Savepoint, SessionRepository, StorageError, StoreInfo, WriteOptions,
    WriteTransaction, read_snapshot, run_write,
};
use crate::ports::{
    AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization, CommandHandler, Decided,
    GrantUse, HandlerFailure, IdKind, IdSource, Preparation, Prepared, QueryHandler, StoredReceipt,
};
use crate::protocol::results::{
    DomainCommandReply, DomainDecision, DomainQueryReply, validate_command_reply,
};
use crate::protocol::support::Revisioned;
use crate::protocol::support::{BoundedList, BoundedText};
use crate::protocol::{
    ActorId, CONTRACT, Check, CommandId, CommandReply, CorrelationId, DomainErrorCode, DomainFault,
    EventId, Id, Protocol, QueryId, Receipt, Rejection, RequestKey, RetryAdvice, Revision,
};
use crate::registry::{EffectClass, Mode, ModeBinding, OperationDescriptor, SchemaRevision};
use serde_json::json;
use std::cell::RefCell;
use std::fmt;
use std::sync::Arc;

/// A query that passed admission and authority. Only the gateway can construct one, so a use
/// case cannot be reached with an unvalidated or unauthorized input.
#[derive(Debug)]
pub struct AdmittedQuery {
    descriptor: &'static OperationDescriptor,
    binding: &'static ModeBinding,
    query_id: QueryId,
    input: OperationInput,
    fingerprint: Fingerprint,
    revision: SchemaRevision,
}
impl AdmittedQuery {
    pub fn descriptor(&self) -> &'static OperationDescriptor {
        self.descriptor
    }
    pub fn binding(&self) -> &'static ModeBinding {
        self.binding
    }
    pub fn query_id(&self) -> &QueryId {
        &self.query_id
    }
    pub fn input(&self) -> &OperationInput {
        &self.input
    }
    /// Digest of the normalized query, usable to bind cursors to the exact query.
    pub fn fingerprint(&self) -> &Fingerprint {
        &self.fingerprint
    }
    /// The schema revision the query negotiated; the gateway projects the result onto it.
    pub fn schema_revision(&self) -> SchemaRevision {
        self.revision
    }
}

/// A new command that passed admission, authority and the in-transaction receipt lookup.
#[derive(Debug)]
pub struct AdmittedCommand {
    descriptor: &'static OperationDescriptor,
    binding: &'static ModeBinding,
    identity: ReceiptIdentity,
    input: OperationInput,
    fingerprint: Fingerprint,
    revision: SchemaRevision,
    effect: EffectClass,
    action: Option<&'static str>,
    prepared: Option<Prepared>,
    targets: Vec<Id>,
    project_targets: Vec<crate::protocol::ProjectId>,
}
impl AdmittedCommand {
    pub fn descriptor(&self) -> &'static OperationDescriptor {
        self.descriptor
    }
    pub fn binding(&self) -> &'static ModeBinding {
        self.binding
    }
    pub fn identity(&self) -> &ReceiptIdentity {
        &self.identity
    }
    pub fn input(&self) -> &OperationInput {
        &self.input
    }
    pub fn fingerprint(&self) -> &Fingerprint {
        &self.fingerprint
    }
    /// The schema revision the command negotiated; its receipt is recorded under it.
    pub fn schema_revision(&self) -> SchemaRevision {
        self.revision
    }
    /// Effective effect class (action-dependent bindings already classified).
    pub fn effect(&self) -> EffectClass {
        self.effect
    }
    pub fn action(&self) -> Option<&'static str> {
        self.action
    }
    /// What the use case's [`CommandHandler::prepare`] staged for this command, if anything.
    pub fn prepared<T: std::any::Any>(&self) -> Option<&T> {
        self.prepared.as_ref().and_then(Prepared::get)
    }
}

/// Outcomes that cannot be expressed as an addressed wire reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GatewayFailure {
    /// The frame carried no valid caller identity to address a reply to.
    Unaddressable {
        error: AdmissionError,
        correlation_id: CorrelationId,
    },
    /// A use case returned a result/decision/stamp that does not match the admitted operation's
    /// contract or the invocation; nothing was recorded or published.
    HandlerContract(String),
    /// No use case is registered for the selected operation; nothing was admitted or recorded.
    NoUseCase(String),
}
impl fmt::Display for GatewayFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unaddressable { error, .. } => write!(f, "unaddressable request: {error}"),
            Self::HandlerContract(e) => write!(f, "use case contract violation: {e}"),
            Self::NoUseCase(op) => write!(f, "no use case is registered for {op}"),
        }
    }
}

pub type QueryOutcome = Result<DomainQueryReply, GatewayFailure>;
pub type CommandOutcome = Result<DomainCommandReply, GatewayFailure>;

/// What the command unit of work established on the write owner.
enum Unit {
    Recorded(Receipt<DomainDecision>),
    Replayed(Receipt<DomainDecision>),
    IdempotencyConflict,
    /// A stored receipt under a different contract/schema revision: visible incompatible hold.
    IncompatibleRecord,
    /// The invocation's store/epoch, principal or consumed grant is no longer current.
    Denied,
    Contract(String),
}

/// The application gateway over its host ports.
pub struct Gateway<'p> {
    pub authority: &'p dyn AuthorityPort,
    pub store: &'p dyn crate::ports::storage::ApplicationStore,
    pub ids: &'p dyn IdSource,
}

impl Gateway<'_> {
    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }

    /// Admits and answers a Query for the host-selected `operation` against one read snapshot.
    pub fn query(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
        handler: &dyn QueryHandler,
    ) -> QueryOutcome {
        let decoded = match admission::decode(
            Selection {
                operation,
                mode: Mode::Query,
            },
            raw,
        ) {
            Ok(d) => d,
            Err(refusal) => {
                return self.address(refusal, |s, key, error| s.query_rejected(key, error));
            }
        };
        let CallerKey::Query(query_id) = decoded.key else {
            return Err(GatewayFailure::HandlerContract(
                "query admitted with a command key".into(),
            ));
        };
        let request = AuthorityRequest {
            descriptor: decoded.descriptor,
            binding: decoded.binding,
            effect: decoded.effect,
            action: decoded.action,
            input: &decoded.input,
            fingerprint: &decoded.fingerprint,
        };
        let authorization = match self.authority.authorize(ctx, &request) {
            Ok(a) => a,
            Err(denial) => {
                let code = match denial {
                    AuthorityDenial::PermissionDenied | AuthorityDenial::Policy(_) => {
                        DomainErrorCode::PermissionDenied
                    }
                    AuthorityDenial::InvalidTarget => DomainErrorCode::InvalidArgument,
                    AuthorityDenial::NotFound => DomainErrorCode::NotFound,
                };
                let mut reply = self.query_fault(query_id, code, RetryAdvice::Never);
                if let (AuthorityDenial::Policy(reason), DomainQueryReply::Rejected { fault, .. }) =
                    (denial, &mut reply)
                {
                    // The typed reason travels in the fault: `authority:<DenialReason>`.
                    fault.target_kind = Some(BoundedText(denial_target(reason)));
                }
                return Ok(reply);
            }
        };
        let admitted = AdmittedQuery {
            descriptor: decoded.descriptor,
            binding: decoded.binding,
            query_id,
            input: decoded.input,
            fingerprint: decoded.fingerprint,
            revision: decoded.revision,
        };
        let answered = read_snapshot(self.store, &ReadOptions::default(), |snapshot| {
            let stamp = snapshot.stamp().clone();
            if &stamp.store_id != ctx.store_id() || stamp.restore_epoch != ctx.restore_epoch().get()
            {
                return Ok(None);
            }
            Ok(Some((handler.query(ctx, &admitted, snapshot), stamp)))
        });
        // Publication point: authority that was revoked or narrowed while the answer was being
        // prepared denies the output. Checked on a fresh snapshot, after the answer exists.
        if matches!(answered, Ok(Some((Ok(_), _))))
            && (!authorization.fences().is_empty() || !authorization.authority_grants().is_empty())
        {
            let holds = read_snapshot(self.store, &ReadOptions::default(), |s| {
                crate::authority::authorization_holds(s, &authorization)
            });
            if !matches!(holds, Ok(true)) {
                return Ok(self.query_fault(
                    admitted.query_id,
                    DomainErrorCode::PermissionDenied,
                    RetryAdvice::Never,
                ));
            }
        }
        let reply = match answered {
            Err(e) => {
                let (code, retry) = storage_fault(&e);
                self.query_fault(admitted.query_id, code, retry)
            }
            // The invocation is bound to another store or an earlier restore epoch.
            Ok(None) => self.query_fault(
                admitted.query_id,
                DomainErrorCode::PermissionDenied,
                RetryAdvice::Never,
            ),
            Ok(Some((Ok(answer), snapshot))) => {
                if answer.result.operation() != admitted.descriptor.name {
                    return Err(GatewayFailure::HandlerContract(format!(
                        "{} result published for {}",
                        answer.result.operation(),
                        admitted.descriptor.name
                    )));
                }
                let stamp = &answer.stamp;
                if &stamp.store_id != ctx.store_id()
                    || stamp.restore_epoch != *ctx.restore_epoch()
                    || stamp.sequence.get() > snapshot.committed_sequence
                {
                    return Err(GatewayFailure::HandlerContract(format!(
                        "read stamp {}/{}@{} does not match the invocation snapshot {}/{}@{}",
                        stamp.store_id.0.as_str(),
                        stamp.restore_epoch.as_str(),
                        stamp.sequence.as_str(),
                        snapshot.store_id.0.as_str(),
                        snapshot.restore_epoch,
                        snapshot.committed_sequence
                    )));
                }
                // Published under the negotiated revision's projection only.
                let mut result = answer.result;
                result.retain_revision(admitted.revision.0);
                // A union alternative introduced later (an event's command aggregate) cannot be
                // projected away: a result carrying one below its revision is a handler fault.
                if let crate::protocol::results::OperationResult::HistoryEvents { value } = &result
                    && let Some(Err(e)) = value
                        .items
                        .0
                        .iter()
                        .map(|e| e.validate_revision(admitted.revision.0))
                        .find(Result::is_err)
                {
                    return Err(GatewayFailure::HandlerContract(format!(
                        "history.events result at schema revision {}: {e}",
                        admitted.revision.0
                    )));
                }
                DomainQueryReply::Result {
                    protocol: Protocol::Draft1,
                    query_id: admitted.query_id,
                    stamp: answer.stamp,
                    result,
                }
            }
            Ok(Some((Err(fault), _))) => DomainQueryReply::Rejected {
                protocol: Protocol::Draft1,
                query_id: admitted.query_id,
                fault,
            },
        };
        reply.validate().map_err(GatewayFailure::HandlerContract)?;
        Ok(reply)
    }

    /// Admits, deduplicates, decides and records a Command for the host-selected `operation`.
    pub fn command(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
        handler: Arc<dyn CommandHandler>,
    ) -> CommandOutcome {
        self.command_with(ctx, operation, raw, handler, WriteOptions::default())
    }

    /// [`Gateway::command`] with explicit write admission options (cancellation, deadline).
    pub fn command_with(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
        handler: Arc<dyn CommandHandler>,
        options: WriteOptions,
    ) -> CommandOutcome {
        let decoded = match admission::decode(
            Selection {
                operation,
                mode: Mode::Command,
            },
            raw,
        ) {
            Ok(d) => d,
            Err(refusal) => {
                return self.address(refusal, |s, key, error| s.command_rejected(key, error));
            }
        };
        let CallerKey::Command(request_key) = decoded.key else {
            return Err(GatewayFailure::HandlerContract(
                "command admitted with a query key".into(),
            ));
        };
        let request = AuthorityRequest {
            descriptor: decoded.descriptor,
            binding: decoded.binding,
            effect: decoded.effect,
            action: decoded.action,
            input: &decoded.input,
            fingerprint: &decoded.fingerprint,
        };
        let authorization = match self.authority.authorize(ctx, &request) {
            Ok(a) => a,
            Err(denial) => {
                let code = match denial {
                    AuthorityDenial::PermissionDenied
                    | AuthorityDenial::NotFound
                    | AuthorityDenial::Policy(_) => Rejection::PermissionDenied,
                    AuthorityDenial::InvalidTarget => Rejection::InvalidTarget,
                };
                let correlation_id = self.correlation();
                if let AuthorityDenial::Policy(reason) = denial {
                    self.record_denial(ctx, decoded.descriptor.name, reason, &correlation_id);
                }
                return Ok(CommandReply::Rejected {
                    protocol: Protocol::Draft1,
                    request_key,
                    code,
                    retry: RetryAdvice::Never,
                    correlation_id,
                });
            }
        };
        let mut admitted = AdmittedCommand {
            descriptor: decoded.descriptor,
            binding: decoded.binding,
            identity: ReceiptIdentity::bind(ctx, request_key.clone()),
            input: decoded.input,
            fingerprint: decoded.fingerprint,
            revision: decoded.revision,
            effect: decoded.effect,
            action: decoded.action,
            prepared: None,
            targets: decoded.targets,
            project_targets: decoded.project_targets,
        };
        // Preparation runs only for an admitted, currently authorized command and before its unit
        // of work; it never records a receipt, so a failure here is retryable with the same key.
        // Authority that can only replay never reaches a new decision, so it prepares nothing
        // (no source read, no staging): the unit of work replays the receipt or denies.
        let cancellation = options.cancellation.clone();
        let env = Preparation::new(self.store, cancellation.as_ref(), options.deadline_ms);
        let preparing = if authorization.is_replay_only() {
            Ok(None)
        } else {
            handler.prepare(ctx, &admitted, &env)
        };
        match preparing {
            Ok(prepared) => admitted.prepared = prepared,
            Err(e) => {
                // Same mapping as a write that was not admitted: nothing durable decided.
                let retry = match e {
                    StorageError::IncompatibleSchema(_) => RetryAdvice::Never,
                    _ => RetryAdvice::SameCommand,
                };
                return Ok(self.rejected(request_key, Rejection::StorageUnavailable, retry));
            }
        }
        // The use case settles what it prepared once the outcome is known (for example a staged
        // file whose metadata a refusal or rollback never committed).
        let prepared = admitted.prepared.clone();
        let settle = handler.clone();
        let owned_ctx = ctx.clone();
        // Only a command that entered as the user (or the host itself) may write authority
        // records through the seams, and only as that principal.
        let ingress = matches!(
            ctx.principal_kind(),
            PrincipalKind::Human | PrincipalKind::Host
        )
        .then(|| ctx.principal().clone());
        let outcome = run_write(self.store, options, move |tx| {
            unit_of_work(
                tx,
                &owned_ctx,
                &admitted,
                &authorization,
                ingress,
                &*handler,
            )
        });
        let reply = match outcome {
            Ok(Unit::Recorded(receipt)) => CommandReply::Recorded {
                protocol: Protocol::Draft1,
                receipt,
                replayed: false,
            },
            Ok(Unit::Replayed(receipt)) => CommandReply::Recorded {
                protocol: Protocol::Draft1,
                receipt,
                replayed: true,
            },
            Ok(Unit::IdempotencyConflict) => self.rejected(
                request_key,
                Rejection::IdempotencyConflict,
                RetryAdvice::RefreshAndNewRequest,
            ),
            Ok(Unit::IncompatibleRecord) => self.rejected(
                request_key,
                Rejection::UnsupportedSchema,
                RetryAdvice::ResolveReceipt,
            ),
            Ok(Unit::Denied) => {
                self.rejected(request_key, Rejection::PermissionDenied, RetryAdvice::Never)
            }
            Ok(Unit::Contract(e)) => {
                settle.settle(ctx, prepared.as_ref(), None, &env);
                return Err(GatewayFailure::HandlerContract(e));
            }
            Err(StorageError::CommitUnknown) => CommandReply::CommandStatusUnknown {
                protocol: Protocol::Draft1,
                request_key,
                correlation_id: self.correlation(),
            },
            Err(StorageError::IncompatibleSchema(_)) => self.rejected(
                request_key,
                Rejection::StorageUnavailable,
                RetryAdvice::Never,
            ),
            // Not admitted, cancelled, or rolled back before commit: nothing was written.
            Err(_) => self.rejected(
                request_key,
                Rejection::StorageUnavailable,
                RetryAdvice::SameCommand,
            ),
        };
        settle.settle(ctx, prepared.as_ref(), Some(&reply), &env);
        validate_command_reply(&reply).map_err(GatewayFailure::HandlerContract)?;
        Ok(reply)
    }

    fn rejected(
        &self,
        request_key: RequestKey,
        code: Rejection,
        retry: RetryAdvice,
    ) -> DomainCommandReply {
        CommandReply::Rejected {
            protocol: Protocol::Draft1,
            request_key,
            code,
            retry,
            correlation_id: self.correlation(),
        }
    }

    fn query_fault(
        &self,
        query_id: QueryId,
        code: DomainErrorCode,
        retry: RetryAdvice,
    ) -> DomainQueryReply {
        DomainQueryReply::Rejected {
            protocol: Protocol::Draft1,
            query_id,
            fault: DomainFault {
                code,
                retry,
                correlation_id: self.correlation(),
                target_id: None,
                target_kind: None,
                revision_dimension: None,
                expected_revision: None,
                current_revision: None,
                blocker_ids: BoundedList(Vec::new()),
                diagnostic_id: None,
            },
        }
    }

    fn command_rejected(
        &self,
        key: CallerKey,
        error: &AdmissionError,
    ) -> Option<DomainCommandReply> {
        let CallerKey::Command(request_key) = key else {
            return None;
        };
        let code = match error {
            AdmissionError::UnsupportedProtocol(_)
            | AdmissionError::UnsupportedSchemaRevision { .. } => Rejection::UnsupportedSchema,
            _ => Rejection::InvalidRequest,
        };
        Some(self.rejected(request_key, code, RetryAdvice::Never))
    }

    fn query_rejected(&self, key: CallerKey, error: &AdmissionError) -> Option<DomainQueryReply> {
        let CallerKey::Query(query_id) = key else {
            return None;
        };
        let code = match error {
            AdmissionError::UnsupportedProtocol(_)
            | AdmissionError::UnsupportedSchemaRevision { .. } => DomainErrorCode::SchemaMismatch,
            _ => DomainErrorCode::InvalidArgument,
        };
        Some(self.query_fault(query_id, code, RetryAdvice::Never))
    }

    /// Addresses an admission refusal to the caller when its key was valid; otherwise reports it
    /// as unaddressable so the adapter can surface it on its own channel.
    fn address<R>(
        &self,
        refusal: Refusal,
        address: impl FnOnce(&Self, CallerKey, &AdmissionError) -> Option<R>,
    ) -> Result<R, GatewayFailure> {
        if let Some(key) = refusal.key
            && let Some(reply) = address(self, key, &refusal.error)
        {
            return Ok(reply);
        }
        Err(GatewayFailure::Unaddressable {
            error: refusal.error.redacted(),
            correlation_id: self.correlation(),
        })
    }

    /// Records one attributed `AuthorityDenied` history event for a command refused by policy, so
    /// its typed reason and rationale are visible (looked up by the rejection's correlation ID).
    /// It is not a receipt and mints no work. A principal the store does not know cannot be
    /// attributed and is not recorded; a recording failure never changes the rejection.
    fn record_denial(
        &self,
        ctx: &InvocationContext,
        operation: &'static str,
        reason: crate::authority::DenialReason,
        correlation_id: &CorrelationId,
    ) {
        let (ctx, correlation) = (ctx.clone(), correlation_id.0.as_str().to_owned());
        let _ = run_write(self.store, WriteOptions::default(), move |tx| {
            if &tx.store().store_id != ctx.store_id()
                || tx.actors().actor_kind(ctx.principal())?.is_none()
            {
                return Ok(());
            }
            let sources: Vec<&str> = ctx.authority_sources().iter().map(Id::as_str).collect();
            let payload = json!({
                "operation": operation,
                "reason": reason.as_str(),
                "rationale": reason.rationale(),
                "principalKind": format!("{:?}", ctx.principal_kind()),
                "projectId": ctx.project_id().map(|p| p.0.as_str()),
                "rootId": ctx.root_id().map(|r| r.0.as_str()),
                "authoritySources": sources,
            });
            let event = |tx: &mut dyn WriteTransaction, lineage: bool| NewEvent {
                id: EventId(tx.new_id(IdKind::Event)),
                actor: ctx.principal().clone(),
                project_id: lineage.then(|| ctx.project_id().cloned()).flatten(),
                root_id: lineage.then(|| ctx.root_id().cloned()).flatten(),
                operation_id: None,
                aggregate_kind: crate::authority::service::DENIAL_AGGREGATE.into(),
                aggregate_id: correlation.clone(),
                aggregate_revision: 1,
                kind: "AuthorityDenied".into(),
                payload: payload.clone(),
            };
            // Attach it to the work history when the lineage resolves; otherwise keep it
            // unattached rather than lose it.
            let savepoint = tx.savepoint()?;
            let with_lineage = event(tx, true);
            if tx.history().append(with_lineage).is_err() {
                tx.rollback_to(savepoint)?;
                let plain = event(tx, false);
                tx.history().append(plain)?;
            } else {
                tx.release(savepoint)?;
            }
            Ok(())
        });
    }
}

/// Fault target kind of a policy denial: `authority:<DenialReason>`.
pub fn denial_target(reason: crate::authority::DenialReason) -> String {
    format!("authority:{}", reason.as_str())
}

fn storage_fault(e: &StorageError) -> (DomainErrorCode, RetryAdvice) {
    match e {
        StorageError::IncompatibleSchema(_) => {
            (DomainErrorCode::SchemaMismatch, RetryAdvice::Never)
        }
        StorageError::ViewUnavailable => (DomainErrorCode::ViewUnavailable, RetryAdvice::Never),
        StorageError::CursorExpired => (
            DomainErrorCode::CursorExpired,
            RetryAdvice::RefreshAndNewRequest,
        ),
        StorageError::Cancelled => (DomainErrorCode::Cancelled, RetryAdvice::Never),
        _ => (
            DomainErrorCode::StorageUnavailable,
            RetryAdvice::SameCommand,
        ),
    }
}

/// The command's write unit of work. Runs on the sole write owner inside one transaction.
fn unit_of_work(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    command: &AdmittedCommand,
    authorization: &Authorization,
    ingress: Option<ActorId>,
    handler: &dyn CommandHandler,
) -> Result<Unit, StorageError> {
    let store = tx.store();
    if &store.store_id != ctx.store_id() || store.restore_epoch != ctx.restore_epoch().get() {
        // An invocation bound to another store or an earlier restore epoch is not current
        // authority, and an old-epoch receipt is never replayed as fresh authority.
        return Ok(Unit::Denied);
    }
    if tx.actors().actor_kind(ctx.principal())?.is_none() {
        // Receipts and history are attributed to a host-registered principal.
        return Ok(Unit::Denied);
    }
    if !crate::authority::fences_hold(tx, authorization.fences())? {
        // Authority changed after it was resolved (revoked or re-bound principal, narrowed
        // policy). Checked before the receipt lookup, so a revoked caller is denied rather
        // than handed an old receipt.
        return Ok(Unit::Denied);
    }

    // Step 3: the receipt check precedes any current-state evaluation. A key reused for another
    // operation is a different intent. The stored contract and schema revision are checked before
    // the fingerprint because the fingerprint embeds the schema revision: a receipt under a
    // revision the operation no longer registers would otherwise be misreported as a changed
    // intent instead of an incompatible record. A receipt under another *registered* revision is
    // a different intent of the same key (its fingerprint differs), never a replay.
    match tx.receipts().lookup(&command.identity)? {
        Some(LedgerEntry::Current(stored)) => {
            if stored.operation != command.descriptor.name {
                return Ok(Unit::IdempotencyConflict);
            }
            if stored.contract != CONTRACT || !command.descriptor.supports(stored.schema_revision) {
                hold_incompatible(tx, ctx, command, &stored.contract, stored.schema_revision)?;
                return Ok(Unit::IncompatibleRecord);
            }
            if stored.schema_revision != command.revision
                || stored.fingerprint != command.fingerprint
            {
                return Ok(Unit::IdempotencyConflict);
            }
            // Replayed under the projection of the revision it was recorded under (the
            // negotiated one): an older receipt is never widened, a newer one never narrowed.
            let mut receipt = stored.receipt;
            project_decision(&mut receipt.decision, stored.schema_revision);
            return Ok(Unit::Replayed(receipt));
        }
        Some(LedgerEntry::Incompatible {
            operation,
            contract,
            schema_revision,
            ..
        }) => {
            if operation != command.descriptor.name {
                return Ok(Unit::IdempotencyConflict);
            }
            hold_incompatible(tx, ctx, command, &contract, schema_revision)?;
            return Ok(Unit::IncompatibleRecord);
        }
        // Authority that can only replay never reaches a new decision.
        None if authorization.is_replay_only() => return Ok(Unit::Denied),
        None => {}
    }

    // Step 4: the use case reads current state and decides inside this transaction. It gets a
    // view of the transaction that cannot record receipts or command history (ARCH-02). Any
    // savepoint it leaves open is discarded when this outer savepoint is unwound.
    let savepoint = tx.savepoint()?;
    let outcome = handler.decide(ctx, command, &mut HandlerTx::new(tx, ingress));
    let decided = match outcome {
        Ok(mut decided) => {
            if let Err(e) = check_decision(command, &decided) {
                tx.rollback_to(savepoint)?;
                return Ok(Unit::Contract(e));
            }
            // Receipts are persisted and returned: secrets are redacted first, so the reply and
            // any replay carry the same redacted decision. One that cannot be redacted is
            // refused with every write undone, never stored unredacted.
            decided.decision = match crate::authority::redact::redact_decision(decided.decision) {
                Ok(d) => d,
                Err(e) => {
                    tx.rollback_to(savepoint)?;
                    return Ok(Unit::Contract(e));
                }
            };
            // The receipt (and so every replay) carries the negotiated revision's projection.
            project_decision(&mut decided.decision, command.revision);
            if matches!(decided.decision, DomainDecision::Refused { .. }) {
                // Receipt-only decisions do not mutate work (step 5): every write the use case
                // made is undone and only the receipt and its CommandDecided history remain.
                tx.rollback_to(savepoint)?;
                if let Some(op) = &decided.operation_id
                    && tx.operations().operation(op)?.is_none()
                {
                    return Ok(Unit::Contract(
                        "a refused decision can only reference an operation that already existed"
                            .into(),
                    ));
                }
            } else {
                let resume = authorization.grants().iter().map(|g| (false, g));
                let host = authorization.authority_grants().iter().map(|g| (true, g));
                for (authority, grant) in resume.chain(host) {
                    let spent = if authority {
                        tx.authority()
                            .consume_grant(grant, ctx.principal())
                            .map(|_| ())
                    } else {
                        tx.grants().consume(grant, ctx.principal()).map(|_| ())
                    };
                    match spent {
                        Ok(_) => {}
                        Err(
                            StorageError::ScopeDenied
                            | StorageError::RevisionConflict(_)
                            | StorageError::NotFound { .. },
                        ) => {
                            tx.rollback_to(savepoint)?;
                            return Ok(Unit::Denied);
                        }
                        Err(e) => return Err(e),
                    }
                }
                tx.release(savepoint)?;
            }
            decided
        }
        Err(HandlerFailure::Storage(StorageError::RevisionConflict(conflict))) => {
            // Undo every partial domain write; the conflict itself is a durable decision.
            tx.rollback_to(savepoint)?;
            let correlation_id = CorrelationId(tx.new_id(IdKind::Correlation));
            Decided {
                decision: DomainDecision::Refused {
                    fault: conflict_fault(*conflict, correlation_id),
                },
                operation_id: None,
            }
        }
        // Any other failure rolls back the entire command: no receipt, history or effects.
        Err(HandlerFailure::Storage(e)) => return Err(e),
    };

    // Step 5: history and receipt in the same transaction as the domain changes.
    let command_id = CommandId(tx.new_id(IdKind::Command));
    let event_id = EventId(tx.new_id(IdKind::Event));
    let accepted_at_ms = crate::protocol::Counter::new(tx.now_ms())
        .map_err(|_| StorageError::InvalidRecord("transaction clock before epoch".into()))?;
    // The decision is attached to the work it acted on (HISTORY-02), so a work item's history
    // shows every command decided on it, from the UI or an agent alike.
    let scope = decision_scope(
        tx,
        ctx,
        &command.targets,
        &command.project_targets,
        decided.operation_id.as_ref(),
    )?;
    let sequence = tx.history().append(NewEvent {
        id: event_id.clone(),
        actor: ctx.principal().clone(),
        project_id: scope.project,
        root_id: scope.root,
        operation_id: decided.operation_id.clone(),
        aggregate_kind: "command".into(),
        aggregate_id: command_id.0.as_str().to_owned(),
        aggregate_revision: 1,
        kind: "CommandDecided".into(),
        payload: json!({
            "operation": command.descriptor.name,
            "contract": CONTRACT,
            "schemaRevision": command.revision.0,
            "requestDigest": command.fingerprint.as_str(),
            "decision": decision_kind(&decided.decision),
            "subject": scope.subject,
        }),
    })?;
    let committed_sequence = Revision::new(sequence)
        .map_err(|_| StorageError::InvalidRecord("non-positive committed sequence".into()))?;
    let decision = decided.decision;
    let receipt = Receipt {
        command_id,
        committed_sequence,
        accepted_at_ms,
        request_digest: command.fingerprint.as_str().to_owned(),
        operation_id: decided.operation_id,
        decision,
    };
    tx.receipts().record(
        &command.identity,
        &StoredReceipt {
            operation: command.descriptor.name.to_owned(),
            contract: CONTRACT.to_owned(),
            schema_revision: command.revision,
            fingerprint: command.fingerprint.clone(),
            receipt: receipt.clone(),
        },
    )?;
    // The receipt's durable typed reply is published after commit; its intent commits here.
    crate::services::artifacts::replies::record_intent(
        tx,
        &command.identity,
        &receipt.command_id,
        &event_id,
    )?;
    Ok(Unit::Recorded(receipt))
}

/// Where a decision belongs in history.
struct DecisionScope {
    project: Option<crate::protocol::ProjectId>,
    root: Option<crate::protocol::RootId>,
    /// The decision's real subject and its revision dimension and value after the decision
    /// (`{"kind","id","dimension","revision"}`), or `null` when the command names no existing
    /// work item or operation (a creation, a session or settings command).
    subject: serde_json::Value,
}

/// Resolves a decision's scope inside its transaction, after the use case decided:
///
/// 1. the first work item the input names as its target ([`admission::work_targets`]) that
///    exists and lies in the invocation's project (any project for a workspace invocation) —
///    the decision's subject is that work item at its current work revision;
/// 2. otherwise the invocation's host-resolved lineage root, if it exists in that project — the
///    decision is attached to it, with no claimed subject;
/// 3. the project is the root's; else the first project the input names
///    ([`admission::project_targets`]) that exists and lies in the invocation's project (any
///    project for a workspace invocation), so a workspace command naming a project belongs to
///    that project's history; else the invocation's project when it exists. A missing root never
///    drops a resolvable project.
///
/// A decision with a durable operation names that operation and its revision as its subject
/// when no work target exists.
fn decision_scope(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    targets: &[Id],
    project_targets: &[crate::protocol::ProjectId],
    operation: Option<&crate::protocol::OperationId>,
) -> Result<DecisionScope, StorageError> {
    let in_scope = |p: &crate::protocol::ProjectId| ctx.project_id().is_none_or(|c| c == p);
    for t in targets {
        if let Some(w) = tx
            .sessions()
            .work_root(&crate::protocol::RootId(t.clone()))?
            && in_scope(&w.project_id)
        {
            return Ok(DecisionScope {
                subject: json!({"kind": "root", "id": w.id.0.as_str(), "dimension": "Work",
                    "revision": w.revision}),
                project: Some(w.project_id),
                root: Some(w.id),
            });
        }
    }
    let subject = match operation {
        Some(op) => match tx.operations().operation(op)? {
            Some(o) => json!({"kind": "operation", "id": op.0.as_str(),
                "dimension": "Operation", "revision": o.revision}),
            None => serde_json::Value::Null,
        },
        None => serde_json::Value::Null,
    };
    if let Some(root) = ctx.root_id()
        && let Some(w) = tx.sessions().work_root(root)?
        && in_scope(&w.project_id)
    {
        return Ok(DecisionScope {
            project: Some(w.project_id),
            root: Some(w.id),
            subject,
        });
    }
    let mut project = None;
    for p in project_targets {
        if in_scope(p) && tx.sessions().project_exists(p)? {
            project = Some(p.clone());
            break;
        }
    }
    let project = match (project, ctx.project_id()) {
        (Some(p), _) => Some(p),
        (None, Some(p)) if tx.sessions().project_exists(p)? => Some(p.clone()),
        _ => None,
    };
    Ok(DecisionScope {
        project,
        root: None,
        subject,
    })
}

/// History a use case may never append through the handler view: the gateway's own command,
/// receipt and denial records, the authority seams' principal, grant and repository-deletion
/// records (written through [`WriteTransaction::authority_history`]), and receipt reply
/// publication (written by the reply publisher on its own transaction).
fn gateway_owned(event: &NewEvent) -> bool {
    use crate::authority::deletion::DELETION_AGGREGATE;
    use crate::authority::service::{DENIAL_AGGREGATE, GRANT_AGGREGATE, PRINCIPAL_AGGREGATE};
    use crate::services::artifacts::replies::REPLY_AGGREGATE;
    matches!(
        event.aggregate_kind.as_str(),
        "command"
            | "command_receipt"
            | DENIAL_AGGREGATE
            | PRINCIPAL_AGGREGATE
            | GRANT_AGGREGATE
            | DELETION_AGGREGATE
            | REPLY_AGGREGATE
    ) || matches!(
        event.kind.as_str(),
        "CommandDecided"
            | "ReceiptIncompatible"
            | "AuthorityDenied"
            | "PrincipalAuthorityBound"
            | "PrincipalAuthorityRevoked"
            | "AuthorityGrantIssued"
            | "AuthorityGrantRevoked"
            | "RepositoryDeletionConfirmed"
            | "RepositoryDeletionAuthorized"
            | "RepositoryDeletionCompleted"
            | "RepositoryDeletionPartial"
            | "ReceiptReplyPublished"
    )
}

/// The transaction a use case decides against: every repository of the unit of work except the
/// receipt ledger and command history, which only the gateway records.
struct HandlerTx<'a> {
    inner: RefCell<&'a mut dyn WriteTransaction>,
    info: StoreInfo,
    /// The principal of a user- or host-ingress command; authority writes are allowed only for
    /// it, and never for a command an agent, package, MCP client or schedule submitted.
    ingress: Option<ActorId>,
}
impl<'a> HandlerTx<'a> {
    fn new(inner: &'a mut dyn WriteTransaction, ingress: Option<ActorId>) -> Self {
        let info = inner.store().clone();
        Self {
            ingress,
            inner: RefCell::new(inner),
            info,
        }
    }
}
fn ledger_owned() -> StorageError {
    StorageError::InvalidRecord("command receipts are recorded only by the gateway".into())
}
fn authority_owned() -> StorageError {
    StorageError::InvalidRecord(
        "authority records are written only through the authority seams".into(),
    )
}
impl GrantRead for HandlerTx<'_> {
    fn grant(&self, id: &Id) -> Result<Option<GrantRecord>, StorageError> {
        self.inner.borrow_mut().grants().grant(id)
    }
}
impl GrantLedger for HandlerTx<'_> {
    fn issue(&mut self, _: NewGrant) -> Result<GrantRecord, StorageError> {
        Err(authority_owned())
    }
    fn consume(&mut self, _: &GrantUse, _: &ActorId) -> Result<GrantRecord, StorageError> {
        Err(authority_owned())
    }
}
impl AuthorityRead for HandlerTx<'_> {
    fn principal(&self, actor: &ActorId) -> Result<Option<PrincipalBinding>, StorageError> {
        self.inner.borrow_mut().authority().principal(actor)
    }
    fn authority_grant(&self, id: &Id) -> Result<Option<AuthorityGrant>, StorageError> {
        self.inner.borrow_mut().authority().authority_grant(id)
    }
    fn registered_repositories(&self) -> Result<Vec<RegisteredRepository>, StorageError> {
        self.inner
            .borrow_mut()
            .authority()
            .registered_repositories()
    }
}
impl AuthorityLedger for HandlerTx<'_> {
    fn bind_principal(
        &mut self,
        _: NewPrincipalBinding,
        _: Option<i64>,
    ) -> Result<PrincipalBinding, StorageError> {
        Err(authority_owned())
    }
    fn revoke_principal(&mut self, _: &ActorId, _: i64) -> Result<PrincipalBinding, StorageError> {
        Err(authority_owned())
    }
    fn issue_grant(&mut self, _: NewAuthorityGrant) -> Result<AuthorityGrant, StorageError> {
        Err(authority_owned())
    }
    fn revoke_grant(
        &mut self,
        _: &Id,
        _: i64,
        _: &ActorId,
    ) -> Result<AuthorityGrant, StorageError> {
        Err(authority_owned())
    }
    fn consume_grant(&mut self, _: &GrantUse, _: &ActorId) -> Result<AuthorityGrant, StorageError> {
        Err(authority_owned())
    }
}
impl CommandLedgerRead for HandlerTx<'_> {
    fn lookup(&self, _: &ReceiptIdentity) -> Result<Option<LedgerEntry>, StorageError> {
        Err(ledger_owned())
    }
    fn reply(&self, _: &ReceiptIdentity) -> Result<Option<ReceiptReply>, StorageError> {
        Err(ledger_owned())
    }
    // Only the ownership answer passes through (queue resolution of reply deliveries needs it);
    // the receipt itself stays unreadable to use cases.
    fn may_be_receipt_of(
        &self,
        identity: &ReceiptIdentity,
        command: &CommandId,
    ) -> Result<bool, StorageError> {
        self.inner
            .borrow_mut()
            .receipts()
            .may_be_receipt_of(identity, command)
    }
}
impl CommandLedger for HandlerTx<'_> {
    fn record(&mut self, _: &ReceiptIdentity, _: &StoredReceipt) -> Result<(), StorageError> {
        Err(ledger_owned())
    }
    fn record_reply(&mut self, _: &ReceiptIdentity, _: &ReceiptReply) -> Result<(), StorageError> {
        Err(ledger_owned())
    }
}
impl HistoryRead for HandlerTx<'_> {
    fn event(&self, id: &EventId) -> Result<Option<EventRecord>, StorageError> {
        self.inner.borrow_mut().history().event(id)
    }
    fn page(
        &self,
        filter: &EventFilter,
        after: i64,
        limit: u16,
    ) -> Result<EventPage, StorageError> {
        self.inner.borrow_mut().history().page(filter, after, limit)
    }
}
impl HistoryLog for HandlerTx<'_> {
    fn append(&mut self, event: NewEvent) -> Result<i64, StorageError> {
        if gateway_owned(&event) {
            return Err(StorageError::InvalidRecord(
                "command, authority and receipt reply history is recorded only by the gateway, \
                 the authority seams and the reply publisher"
                    .into(),
            ));
        }
        self.inner.get_mut().history().append(event)
    }
}

// The artifact repository is narrowed to what the artifact use cases write: blobs they publish
// or release (`Present` → `UnlinkPending`), explicitly retained artifacts, their versions,
// links, tags and shares, and the reference revision of a receipt reply they link. Backup pins,
// physical-removal claims (the collector's) and receipt reply records (the reply publisher's,
// on its own transaction) are refused, so a use case can neither block collection, remove a
// file, nor create, replace or retire a receipt's reply.
use crate::ports::storage::{
    ArtifactLink, ArtifactRead, ArtifactRetention, BlobRecord, BlobState, ListPosition,
    StoredArtifact, StoredVersion, SubjectKey,
};
use crate::protocol::{OperationId, ProjectId};
fn artifact_owned() -> StorageError {
    StorageError::InvalidRecord(
        "backup pins, removal claims and receipt reply records are written only by the \
         artifact store's own paths"
            .into(),
    )
}
impl HandlerTx<'_> {
    /// Refuses a write that would change a receipt reply (`artifact` is its ID).
    fn explicit_only(&mut self, artifact: &Id) -> Result<(), StorageError> {
        match self.inner.get_mut().artifacts().artifact(artifact)? {
            Some(a) if a.retention == ArtifactRetention::Receipt => Err(artifact_owned()),
            _ => Ok(()),
        }
    }
}
impl ArtifactRead for HandlerTx<'_> {
    fn project_exists(&self, project: &ProjectId) -> Result<bool, StorageError> {
        self.inner.borrow_mut().artifacts().project_exists(project)
    }
    fn subject_exists(
        &self,
        project: &ProjectId,
        subject: &SubjectKey,
    ) -> Result<bool, StorageError> {
        self.inner
            .borrow_mut()
            .artifacts()
            .subject_exists(project, subject)
    }
    fn artifact(&self, id: &Id) -> Result<Option<StoredArtifact>, StorageError> {
        self.inner.borrow_mut().artifacts().artifact(id)
    }
    fn version(&self, id: &Id) -> Result<Option<StoredVersion>, StorageError> {
        self.inner.borrow_mut().artifacts().version(id)
    }
    fn versions(&self, artifact: &Id) -> Result<Vec<StoredVersion>, StorageError> {
        self.inner.borrow_mut().artifacts().versions(artifact)
    }
    fn version_by_operation(
        &self,
        operation: &OperationId,
    ) -> Result<Option<StoredVersion>, StorageError> {
        self.inner
            .borrow_mut()
            .artifacts()
            .version_by_operation(operation)
    }
    fn list(
        &self,
        project: &ProjectId,
        tags: &[String],
        after: Option<&ListPosition>,
        limit: u16,
    ) -> Result<Vec<StoredArtifact>, StorageError> {
        self.inner
            .borrow_mut()
            .artifacts()
            .list(project, tags, after, limit)
    }
    fn list_workspace(
        &self,
        owner: Option<&ActorId>,
        tags: &[String],
        after: Option<&ListPosition>,
        limit: u16,
    ) -> Result<Vec<StoredArtifact>, StorageError> {
        self.inner
            .borrow_mut()
            .artifacts()
            .list_workspace(owner, tags, after, limit)
    }
    fn tags(&self, artifact: &Id) -> Result<Vec<String>, StorageError> {
        self.inner.borrow_mut().artifacts().tags(artifact)
    }
    fn shares(&self, artifact: &Id) -> Result<Vec<ProjectId>, StorageError> {
        self.inner.borrow_mut().artifacts().shares(artifact)
    }
    fn links(&self, artifact: &Id) -> Result<Vec<ArtifactLink>, StorageError> {
        self.inner.borrow_mut().artifacts().links(artifact)
    }
    fn linked_to(&self, subject: &SubjectKey) -> Result<Vec<ArtifactLink>, StorageError> {
        self.inner.borrow_mut().artifacts().linked_to(subject)
    }
    fn blob(&self, digest: &str) -> Result<Option<BlobRecord>, StorageError> {
        self.inner.borrow_mut().artifacts().blob(digest)
    }
    fn live_references(&self, digest: &str) -> Result<i64, StorageError> {
        self.inner.borrow_mut().artifacts().live_references(digest)
    }
    fn pinned(&self, digest: &str) -> Result<bool, StorageError> {
        self.inner.borrow_mut().artifacts().pinned(digest)
    }
    fn blobs_in(
        &self,
        state: BlobState,
        after: Option<&str>,
        limit: u16,
    ) -> Result<Vec<BlobRecord>, StorageError> {
        self.inner
            .borrow_mut()
            .artifacts()
            .blobs_in(state, after, limit)
    }
    fn project_usage(&self, project: &ProjectId) -> Result<i64, StorageError> {
        self.inner.borrow_mut().artifacts().project_usage(project)
    }
    fn workspace_usage(&self) -> Result<i64, StorageError> {
        self.inner.borrow_mut().artifacts().workspace_usage()
    }
    fn receipt_usage(&self, project: Option<&ProjectId>) -> Result<i64, StorageError> {
        self.inner.borrow_mut().artifacts().receipt_usage(project)
    }
    fn unresolved_staging(
        &self,
        after: Option<&OperationId>,
        limit: u16,
    ) -> Result<Vec<OperationId>, StorageError> {
        self.inner
            .borrow_mut()
            .artifacts()
            .unresolved_staging(after, limit)
    }
}
impl ArtifactRepository for HandlerTx<'_> {
    fn put_blob(
        &mut self,
        digest: &str,
        byte_count: i64,
        generation: &str,
        relative_path: &str,
    ) -> Result<BlobRecord, StorageError> {
        self.inner
            .get_mut()
            .artifacts()
            .put_blob(digest, byte_count, generation, relative_path)
    }
    fn set_blob_state(
        &mut self,
        digest: &str,
        expected: BlobState,
        to: BlobState,
        error: Option<&str>,
    ) -> Result<BlobRecord, StorageError> {
        // Releasing a blob no live version names is a use case's; every other transition
        // belongs to collection and recovery.
        if (expected, to) != (BlobState::Present, BlobState::UnlinkPending) {
            return Err(artifact_owned());
        }
        self.inner
            .get_mut()
            .artifacts()
            .set_blob_state(digest, expected, to, error)
    }
    fn insert_artifact(&mut self, artifact: &StoredArtifact) -> Result<(), StorageError> {
        if artifact.retention == ArtifactRetention::Receipt {
            return Err(artifact_owned());
        }
        self.inner.get_mut().artifacts().insert_artifact(artifact)
    }
    fn insert_version(&mut self, version: &StoredVersion) -> Result<(), StorageError> {
        self.explicit_only(&version.artifact_id)?;
        self.inner.get_mut().artifacts().insert_version(version)
    }
    fn update_artifact(&mut self, artifact: &StoredArtifact) -> Result<(), StorageError> {
        // A use case may only advance a receipt reply's reference revision (linking it to work,
        // `artifacts.reference`); its content, scope, state and retention never change here.
        if let Some(current) = self.inner.get_mut().artifacts().artifact(&artifact.id)?
            && current.retention == ArtifactRetention::Receipt
        {
            let linked_only = StoredArtifact {
                reference_revision: artifact.reference_revision,
                ..current
            };
            if &linked_only != artifact {
                return Err(artifact_owned());
            }
        }
        self.inner.get_mut().artifacts().update_artifact(artifact)
    }
    fn tombstone_version(&mut self, version: &Id, at_ms: i64) -> Result<(), StorageError> {
        if let Some(v) = self.inner.get_mut().artifacts().version(version)? {
            self.explicit_only(&v.artifact_id)?;
        }
        self.inner
            .get_mut()
            .artifacts()
            .tombstone_version(version, at_ms)
    }
    fn add_link(&mut self, link: &ArtifactLink) -> Result<bool, StorageError> {
        self.inner.get_mut().artifacts().add_link(link)
    }
    fn set_tags(&mut self, artifact: &Id, tags: &[String]) -> Result<(), StorageError> {
        self.explicit_only(artifact)?;
        self.inner.get_mut().artifacts().set_tags(artifact, tags)
    }
    fn add_share(&mut self, artifact: &Id, project: &ProjectId) -> Result<bool, StorageError> {
        self.explicit_only(artifact)?;
        self.inner
            .get_mut()
            .artifacts()
            .add_share(artifact, project)
    }
    fn remove_share(&mut self, artifact: &Id, project: &ProjectId) -> Result<bool, StorageError> {
        self.explicit_only(artifact)?;
        self.inner
            .get_mut()
            .artifacts()
            .remove_share(artifact, project)
    }
    fn pin(&mut self, _: &Id, _: &str) -> Result<(), StorageError> {
        Err(artifact_owned())
    }
    fn release_pins(&mut self, _: &Id) -> Result<i64, StorageError> {
        Err(artifact_owned())
    }
    fn claim_blob(&mut self, _: &str, _: &str, _: &str) -> Result<bool, StorageError> {
        Err(artifact_owned())
    }
    fn adopt_claim(&mut self, _: &str, _: &str, _: &str, _: &str) -> Result<bool, StorageError> {
        Err(artifact_owned())
    }
    fn finish_claim(
        &mut self,
        _: &str,
        _: &str,
        _: &str,
        _: Option<&str>,
    ) -> Result<bool, StorageError> {
        Err(artifact_owned())
    }
}

// The durable queue passes through, except that a use case cannot enqueue a receipt reply
// publication: only the gateway records a receipt's intent, so a handler cannot mis-scope or
// pre-empt another receipt's reply.
use crate::ports::storage::{
    ClaimRecovery, ClaimRequest, ConsumerProgress, DeliveryFilter, DeliveryKey, DeliveryPage,
    FailureDisposition, NewQueueEntry, QueueEntry, QueueFence, QueueRead, QueueStatus,
    RecoveryOutcome, Settlement,
};
impl QueueRead for HandlerTx<'_> {
    fn entry(&self, id: &Id) -> Result<Option<QueueEntry>, StorageError> {
        self.inner.borrow_mut().queue().entry(id)
    }
    fn delivery(
        &self,
        consumer: &str,
        event: &EventId,
    ) -> Result<Option<QueueEntry>, StorageError> {
        self.inner.borrow_mut().queue().delivery(consumer, event)
    }
    fn status(&self, consumer: &str) -> Result<QueueStatus, StorageError> {
        self.inner.borrow_mut().queue().status(consumer)
    }
    fn deliveries(
        &self,
        filter: &DeliveryFilter,
        after: Option<&DeliveryKey>,
        limit: u16,
    ) -> Result<DeliveryPage, StorageError> {
        self.inner
            .borrow_mut()
            .queue()
            .deliveries(filter, after, limit)
    }
    fn progress(&self, consumer: &str) -> Result<ConsumerProgress, StorageError> {
        self.inner.borrow_mut().queue().progress(consumer)
    }
    fn checkpoint(&self, consumer: &str, partition: Option<&str>) -> Result<i64, StorageError> {
        self.inner
            .borrow_mut()
            .queue()
            .checkpoint(consumer, partition)
    }
    fn next_due_ms(&self, now_ms: i64) -> Result<Option<i64>, StorageError> {
        self.inner.borrow_mut().queue().next_due_ms(now_ms)
    }
    fn awaiting_eligibility(
        &self,
        consumer: &str,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<QueueEntry>, StorageError> {
        self.inner
            .borrow_mut()
            .queue()
            .awaiting_eligibility(consumer, after, limit)
    }
}
impl DurableQueue for HandlerTx<'_> {
    fn enqueue(&mut self, entry: NewQueueEntry) -> Result<QueueEntry, StorageError> {
        if entry.consumer == crate::services::artifacts::replies::REPLY_CONSUMER {
            return Err(StorageError::InvalidRecord(
                "receipt reply intents are recorded only by the gateway".into(),
            ));
        }
        self.inner.get_mut().queue().enqueue(entry)
    }
    fn claim_ready(&mut self, request: &ClaimRequest) -> Result<Vec<QueueEntry>, StorageError> {
        self.inner.get_mut().queue().claim_ready(request)
    }
    fn renew(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        lease_ms: i64,
    ) -> Result<QueueEntry, StorageError> {
        self.inner.get_mut().queue().renew(id, fence, lease_ms)
    }
    fn settle(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        settlement: Settlement,
    ) -> Result<QueueEntry, StorageError> {
        self.inner.get_mut().queue().settle(id, fence, settlement)
    }
    fn fail(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        cause: &str,
        disposition: FailureDisposition,
    ) -> Result<QueueEntry, StorageError> {
        self.inner
            .get_mut()
            .queue()
            .fail(id, fence, cause, disposition)
    }
    fn cancel(&mut self, id: &Id) -> Result<QueueEntry, StorageError> {
        self.inner.get_mut().queue().cancel(id)
    }
    fn recover_claims(
        &mut self,
        mode: ClaimRecovery,
        limit: u16,
    ) -> Result<RecoveryOutcome, StorageError> {
        self.inner.get_mut().queue().recover_claims(mode, limit)
    }
    fn requeue(&mut self, id: &Id, delay_ms: i64) -> Result<QueueEntry, StorageError> {
        self.inner.get_mut().queue().requeue(id, delay_ms)
    }
    fn hold(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        cause: &str,
    ) -> Result<QueueEntry, StorageError> {
        self.inner.get_mut().queue().hold(id, fence, cause)
    }
    fn dismiss(&mut self, id: &Id, reason: &str) -> Result<QueueEntry, StorageError> {
        self.inner.get_mut().queue().dismiss(id, reason)
    }
    fn reassert(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        lease_ms: i64,
    ) -> Result<QueueEntry, StorageError> {
        self.inner.get_mut().queue().reassert(id, fence, lease_ms)
    }
    fn compact(&mut self, id: &Id) -> Result<(), StorageError> {
        self.inner.get_mut().queue().compact(id)
    }
}

impl WriteTransaction for HandlerTx<'_> {
    fn now_ms(&self) -> i64 {
        self.inner.borrow().now_ms()
    }
    fn new_id(&mut self, kind: IdKind) -> Id {
        self.inner.get_mut().new_id(kind)
    }
    fn store(&self) -> &StoreInfo {
        &self.info
    }
    fn committed_sequence(&self) -> Result<i64, StorageError> {
        self.inner.borrow().committed_sequence()
    }
    fn savepoint(&mut self) -> Result<Savepoint, StorageError> {
        self.inner.get_mut().savepoint()
    }
    fn rollback_to(&mut self, savepoint: Savepoint) -> Result<(), StorageError> {
        self.inner.get_mut().rollback_to(savepoint)
    }
    fn release(&mut self, savepoint: Savepoint) -> Result<(), StorageError> {
        self.inner.get_mut().release(savepoint)
    }
    fn actors(&mut self) -> &mut dyn ActorRegistry {
        self.inner.get_mut().actors()
    }
    fn receipts(&mut self) -> &mut dyn CommandLedger {
        self
    }
    fn history(&mut self) -> &mut dyn HistoryLog {
        self
    }
    fn operations(&mut self) -> &mut dyn OperationJournal {
        self.inner.get_mut().operations()
    }
    fn queue(&mut self) -> &mut dyn DurableQueue {
        self
    }
    // Grant and authority reads pass through; their writes are refused here and reach the
    // store only through the authority seams (which hold the capability) or the gateway's own
    // consumption step, which runs on the unwrapped transaction.
    fn grants(&mut self) -> &mut dyn GrantLedger {
        self
    }
    fn grant_writes(
        &mut self,
        capability: &AuthorityCapability,
        actor: &ActorId,
    ) -> Result<&mut dyn GrantLedger, StorageError> {
        if self.ingress.as_ref() != Some(actor) {
            return Err(authority_owned());
        }
        self.inner.get_mut().grant_writes(capability, actor)
    }
    fn packages(&mut self) -> &mut dyn PackageHandlerLedger {
        self.inner.get_mut().packages()
    }
    fn configuration(&mut self) -> &mut dyn ConfigurationRepository {
        self.inner.get_mut().configuration()
    }
    fn authority(&mut self) -> &mut dyn AuthorityLedger {
        self
    }
    fn authority_writes(
        &mut self,
        capability: &AuthorityCapability,
        actor: &ActorId,
    ) -> Result<&mut dyn AuthorityLedger, StorageError> {
        if self.ingress.as_ref() != Some(actor) {
            return Err(authority_owned());
        }
        self.inner.get_mut().authority_writes(capability, actor)
    }
    fn authority_history(
        &mut self,
        capability: &AuthorityCapability,
        actor: &ActorId,
    ) -> Result<&mut dyn HistoryLog, StorageError> {
        if self.ingress.as_ref() != Some(actor) {
            return Err(authority_owned());
        }
        self.inner.get_mut().authority_history(capability, actor)
    }
    fn artifacts(&mut self) -> &mut dyn ArtifactRepository {
        self
    }
    fn sessions(&mut self) -> &mut dyn SessionRepository {
        self.inner.get_mut().sessions()
    }
}

/// Records one visible `ReceiptIncompatible` history entry per receipt identity. The stored
/// record is never replayed, rewritten or reported as a different intent.
fn hold_incompatible(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    command: &AdmittedCommand,
    stored_contract: &str,
    stored_revision: SchemaRevision,
) -> Result<(), StorageError> {
    let identity = &command.identity;
    let aggregate_id = fingerprint::sha256_hex(
        format!(
            "{}/{}/{}",
            identity.actor_id().0.as_str(),
            identity.scope_key().as_str(),
            identity.request_key().0.as_str()
        )
        .as_bytes(),
    );
    let filter = EventFilter::Aggregate {
        kind: "command_receipt".into(),
        id: aggregate_id.clone(),
    };
    if !tx.history().page(&filter, 0, 1)?.events.is_empty() {
        return Ok(());
    }
    let id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id,
        actor: ctx.principal().clone(),
        project_id: None,
        root_id: None,
        operation_id: None,
        aggregate_kind: "command_receipt".into(),
        aggregate_id,
        aggregate_revision: 1,
        kind: "ReceiptIncompatible".into(),
        payload: json!({
            "operation": command.descriptor.name,
            "storedContract": stored_contract,
            "currentContract": CONTRACT,
            "storedSchemaRevision": stored_revision.0,
            "schemaRevision": command.revision.0,
        }),
    })?;
    Ok(())
}

fn conflict_fault(c: RevisionConflict, correlation_id: CorrelationId) -> DomainFault {
    DomainFault {
        code: DomainErrorCode::RevisionConflict,
        retry: RetryAdvice::RefreshAndNewRequest,
        correlation_id,
        target_id: Some(c.target_id),
        target_kind: Some(BoundedText(c.target_kind.to_owned())),
        revision_dimension: Some(c.dimension),
        expected_revision: Some(c.expected),
        current_revision: Some(c.current),
        blocker_ids: BoundedList(Vec::new()),
        diagnostic_id: None,
    }
}

/// Projects a decision's result onto `revision` (members introduced later are removed).
fn project_decision(decision: &mut DomainDecision, revision: SchemaRevision) {
    if let DomainDecision::Applied { result } | DomainDecision::NoChange { result } = decision {
        result.retain_revision(revision.0);
    }
}

fn decision_kind(d: &DomainDecision) -> &'static str {
    match d {
        DomainDecision::Applied { .. } => "Applied",
        DomainDecision::Accepted { .. } => "Accepted",
        DomainDecision::NoChange { .. } => "NoChange",
        DomainDecision::Refused { .. } => "Refused",
    }
}

/// Rejects decisions that would publish a different operation's result, an `Accepted` without
/// its durable operation or for an operation that has no asynchronous completion contract and
/// effect, or an unknown external outcome without reconciliation identity.
fn check_decision(command: &AdmittedCommand, decided: &Decided) -> Result<(), String> {
    let descriptor = command.descriptor;
    match &decided.decision {
        DomainDecision::Applied { result } | DomainDecision::NoChange { result } => {
            if result.operation() != descriptor.name {
                return Err(format!(
                    "{} result recorded for {}",
                    result.operation(),
                    descriptor.name
                ));
            }
            result.check()
        }
        DomainDecision::Accepted { operation_id } => {
            if !descriptor.completion
                || matches!(command.effect, EffectClass::ReadOnly | EffectClass::Local)
            {
                return Err(format!(
                    "{} has no asynchronous completion for its {:?} effect; it cannot be Accepted",
                    descriptor.name, command.effect
                ));
            }
            if decided.operation_id.as_ref() != Some(operation_id) {
                return Err("accepted operation does not match the receipt operation".into());
            }
            Ok(())
        }
        DomainDecision::Refused { fault } => {
            fault.check()?;
            if matches!(fault.code, DomainErrorCode::OutcomeUnknown)
                && decided.operation_id.is_none()
            {
                return Err("unknown effect needs durable operation identity".into());
            }
            Ok(())
        }
    }
}
