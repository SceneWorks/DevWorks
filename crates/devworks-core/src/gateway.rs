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
//!    consumption → `CommandDecided` history → receipt. Domain changes, outbox intents, consumed
//!    grants, history and receipt commit together or not at all;
//! 5. `Recorded` only after the commit is confirmed; a lost acknowledgment is
//!    `CommandStatusUnknown`, resolved by resubmitting the same command/key (receipt lookup).
//!
//! Malformed, unsupported and denied requests stop before the store, so they never mint durable
//! work or a receipt record. Because lookup and decision run in one serialized transaction, a
//! duplicate that loses a race finds the winner's receipt and its handler never runs.
use crate::admission::{self, AdmissionError, CallerKey, Refusal, Selection};
use crate::context::{InvocationContext, ReceiptIdentity};
use crate::fingerprint::{self, Fingerprint};
use crate::operations::OperationInput;
use crate::ports::storage::{
    EventFilter, LedgerEntry, NewEvent, ReadOptions, RevisionConflict, StorageError, WriteOptions,
    WriteTransaction, read_snapshot, run_write,
};
use crate::ports::{
    AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization, CommandHandler, Decided,
    HandlerFailure, IdKind, IdSource, QueryHandler, StoredReceipt,
};
use crate::protocol::results::{
    DomainCommandReply, DomainDecision, DomainQueryReply, validate_command_reply,
};
use crate::protocol::support::{BoundedList, BoundedText};
use crate::protocol::{
    CONTRACT, Check, CommandId, CommandReply, CorrelationId, DomainErrorCode, DomainFault, EventId,
    Protocol, QueryId, Receipt, Rejection, RequestKey, RetryAdvice, Revision,
};
use crate::registry::{EffectClass, Mode, ModeBinding, OperationDescriptor};
use serde_json::json;
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
}

/// A new command that passed admission, authority and the in-transaction receipt lookup.
#[derive(Debug)]
pub struct AdmittedCommand {
    descriptor: &'static OperationDescriptor,
    binding: &'static ModeBinding,
    identity: ReceiptIdentity,
    input: OperationInput,
    fingerprint: Fingerprint,
    effect: EffectClass,
    action: Option<&'static str>,
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
    /// Effective effect class (action-dependent bindings already classified).
    pub fn effect(&self) -> EffectClass {
        self.effect
    }
    pub fn action(&self) -> Option<&'static str> {
        self.action
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
}
impl fmt::Display for GatewayFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unaddressable { error, .. } => write!(f, "unaddressable request: {error}"),
            Self::HandlerContract(e) => write!(f, "use case contract violation: {e}"),
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
        };
        if let Err(denial) = self.authority.authorize(ctx, &request) {
            let code = match denial {
                AuthorityDenial::PermissionDenied => DomainErrorCode::PermissionDenied,
                AuthorityDenial::InvalidTarget => DomainErrorCode::InvalidArgument,
                AuthorityDenial::NotFound => DomainErrorCode::NotFound,
            };
            return Ok(self.query_fault(query_id, code, RetryAdvice::Never));
        }
        let admitted = AdmittedQuery {
            descriptor: decoded.descriptor,
            binding: decoded.binding,
            query_id,
            input: decoded.input,
            fingerprint: decoded.fingerprint,
        };
        let answered = read_snapshot(self.store, &ReadOptions::default(), |snapshot| {
            let stamp = snapshot.stamp().clone();
            if &stamp.store_id != ctx.store_id() || stamp.restore_epoch != ctx.restore_epoch().get()
            {
                return Ok(None);
            }
            Ok(Some((handler.query(ctx, &admitted, snapshot), stamp)))
        });
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
                DomainQueryReply::Result {
                    protocol: Protocol::Draft1,
                    query_id: admitted.query_id,
                    stamp: answer.stamp,
                    result: answer.result,
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
        };
        let authorization = match self.authority.authorize(ctx, &request) {
            Ok(a) => a,
            Err(denial) => {
                let code = match denial {
                    AuthorityDenial::PermissionDenied | AuthorityDenial::NotFound => {
                        Rejection::PermissionDenied
                    }
                    AuthorityDenial::InvalidTarget => Rejection::InvalidTarget,
                };
                return Ok(self.rejected(request_key, code, RetryAdvice::Never));
            }
        };
        let admitted = AdmittedCommand {
            descriptor: decoded.descriptor,
            binding: decoded.binding,
            identity: ReceiptIdentity::bind(ctx, request_key.clone()),
            input: decoded.input,
            fingerprint: decoded.fingerprint,
            effect: decoded.effect,
            action: decoded.action,
        };
        let owned_ctx = ctx.clone();
        let outcome = run_write(self.store, options, move |tx| {
            unit_of_work(tx, &owned_ctx, &admitted, &authorization, &*handler)
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
            Ok(Unit::Contract(e)) => return Err(GatewayFailure::HandlerContract(e)),
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
            AdmissionError::UnsupportedProtocol(_) => Rejection::UnsupportedSchema,
            _ => Rejection::InvalidRequest,
        };
        Some(self.rejected(request_key, code, RetryAdvice::Never))
    }

    fn query_rejected(&self, key: CallerKey, error: &AdmissionError) -> Option<DomainQueryReply> {
        let CallerKey::Query(query_id) = key else {
            return None;
        };
        let code = match error {
            AdmissionError::UnsupportedProtocol(_) => DomainErrorCode::SchemaMismatch,
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
            error: refusal.error,
            correlation_id: self.correlation(),
        })
    }
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

    // Step 3: the receipt check precedes any current-state evaluation.
    match tx.receipts().lookup(&command.identity)? {
        Some(LedgerEntry::Current(stored)) => {
            if stored.contract != CONTRACT
                || stored.schema_revision != command.descriptor.schema_revision
            {
                hold_incompatible(tx, ctx, command, &stored.contract)?;
                return Ok(Unit::IncompatibleRecord);
            }
            return Ok(if stored.fingerprint == command.fingerprint {
                Unit::Replayed(stored.receipt)
            } else {
                Unit::IdempotencyConflict
            });
        }
        Some(LedgerEntry::Incompatible { contract, .. }) => {
            hold_incompatible(tx, ctx, command, &contract)?;
            return Ok(Unit::IncompatibleRecord);
        }
        None => {}
    }

    // Step 4: the use case reads current state and decides inside this transaction.
    let savepoint = tx.savepoint()?;
    let decided = match handler.decide(ctx, command, tx) {
        Ok(decided) => {
            if let Err(e) = check_decision(command, &decided) {
                tx.rollback_to(savepoint)?;
                return Ok(Unit::Contract(e));
            }
            if !matches!(decided.decision, DomainDecision::Refused { .. }) {
                for grant in authorization.grants() {
                    match tx.grants().consume(grant, ctx.principal()) {
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
            }
            tx.release(savepoint)?;
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
    let sequence = tx.history().append(NewEvent {
        id: event_id,
        actor: ctx.principal().clone(),
        project_id: None,
        root_id: None,
        operation_id: decided.operation_id.clone(),
        aggregate_kind: "command".into(),
        aggregate_id: command_id.0.as_str().to_owned(),
        aggregate_revision: 1,
        kind: "CommandDecided".into(),
        payload: json!({
            "operation": command.descriptor.name,
            "contract": CONTRACT,
            "requestDigest": command.fingerprint.as_str(),
            "decision": decision_kind(&decided.decision),
        }),
    })?;
    let committed_sequence = Revision::new(sequence)
        .map_err(|_| StorageError::InvalidRecord("non-positive committed sequence".into()))?;
    let receipt = Receipt {
        command_id,
        committed_sequence,
        accepted_at_ms,
        request_digest: command.fingerprint.as_str().to_owned(),
        operation_id: decided.operation_id,
        decision: decided.decision,
    };
    tx.receipts().record(
        &command.identity,
        &StoredReceipt {
            operation: command.descriptor.name.to_owned(),
            contract: CONTRACT.to_owned(),
            schema_revision: command.descriptor.schema_revision,
            fingerprint: command.fingerprint.clone(),
            receipt: receipt.clone(),
        },
    )?;
    Ok(Unit::Recorded(receipt))
}

/// Records one visible `ReceiptIncompatible` history entry per receipt identity. The stored
/// record is never replayed, rewritten or reported as a different intent.
fn hold_incompatible(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    command: &AdmittedCommand,
    stored_contract: &str,
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
            "schemaRevision": command.descriptor.schema_revision.0,
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
