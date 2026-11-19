//! `broker.call`: what one invocation may call (its [`Grants`]) and the brokered application
//! operation path.
//!
//! # Grants
//!
//! The host resolves an invocation's grants at admission ([`Grants::resolve`]) from the running
//! version's approved manifest and the invoking caller; nothing the package sends changes them,
//! and every `broker.call` is checked against them:
//!
//! * **Application operations** run as the *invoking principal* (an agent's tool call runs as
//!   that agent), through the ordinary gateway admission, so a package never gets more authority
//!   than the principal that invoked it. An invocation without an invoking principal (an event
//!   handler, a host-started invocation) calls none. A read-only query is granted to every
//!   contribution; a command or any other effect only to a tool whose approved manifest declares
//!   a `Mutation` or `Opaque` effect (a `ReadCandidate` tool only reads). When the caller names
//!   allowed operations, the call must also be one of them (a list only narrows).
//! * **HTTP** needs the approved `network:<endpoint>` access; **inference** the approved
//!   `inference` access.
//! * **Artifacts** an invocation may read (as arguments) or return are its own: its input and the
//!   results the broker produced for it.
//!
//! # Application calls
//!
//! The arguments artifact must be in the invocation's scope and is read as the invoking
//! principal; the input schema must be the operation's registered schema. The host frames the
//! request under a stable identity (the invocation's durable operation and the call's action
//! key), so a retried command finds its original receipt. Each admitted call is journaled as a
//! `package.broker.application` operation whose history names the package's own actor; the typed
//! reply (a result or a rejection) is published as an artifact owned by the invoking principal
//! and returned as `BrokerCompleted`.
use super::session::{Slot, rpc_error};
use super::{PackageHost, SlotContext};
use crate::providers::schemas::{builtin_operation, schema_ref};
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::gateway::{CommandOutcome, GatewayFailure, QueryOutcome};
use devworks_core::locks::LockExt;
use devworks_core::ports::IdKind;
use devworks_core::ports::packages::Inspection;
use devworks_core::ports::storage::{
    ArtifactScope, AttemptBinding, Cancellation, NewEvent, NewOperation, OperationRecord,
    StorageError, WriteOptions, WriteTransaction, run_write,
};
use devworks_core::protocol::provider::ArtifactRef as HostArtifact;
use devworks_core::protocol::results::DomainQueryReply;
use devworks_core::protocol::{ActorId, CommandReply, EventId, Id, OperationId, OperationState};
use devworks_core::registry::{self, EffectClass, Mode, OperationDescriptor, SchemaRevision};
use devworks_core::services::artifacts::{
    ArtifactService, host::HostArtifactPublication, version_ref,
};
use devworks_extension_protocol::package::{
    BrokerCall, BrokerInvoke, RpcError, RpcErrorClass, RpcResult,
};
use devworks_extension_protocol::scalars as wire;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::{Arc, Weak};

/// The journal capability of one brokered application call.
pub const APPLICATION_CAPABILITY: &str = "package.broker.application";
/// Largest arguments artifact a brokered call reads.
const MAX_ARGUMENTS: u64 = 8 * 1024 * 1024;

/// Runs a command's final transactional decision (the closure) under the caller's fence;
/// `None` when the fence refused it, so nothing was decided.
pub type Fence<'a> = &'a dyn Fn(&mut dyn FnMut() -> CommandOutcome) -> Option<CommandOutcome>;

/// The gateway the broker admits application calls through (the runtime's command path).
pub trait GatewayCalls: Send + Sync {
    fn query(&self, ctx: &InvocationContext, operation: &str, raw: &[u8]) -> QueryOutcome;
    /// Admits a command whose final transactional decision runs inside `fence` (`None` when the
    /// fence refused it). Preparation that may wait (a retained receive) runs outside the fence
    /// and stops early once `cancellation` is cancelled.
    fn command(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
        cancellation: Cancellation,
        fence: Fence<'_>,
    ) -> Option<CommandOutcome>;
}

/// What the broker needs besides the package host, attached once by the runtime at boot. The
/// gateway is held weakly: the package host never keeps the runtime's command path alive.
pub struct BrokerPorts {
    pub gateway: Weak<dyn GatewayCalls>,
    pub artifacts: Arc<ArtifactService>,
}

/// What one invocation may call through `broker.call` (module documentation).
#[derive(Clone)]
pub struct Grants {
    principal: Option<InvocationContext>,
    /// The caller's allowed operations; `None` narrows nothing.
    operations: Option<BTreeSet<String>>,
    /// The approved manifest declares this tool's effect `Mutation` or `Opaque`.
    commands: bool,
    network: BTreeSet<String>,
    inference: bool,
}
impl Grants {
    /// Resolves the grants of an invocation of `contribution` from the running version's
    /// approved manifest (`inspection`), the invoking principal and the caller's allowed
    /// operations.
    pub(super) fn resolve(
        inspection: &Inspection,
        contribution: &str,
        principal: Option<InvocationContext>,
        allowed: &[wire::QualifiedId],
    ) -> Self {
        let commands = inspection
            .contributions
            .iter()
            .find(|c| c.id == contribution)
            .is_some_and(|c| {
                c.kind == "Tool"
                    && c.descriptor.as_ref().is_some_and(|d| {
                        // Only the verified descriptor counts (its digest is the approved one).
                        devworks_core::jcs::canonicalize(d)
                            .is_ok_and(|s| sha256_hex(s.as_bytes()) == c.contract_digest)
                            && matches!(d["requestedEffect"].as_str(), Some("Mutation" | "Opaque"))
                    })
            });
        Self {
            principal,
            operations: (!allowed.is_empty())
                .then(|| allowed.iter().map(|o| o.as_str().to_owned()).collect()),
            commands,
            network: inspection
                .access
                .iter()
                .filter_map(|a| a.strip_prefix("network:"))
                .map(str::to_owned)
                .collect(),
            inference: inspection.access.iter().any(|a| a == "inference"),
        }
    }
    pub(super) fn principal(&self) -> Option<&InvocationContext> {
        self.principal.as_ref()
    }
    /// Whether the approved manifest grants network access to `endpoint`.
    pub(super) fn network_granted(&self, endpoint: &str) -> bool {
        self.network.contains(endpoint)
    }
}

fn denied(message: &str) -> RpcError {
    rpc_error(RpcErrorClass::PermissionDenied, message)
}

/// A host-side failure as the package sees it: its class and a fixed message. The detail goes
/// to the package's host log (best effort), never to the helper.
pub(super) fn host_failure(
    host: &PackageHost,
    package: &str,
    class: RpcErrorClass,
    message: &str,
    detail: &dyn std::fmt::Debug,
) -> RpcError {
    host.service.record_log(
        package,
        devworks_core::protocol::records::Severity::Warning,
        "PackageBrokerHostFailure",
        message,
        Some(format!("{detail:?}")),
    );
    rpc_error(class, message)
}

/// The identity of an artifact version an invocation may use: `(artifact, version, digest)`.
pub(super) fn artifact_key(a: &wire::ArtifactRef) -> (String, String, String) {
    (
        a.artifact_id.as_str().to_owned(),
        a.version_id.as_str().to_owned(),
        String::from(a.sha256.clone()),
    )
}

/// Answers one `broker.call` of a live invocation (identity and token already checked).
pub(super) fn call(
    host: &PackageHost,
    ctx: &SlotContext,
    slot: &Arc<Slot>,
    params: &BrokerInvoke,
) -> Result<RpcResult, RpcError> {
    let grants = &ctx.grants;
    match &params.call {
        BrokerCall::Application {
            operation,
            input_schema,
            arguments,
        } => {
            let admitted = admit(grants, operation.as_str(), input_schema)?;
            if !ctx.in_scope(arguments) {
                return Err(denied(
                    "the arguments are not an artifact of this invocation",
                ));
            }
            application(host, ctx, slot, params, admitted, arguments)
        }
        BrokerCall::Http { endpoint_id, .. } => {
            if !grants.network.contains(endpoint_id.as_str()) {
                return Err(denied(
                    "the approved manifest grants no network access to this endpoint",
                ));
            }
            super::http::call(host, ctx, slot, params)
        }
        BrokerCall::Inference { .. } => {
            if !grants.inference {
                return Err(denied("the approved manifest grants no inference access"));
            }
            super::inference::call(host, ctx, slot, params)
        }
        BrokerCall::ArtifactRead { artifact, offset } => {
            super::artifact_io::read(host, ctx, artifact, offset.as_ref())
        }
        BrokerCall::ArtifactAppend { write_id, data } => {
            super::artifact_io::append(ctx, params.action_key.as_str(), write_id.as_str(), data)
        }
        BrokerCall::ArtifactPublish {
            write_id,
            media_type,
            sha256,
        } => super::artifact_io::publish(
            host,
            ctx,
            slot,
            params.action_key.as_str(),
            write_id.as_str(),
            media_type,
            sha256,
        ),
        BrokerCall::ArtifactWrite { .. } => Err(rpc_error(
            RpcErrorClass::Unavailable,
            "brokered artifact writes are not yet active",
        )),
    }
}

/// An application call that passed its grant: who it runs as and the selected binding.
pub(super) struct Admitted<'a> {
    pub(super) principal: &'a InvocationContext,
    pub(super) descriptor: &'static OperationDescriptor,
    pub(super) mode: Mode,
    pub(super) revision: SchemaRevision,
}

/// The binding an application call selects, when the approved manifest grants it.
struct Selected {
    descriptor: &'static OperationDescriptor,
    mode: Mode,
    revision: SchemaRevision,
}

pub(super) fn admit<'a>(
    grants: &'a Grants,
    operation: &str,
    input_schema: &wire::SchemaRef,
) -> Result<Admitted<'a>, RpcError> {
    let Some(principal) = grants.principal() else {
        return Err(denied(
            "this invocation has no invoking principal to run application operations as",
        ));
    };
    let Selected {
        descriptor,
        mode,
        revision,
    } = select(grants, operation, input_schema)?;
    Ok(Admitted {
        principal,
        descriptor,
        mode,
        revision,
    })
}

fn select(
    grants: &Grants,
    operation: &str,
    input_schema: &wire::SchemaRef,
) -> Result<Selected, RpcError> {
    if grants
        .operations
        .as_ref()
        .is_some_and(|allowed| !allowed.contains(operation))
    {
        return Err(denied("the operation is not allowed for this invocation"));
    }
    let Some((name, selected)) = builtin_operation(operation) else {
        return Err(denied("not a host application operation"));
    };
    let Some(descriptor) = registry::find(name) else {
        return Err(denied("not a host application operation"));
    };
    let revision = String::from(input_schema.revision.clone())
        .parse::<u32>()
        .ok()
        .map(SchemaRevision)
        .filter(|r| descriptor.supports(*r))
        .ok_or_else(|| {
            rpc_error(
                RpcErrorClass::ProtocolViolation,
                "the input schema revision is not supported",
            )
        })?;
    let binding = descriptor
        .modes
        .iter()
        .filter(|b| selected.is_none_or(|m| m == b.mode))
        .find(|b| {
            schema_ref(
                descriptor.name,
                b.mode,
                revision,
                &b.input_schema(revision).to_value(),
            )
            .is_ok_and(|r| {
                r.schema_id.as_str() == input_schema.schema_id.as_str()
                    && r.sha256.as_str() == String::from(input_schema.sha256.clone())
            })
        })
        .ok_or_else(|| {
            rpc_error(
                RpcErrorClass::ProtocolViolation,
                "the input schema is not the operation's registered schema",
            )
        })?;
    let reads_only = binding.mode == Mode::Query && binding.effect == EffectClass::ReadOnly;
    if !reads_only && !grants.commands {
        return Err(denied(
            "the approved manifest declares this contribution read-only",
        ));
    }
    Ok(Selected {
        descriptor,
        mode: binding.mode,
        revision,
    })
}

impl Grants {
    /// Whether the approved manifest lets this contribution commit `call` as an effect intent of
    /// `broker.commit_event` (who it runs as is resolved when it executes, see
    /// [`super::intents`]). An application call's arguments must be an artifact of the invocation
    /// (`in_scope`), and an intent never runs a human-only operation. An HTTP call needs the
    /// endpoint's network grant and a body of the invocation (its connection is resolved when it
    /// executes). Inference and artifact writes are refused as the live broker refuses them; an
    /// artifact read is not an effect and is accepted.
    pub(super) fn permits_intent(
        &self,
        call: &BrokerCall,
        in_scope: impl Fn(&wire::ArtifactRef) -> bool,
    ) -> Result<(), RpcError> {
        match call {
            BrokerCall::Application {
                operation,
                input_schema,
                arguments,
            } => {
                let selected = select(self, operation.as_str(), input_schema)?;
                if selected.descriptor.human_only {
                    return Err(denied("a package intent never runs a human-only operation"));
                }
                if !in_scope(arguments) {
                    return Err(denied(
                        "the arguments are not an artifact of this invocation",
                    ));
                }
                Ok(())
            }
            BrokerCall::Http {
                endpoint_id, body, ..
            } => {
                if !self.network.contains(endpoint_id.as_str()) {
                    return Err(denied(
                        "the approved manifest grants no network access to this endpoint",
                    ));
                }
                if body.as_ref().is_some_and(|b| !in_scope(b)) {
                    return Err(denied("the body is not an artifact of this invocation"));
                }
                Ok(())
            }
            BrokerCall::Inference { .. } => {
                if !self.inference {
                    return Err(denied("the approved manifest grants no inference access"));
                }
                Err(rpc_error(
                    RpcErrorClass::Unavailable,
                    "brokered inference is not yet active",
                ))
            }
            BrokerCall::ArtifactRead { .. } => Ok(()),
            BrokerCall::ArtifactWrite { .. } => Err(rpc_error(
                RpcErrorClass::Unavailable,
                "brokered artifact writes are not yet active",
            )),
            // An invocation's writes are its own, not intents of a commit.
            BrokerCall::ArtifactAppend { .. } | BrokerCall::ArtifactPublish { .. } => {
                Err(rpc_error(
                    RpcErrorClass::Unavailable,
                    "artifact writes are not event intents",
                ))
            }
        }
    }
}

/// The package's own actor (attribution of the calls it makes).
pub(super) fn package_actor(package: &str) -> ActorId {
    let id = format!("package-{package}");
    let id = if id.len() <= 128 {
        id
    } else {
        format!("package-{}", &sha256_hex(package.as_bytes())[..64])
    };
    ActorId(Id::new(id).expect("bounded package actor"))
}

/// How a brokered call ended in the journal.
pub(super) enum Ended {
    Answered(OperationState, Vec<u8>),
    /// The package's error (a class and a fixed message), the journal state and the host-only
    /// detail recorded in the journal.
    Failed(RpcError, OperationState, String),
}

fn application(
    host: &PackageHost,
    ctx: &SlotContext,
    slot: &Arc<Slot>,
    params: &BrokerInvoke,
    admitted: Admitted<'_>,
    arguments: &wire::ArtifactRef,
) -> Result<RpcResult, RpcError> {
    let Some(ports) = host.broker.get() else {
        return Err(rpc_error(
            RpcErrorClass::Unavailable,
            "application calls are not composed in this host",
        ));
    };
    let Some(gateway) = ports.gateway.upgrade() else {
        return Err(rpc_error(
            RpcErrorClass::Unavailable,
            "the application stopped",
        ));
    };
    let principal = admitted.principal;
    let reference: HostArtifact = serde_json::to_value(arguments)
        .and_then(serde_json::from_value)
        .map_err(|_| {
            rpc_error(
                RpcErrorClass::ProtocolViolation,
                "invalid arguments artifact",
            )
        })?;
    let bytes = ports
        .artifacts
        .read_exact(&*host.store, principal, &reference, MAX_ARGUMENTS)
        .map_err(|_| denied("the arguments artifact is not readable by the invoking principal"))?
        .0;
    let args = devworks_core::framing::parse(&bytes).map_err(|_| {
        rpc_error(
            RpcErrorClass::ProtocolViolation,
            "the arguments are not a JSON object",
        )
    })?;
    let action = params.action_key.as_str();
    let identity = sha256_hex(format!("{}\0{action}", ctx.operation_id).as_bytes());
    let frame = crate::tool_turn::frame_as(
        format!("pkg-{identity}"),
        admitted.descriptor,
        admitted.mode,
        admitted.revision,
        args,
    )
    .map_err(|e| rpc_error(RpcErrorClass::ProtocolViolation, e))?;
    let operation = OperationId(Id::new(format!("pkgcall-{identity}")).expect("bounded id"));
    let digest = sha256_hex(&frame);
    journal(
        host,
        ctx,
        slot,
        &Journal {
            operation: &operation,
            capability: APPLICATION_CAPABILITY,
            action,
            name: admitted.descriptor.name,
            digest: &digest,
            detail: json!({}),
        },
    )?;
    // A command's final decision is made under the invocation's revocation lock (as a state
    // compare-and-set is): a cancelled or expired invocation never applies one afterwards. Its
    // preparation (a retained receive's wait) runs outside the lock and stops when the
    // invocation is revoked, so the revocation and `host.cancel` are never held behind it.
    let ended = decide(
        &*gateway,
        &admitted,
        &frame,
        slot.cancellation.clone(),
        &|decide| {
            let _guard = slot.state.locked();
            if slot.revoked.load(std::sync::atomic::Ordering::SeqCst) {
                return None;
            }
            Some(decide())
        },
    );
    match ended {
        Ended::Failed(error, state, detail) => {
            let _ = run_write(&*host.store, WriteOptions::observed(), {
                let operation = operation.clone();
                move |tx| settle(tx, &operation, state, json!({"failed": detail}))
            });
            Err(error)
        }
        Ended::Answered(state, reply) => {
            let produced = publish(
                host,
                &ports.artifacts,
                principal,
                &operation,
                Journaling::Settle(state, json!({})),
                &reply,
            )
            .map_err(|e| {
                host_failure(
                    host,
                    &ctx.package_id,
                    RpcErrorClass::OutcomeUnknown,
                    "the call's reply could not be published",
                    &e,
                )
            })?;
            let produced: wire::ArtifactRef = serde_json::to_value(&produced)
                .and_then(serde_json::from_value)
                .map_err(|_| rpc_error(RpcErrorClass::OutcomeUnknown, "invalid result artifact"))?;
            ctx.admit_artifact(&produced);
            Ok(RpcResult::BrokerCompleted { artifact: produced })
        }
    }
}

/// Runs an admitted application call through the gateway as its principal. A command's final
/// decision runs inside `fence` (`None`: refused, nothing was decided).
pub(super) fn decide(
    gateway: &dyn GatewayCalls,
    admitted: &Admitted<'_>,
    frame: &[u8],
    cancellation: Cancellation,
    fence: Fence<'_>,
) -> Ended {
    let (principal, name) = (admitted.principal, admitted.descriptor.name);
    match admitted.mode {
        Mode::Query => match gateway.query(principal, name, frame) {
            Ok(reply) => {
                let state = match &reply {
                    DomainQueryReply::Result { .. } => OperationState::ObservedComplete,
                    DomainQueryReply::Rejected { .. } => OperationState::Failed,
                };
                Ended::Answered(state, serde_json::to_vec(&reply).unwrap_or_default())
            }
            Err(e) => Ended::Failed(
                rpc_error(RpcErrorClass::Unavailable, "the query could not be run"),
                OperationState::Failed,
                e.to_string(),
            ),
        },
        Mode::Command => match gateway.command(principal, name, frame, cancellation, fence) {
            None => Ended::Failed(
                denied("the invocation is no longer live"),
                OperationState::Failed,
                "the invocation was revoked before the command's decision".into(),
            ),
            Some(Ok(reply @ CommandReply::Recorded { .. })) => Ended::Answered(
                OperationState::ObservedComplete,
                serde_json::to_vec(&reply).unwrap_or_default(),
            ),
            Some(Ok(reply @ CommandReply::Rejected { .. })) => Ended::Answered(
                OperationState::Failed,
                serde_json::to_vec(&reply).unwrap_or_default(),
            ),
            Some(Ok(other)) => Ended::Failed(
                rpc_error(
                    RpcErrorClass::OutcomeUnknown,
                    "the command's status is unknown",
                ),
                OperationState::OutcomeUnknown,
                format!("{other:?}"),
            ),
            // Nothing was admitted or recorded for these.
            Some(Err(
                e @ (GatewayFailure::NoUseCase(_) | GatewayFailure::Unaddressable { .. }),
            )) => Ended::Failed(
                rpc_error(RpcErrorClass::Unavailable, "the command was not admitted"),
                OperationState::Failed,
                e.to_string(),
            ),
            Some(Err(e)) => Ended::Failed(
                rpc_error(
                    RpcErrorClass::OutcomeUnknown,
                    "the command failed before its receipt",
                ),
                OperationState::OutcomeUnknown,
                e.to_string(),
            ),
        },
    }
}

/// One brokered call to journal: its stable operation, journal capability, action key, the
/// operation or endpoint it names, the digest of its arguments and further payload fields.
pub(super) struct Journal<'a> {
    pub(super) operation: &'a OperationId,
    pub(super) capability: &'static str,
    pub(super) action: &'a str,
    pub(super) name: &'a str,
    pub(super) digest: &'a str,
    pub(super) detail: Value,
}

/// Journals an admitted call before it runs: its operation (`Dispatching`) and a history event
/// attributed to the package's actor. A call already journaled under the same identity (a
/// retried invocation) is not journaled again and is returned; a different call under it is a
/// conflict. An application call needs the invoking principal; a call without one is journaled
/// without lineage.
pub(super) fn journal(
    host: &PackageHost,
    ctx: &SlotContext,
    slot: &Arc<Slot>,
    call: &Journal<'_>,
) -> Result<Option<OperationRecord>, RpcError> {
    let principal = ctx.grants.principal().cloned();
    let (operation, capability, action, name, digest, detail) = (
        call.operation.clone(),
        call.capability,
        call.action.to_owned(),
        call.name.to_owned(),
        call.digest.to_owned(),
        call.detail.clone(),
    );
    let (package, invocation, contribution) = (
        ctx.package_id.clone(),
        ctx.operation_id.clone(),
        slot.contribution_id.clone(),
    );
    let (slot, invocation_id) = (slot.clone(), slot.invocation_id.clone());
    let written = run_write(&*host.store, WriteOptions::observed(), move |tx| {
        // Serialized with revocation: a cancelled invocation journals nothing new.
        let _guard = slot.state.locked();
        if slot.revoked.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StorageError::ScopeDenied);
        }
        if let Some(existing) = tx.operations().operation(&operation)? {
            return if existing.capability == capability && existing.args_digest == digest {
                Ok(Some(existing))
            } else {
                Err(StorageError::Duplicate {
                    kind: "package_broker_call",
                    id: operation.0.as_str().to_owned(),
                })
            };
        }
        if capability == APPLICATION_CAPABILITY && principal.is_none() {
            return Err(StorageError::ScopeDenied);
        }
        let actor = package_actor(&package);
        tx.actors()
            .register(&actor, PrincipalKind::Package, &package)?;
        let mut payload = json!({
            "packageId": package, "contributionId": contribution,
            "invocationOperationId": invocation, "invocationId": invocation_id,
            "actionKey": action, "operation": name,
            "principal": principal.as_ref().map(|p| p.principal().clone()),
            "principalKind": principal.as_ref().map(|p| format!("{:?}", p.principal_kind())),
        });
        if let (Some(fields), Value::Object(more)) = (payload.as_object_mut(), detail) {
            fields.extend(more);
        }
        tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: principal.as_ref().and_then(|p| p.root_id().cloned()),
            assignment_id: principal.as_ref().and_then(|p| p.assignment_id().cloned()),
            capability: capability.into(),
            schema_version: 1,
            args_digest: digest.clone(),
            payload,
        })?;
        let attempt = tx
            .operations()
            .begin_attempt(&operation, 1, AttemptBinding::default())?;
        let dispatching = tx.operations().transition(
            &operation,
            2,
            Some(&attempt.fence()),
            OperationState::Dispatching,
            None,
        )?;
        let event = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: event,
            actor,
            project_id: principal.as_ref().and_then(|p| p.project_id().cloned()),
            root_id: principal.as_ref().and_then(|p| p.root_id().cloned()),
            operation_id: Some(operation.clone()),
            aggregate_kind: "package".into(),
            aggregate_id: package.clone(),
            aggregate_revision: dispatching.revision,
            kind: "PackageBrokerCallStarted".into(),
            payload: json!({"operation": name, "actionKey": action,
                "invocationOperationId": invocation}),
        })?;
        Ok(None)
    });
    match written {
        Ok(existing) => Ok(existing),
        Err(StorageError::ScopeDenied) => Err(denied("the invocation is no longer live")),
        Err(StorageError::Duplicate { .. }) => Err(rpc_error(
            RpcErrorClass::Conflict,
            "a different call was already made under this action key",
        )),
        Err(e) => Err(host_failure(
            host,
            &ctx.package_id,
            RpcErrorClass::Unavailable,
            "the call could not be journaled",
            &e,
        )),
    }
}

/// Moves a journaled call that is still `Dispatching` to its outcome (a replayed call's earlier
/// outcome stays).
pub(super) fn settle(
    tx: &mut dyn WriteTransaction,
    operation: &OperationId,
    to: OperationState,
    observation: Value,
) -> Result<(), StorageError> {
    let Some(op) = tx.operations().operation(operation)? else {
        return Ok(());
    };
    if op.state != OperationState::Dispatching {
        return Ok(());
    }
    let fence = op.latest_attempt().map(|a| a.fence());
    tx.operations()
        .transition(&op.id, op.revision, fence.as_ref(), to, Some(observation))?;
    Ok(())
}

/// What [`publish`] records in the call's journal in the transaction that publishes its reply,
/// with the reply artifact as the observation's `result`.
pub(super) enum Journaling {
    /// Nothing (a dispatched intent's outcome is the dispatcher's to record).
    None,
    /// Settles the call with this state and observation.
    Settle(OperationState, Value),
    /// Adds this observation to the call's current attempt; the dispatcher settles it.
    Observe(Value),
}

/// Publishes a call's typed reply as an artifact owned by the invoking principal and records it
/// in the call's journal as `journaling` says, in the same transaction.
pub(super) fn publish(
    host: &PackageHost,
    artifacts: &Arc<ArtifactService>,
    principal: &InvocationContext,
    operation: &OperationId,
    journaling: Journaling,
    reply: &[u8],
) -> Result<HostArtifact, StorageError> {
    let staged = artifacts
        .stage_host_bytes(
            &*host.store,
            reply,
            json!({"kind": "PackageBrokerResult", "operationId": operation}),
        )?
        .map_err(StorageError::InvalidRecord)?;
    let (artifacts_w, staged_w, operation, owner) = (
        artifacts.clone(),
        staged.clone(),
        operation.clone(),
        principal.principal().clone(),
    );
    let scope = principal
        .project_id()
        .cloned()
        .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
    let committed = run_write(&*host.store, WriteOptions::observed(), move |tx| {
        let version = artifacts_w.commit_host_bytes_in_scope(
            tx,
            &staged_w,
            HostArtifactPublication {
                title: "Package broker result",
                media_type: "application/json",
                provenance: json!({"kind": "PackageBrokerResult", "operationId": operation}),
                scope,
                owner,
            },
        )?;
        let produced = version_ref(&version);
        let with_result = |observation: &Value| {
            let mut observation = observation.clone();
            if let Some(fields) = observation.as_object_mut() {
                fields.insert("result".into(), json!(produced));
            }
            observation
        };
        match &journaling {
            Journaling::None => {}
            Journaling::Settle(state, observation) => {
                settle(tx, &operation, *state, with_result(observation))?;
            }
            Journaling::Observe(observation) => {
                if let Some(attempt) = tx
                    .operations()
                    .operation(&operation)?
                    .and_then(|op| op.latest_attempt().map(|a| a.attempt_no))
                {
                    tx.operations()
                        .observe(&operation, attempt, with_result(observation))?;
                }
            }
        }
        Ok(produced)
    });
    artifacts.settle_host_bytes(&*host.store, &staged, committed.is_ok());
    committed
}
