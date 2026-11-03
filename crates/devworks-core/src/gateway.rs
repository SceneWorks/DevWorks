//! `ApplicationGateway`: the single entry point for UI actions, agent tools, schedules, package
//! host calls and MCP requests (ARCH-02). There is no second admission path: a typed caller
//! serializes its request and submits it here like any other adapter.
//!
//! Command order: pure admission ([`crate::admission`]) → current authority → receipt identity →
//! receipt lookup (equal fingerprint replays, different fingerprint is `IdempotencyConflict`) →
//! use-case decision → receipt commit → `Recorded`. Malformed, unsupported and denied requests
//! stop before the receipt port, so they never mint durable work or a receipt record.
use crate::admission::{self, AdmissionError, CallerKey, Refusal, Selection};
use crate::context::{InvocationContext, ReceiptIdentity};
use crate::fingerprint::Fingerprint;
use crate::operations::OperationInput;
use crate::ports::{
    AuthorityDenial, AuthorityPort, AuthorityRequest, Clock, CommandHandler, CommitOutcome,
    Decided, HandlerFailure, IdKind, IdSource, NewReceipt, QueryHandler, ReceiptStore,
    ReceiptStoreError, StoredReceipt,
};
use crate::protocol::results::{
    DomainCommandReply, DomainDecision, DomainQueryReply, validate_command_reply,
};
use crate::protocol::support::BoundedList;
use crate::protocol::{
    CONTRACT, Check, CommandId, CommandReply, CorrelationId, DomainErrorCode, DomainFault,
    Protocol, QueryId, Receipt, Rejection, RequestKey, RetryAdvice,
};
use crate::registry::{Mode, ModeBinding, OperationDescriptor};
use std::fmt;

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

/// A new command that passed admission, authority and receipt lookup.
#[derive(Debug)]
pub struct AdmittedCommand {
    descriptor: &'static OperationDescriptor,
    binding: &'static ModeBinding,
    identity: ReceiptIdentity,
    input: OperationInput,
    fingerprint: Fingerprint,
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
}

/// Outcomes that cannot be expressed as an addressed wire reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GatewayFailure {
    /// The frame carried no valid caller identity to address a reply to.
    Unaddressable {
        error: AdmissionError,
        correlation_id: CorrelationId,
    },
    /// A use case returned a result/decision that does not match the admitted operation's
    /// contract; nothing was recorded or published.
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

/// The application gateway over its host ports.
pub struct Gateway<'p> {
    pub authority: &'p dyn AuthorityPort,
    pub receipts: &'p dyn ReceiptStore,
    pub clock: &'p dyn Clock,
    pub ids: &'p dyn IdSource,
}

impl Gateway<'_> {
    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }

    /// Admits and answers a Query for the host-selected `operation`.
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
        let reply = match handler.query(ctx, &admitted) {
            Ok(answer) => {
                if answer.result.operation() != admitted.descriptor.name {
                    return Err(GatewayFailure::HandlerContract(format!(
                        "{} result published for {}",
                        answer.result.operation(),
                        admitted.descriptor.name
                    )));
                }
                DomainQueryReply::Result {
                    protocol: Protocol::Draft1,
                    query_id: admitted.query_id,
                    stamp: answer.stamp,
                    result: answer.result,
                }
            }
            Err(fault) => DomainQueryReply::Rejected {
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
        handler: &dyn CommandHandler,
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
            input: &decoded.input,
        };
        if let Err(denial) = self.authority.authorize(ctx, &request) {
            let code = match denial {
                AuthorityDenial::PermissionDenied | AuthorityDenial::NotFound => {
                    Rejection::PermissionDenied
                }
                AuthorityDenial::InvalidTarget => Rejection::InvalidTarget,
            };
            return Ok(self.rejected(request_key, code, RetryAdvice::Never));
        }
        let identity = ReceiptIdentity::bind(ctx, request_key.clone());
        match self.receipts.lookup(&identity) {
            Ok(Some(stored)) => return Ok(self.replay(stored, &decoded.fingerprint, request_key)),
            Ok(None) => {}
            Err(_) => {
                return Ok(self.rejected(
                    request_key,
                    Rejection::StorageUnavailable,
                    RetryAdvice::SameCommand,
                ));
            }
        }
        let admitted = AdmittedCommand {
            descriptor: decoded.descriptor,
            binding: decoded.binding,
            identity,
            input: decoded.input,
            fingerprint: decoded.fingerprint,
        };
        let decided = match handler.decide(ctx, &admitted) {
            Ok(d) => d,
            Err(HandlerFailure::StorageUnavailable) => {
                return Ok(self.rejected(
                    request_key,
                    Rejection::StorageUnavailable,
                    RetryAdvice::SameCommand,
                ));
            }
        };
        check_decision(admitted.descriptor, &decided).map_err(GatewayFailure::HandlerContract)?;
        let command_id = CommandId(self.ids.next_id(IdKind::Command));
        let accepted_at_ms = self.clock.now_ms();
        let new = NewReceipt {
            operation: admitted.descriptor.name,
            contract: CONTRACT,
            schema_revision: admitted.descriptor.schema_revision,
            fingerprint: admitted.fingerprint.clone(),
            command_id: command_id.clone(),
            accepted_at_ms: accepted_at_ms.clone(),
            operation_id: decided.operation_id.clone(),
            decision: decided.decision.clone(),
        };
        let reply = match self.receipts.commit(&admitted.identity, new) {
            Ok(CommitOutcome::Committed { committed_sequence }) => CommandReply::Recorded {
                protocol: Protocol::Draft1,
                receipt: Receipt {
                    command_id,
                    committed_sequence,
                    accepted_at_ms,
                    request_digest: admitted.fingerprint.as_str().to_owned(),
                    operation_id: decided.operation_id,
                    decision: decided.decision,
                },
                replayed: false,
            },
            Ok(CommitOutcome::AlreadyRecorded(stored)) => {
                self.replay(stored, &admitted.fingerprint, request_key)
            }
            Err(ReceiptStoreError::Unavailable) => self.rejected(
                request_key,
                Rejection::StorageUnavailable,
                RetryAdvice::SameCommand,
            ),
            Err(ReceiptStoreError::AcknowledgmentLost) => CommandReply::CommandStatusUnknown {
                protocol: Protocol::Draft1,
                request_key,
                correlation_id: self.correlation(),
            },
        };
        validate_command_reply(&reply).map_err(GatewayFailure::HandlerContract)?;
        Ok(reply)
    }

    /// Equal fingerprint replays the historical decision without redispatching anything; a
    /// different fingerprint under the same identity is an `IdempotencyConflict` with no effects.
    fn replay(
        &self,
        stored: StoredReceipt,
        fingerprint: &Fingerprint,
        request_key: RequestKey,
    ) -> DomainCommandReply {
        if &stored.fingerprint == fingerprint && stored.contract == CONTRACT {
            CommandReply::Recorded {
                protocol: Protocol::Draft1,
                receipt: stored.receipt,
                replayed: true,
            }
        } else {
            self.rejected(
                request_key,
                Rejection::IdempotencyConflict,
                RetryAdvice::RefreshAndNewRequest,
            )
        }
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

/// Rejects decisions that would publish a different operation's result, an `Accepted` without
/// its durable operation, or an unknown external outcome without reconciliation identity.
fn check_decision(descriptor: &OperationDescriptor, decided: &Decided) -> Result<(), String> {
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
