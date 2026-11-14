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
use devworks_core::ports::IdKind;
use devworks_core::ports::packages::Inspection;
use devworks_core::ports::storage::{
    ArtifactScope, AttemptBinding, NewEvent, NewOperation, StorageError, WriteOptions,
    WriteTransaction, run_write,
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

/// The gateway the broker admits application calls through (the runtime's command path).
pub trait GatewayCalls: Send + Sync {
    fn query(&self, ctx: &InvocationContext, operation: &str, raw: &[u8]) -> QueryOutcome;
    fn command(&self, ctx: &InvocationContext, operation: &str, raw: &[u8]) -> CommandOutcome;
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
}

fn denied(message: &str) -> RpcError {
    rpc_error(RpcErrorClass::PermissionDenied, message)
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
            Err(rpc_error(
                RpcErrorClass::Unavailable,
                "brokered HTTP calls are not yet active",
            ))
        }
        BrokerCall::Inference { .. } => {
            if !grants.inference {
                return Err(denied("the approved manifest grants no inference access"));
            }
            Err(rpc_error(
                RpcErrorClass::Unavailable,
                "brokered inference is not yet active",
            ))
        }
        BrokerCall::ArtifactRead { artifact } => {
            if !ctx.in_scope(artifact) {
                return Err(denied("the artifact is not an artifact of this invocation"));
            }
            Err(rpc_error(
                RpcErrorClass::Unavailable,
                "brokered artifact reads are not yet active",
            ))
        }
        BrokerCall::ArtifactWrite { .. } => Err(rpc_error(
            RpcErrorClass::Unavailable,
            "brokered artifact writes are not yet active",
        )),
    }
}

/// An application call that passed its grant: who it runs as and the selected binding.
struct Admitted<'a> {
    principal: &'a InvocationContext,
    descriptor: &'static OperationDescriptor,
    mode: Mode,
    revision: SchemaRevision,
}

fn admit<'a>(
    grants: &'a Grants,
    operation: &str,
    input_schema: &wire::SchemaRef,
) -> Result<Admitted<'a>, RpcError> {
    let Some(principal) = grants.principal() else {
        return Err(denied(
            "this invocation has no invoking principal to run application operations as",
        ));
    };
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
    Ok(Admitted {
        principal,
        descriptor,
        mode: binding.mode,
        revision,
    })
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
enum Ended {
    Answered(OperationState, Vec<u8>),
    Failed(RpcError, OperationState),
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
        &operation,
        action,
        admitted.descriptor.name,
        &digest,
    )?;
    let ended = match admitted.mode {
        Mode::Query => match gateway.query(principal, admitted.descriptor.name, &frame) {
            Ok(reply) => {
                let state = match &reply {
                    DomainQueryReply::Result { .. } => OperationState::ObservedComplete,
                    DomainQueryReply::Rejected { .. } => OperationState::Failed,
                };
                Ended::Answered(state, serde_json::to_vec(&reply).unwrap_or_default())
            }
            Err(e) => Ended::Failed(
                rpc_error(
                    RpcErrorClass::Unavailable,
                    &format!("the query failed: {e:?}"),
                ),
                OperationState::Failed,
            ),
        },
        // A command is admitted and committed under the invocation's revocation lock (as a state
        // compare-and-set is): a cancelled or expired invocation never applies one afterwards.
        Mode::Command => {
            match fenced_command(&*gateway, slot, principal, admitted.descriptor.name, &frame) {
                None => Ended::Failed(
                    denied("the invocation is no longer live"),
                    OperationState::Failed,
                ),
                Some(outcome) => match outcome {
                    Ok(reply @ CommandReply::Recorded { .. }) => Ended::Answered(
                        OperationState::ObservedComplete,
                        serde_json::to_vec(&reply).unwrap_or_default(),
                    ),
                    Ok(reply @ CommandReply::Rejected { .. }) => Ended::Answered(
                        OperationState::Failed,
                        serde_json::to_vec(&reply).unwrap_or_default(),
                    ),
                    Ok(other) => Ended::Failed(
                        rpc_error(
                            RpcErrorClass::OutcomeUnknown,
                            &format!("the command's status is unknown: {other:?}"),
                        ),
                        OperationState::OutcomeUnknown,
                    ),
                    // Nothing was admitted or recorded for these.
                    Err(
                        e @ (GatewayFailure::NoUseCase(_) | GatewayFailure::Unaddressable { .. }),
                    ) => Ended::Failed(
                        rpc_error(
                            RpcErrorClass::Unavailable,
                            &format!("the command was not admitted: {e}"),
                        ),
                        OperationState::Failed,
                    ),
                    Err(e) => Ended::Failed(
                        rpc_error(
                            RpcErrorClass::OutcomeUnknown,
                            &format!("the command failed before its receipt: {e}"),
                        ),
                        OperationState::OutcomeUnknown,
                    ),
                },
            }
        }
    };
    match ended {
        Ended::Failed(error, state) => {
            let _ = run_write(&*host.store, WriteOptions::default(), {
                let (operation, note) = (operation.clone(), error.message.clone());
                move |tx| settle(tx, &operation, state, json!({"failed": note}))
            });
            Err(error)
        }
        Ended::Answered(state, reply) => {
            let produced = publish(host, &ports.artifacts, principal, &operation, state, &reply)
                .map_err(|e| rpc_error(RpcErrorClass::OutcomeUnknown, &format!("{e:?}")))?;
            let produced: wire::ArtifactRef = serde_json::to_value(&produced)
                .and_then(serde_json::from_value)
                .map_err(|_| rpc_error(RpcErrorClass::OutcomeUnknown, "invalid result artifact"))?;
            ctx.admit_artifact(&produced);
            Ok(RpcResult::BrokerCompleted { artifact: produced })
        }
    }
}

/// Runs a command while holding the invocation's revocation lock; `None` when it was already
/// revoked (nothing ran).
fn fenced_command(
    gateway: &dyn GatewayCalls,
    slot: &Slot,
    principal: &InvocationContext,
    operation: &str,
    frame: &[u8],
) -> Option<CommandOutcome> {
    let _guard = slot.state.lock().unwrap_or_else(|p| p.into_inner());
    if slot.revoked.load(std::sync::atomic::Ordering::SeqCst) {
        return None;
    }
    Some(gateway.command(principal, operation, frame))
}

/// Journals an admitted call before it runs: its operation (`Dispatching`) and a history event
/// attributed to the package's actor. A call already journaled under the same identity (a
/// retried invocation) is not journaled again; a different call under it is a conflict.
fn journal(
    host: &PackageHost,
    ctx: &SlotContext,
    slot: &Arc<Slot>,
    operation: &OperationId,
    action: &str,
    name: &str,
    digest: &str,
) -> Result<(), RpcError> {
    let principal = ctx.grants.principal().cloned();
    let (operation, action, name, digest) = (
        operation.clone(),
        action.to_owned(),
        name.to_owned(),
        digest.to_owned(),
    );
    let (package, invocation, contribution) = (
        ctx.package_id.clone(),
        ctx.operation_id.clone(),
        slot.contribution_id.clone(),
    );
    let slot = slot.clone();
    let written = run_write(&*host.store, WriteOptions::default(), move |tx| {
        // Serialized with revocation: a cancelled invocation journals nothing new.
        let _guard = slot.state.lock().unwrap_or_else(|p| p.into_inner());
        if slot.revoked.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StorageError::ScopeDenied);
        }
        if let Some(existing) = tx.operations().operation(&operation)? {
            return if existing.capability == APPLICATION_CAPABILITY
                && existing.args_digest == digest
            {
                Ok(())
            } else {
                Err(StorageError::Duplicate {
                    kind: "package_broker_call",
                    id: operation.0.as_str().to_owned(),
                })
            };
        }
        let principal = principal.as_ref().ok_or(StorageError::ScopeDenied)?;
        let actor = package_actor(&package);
        tx.actors()
            .register(&actor, PrincipalKind::Package, &package)?;
        tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: principal.root_id().cloned(),
            assignment_id: principal.assignment_id().cloned(),
            capability: APPLICATION_CAPABILITY.into(),
            schema_version: 1,
            args_digest: digest.clone(),
            payload: json!({
                "packageId": package, "contributionId": contribution,
                "invocationOperationId": invocation, "actionKey": action, "operation": name,
                "principal": principal.principal(),
                "principalKind": format!("{:?}", principal.principal_kind()),
            }),
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
            project_id: principal.project_id().cloned(),
            root_id: principal.root_id().cloned(),
            operation_id: Some(operation.clone()),
            aggregate_kind: "package".into(),
            aggregate_id: package.clone(),
            aggregate_revision: dispatching.revision,
            kind: "PackageBrokerCallStarted".into(),
            payload: json!({"operation": name, "actionKey": action,
                "invocationOperationId": invocation}),
        })?;
        Ok(())
    });
    match written {
        Ok(()) => Ok(()),
        Err(StorageError::ScopeDenied) => Err(denied("the invocation is no longer live")),
        Err(StorageError::Duplicate { .. }) => Err(rpc_error(
            RpcErrorClass::Conflict,
            "a different call was already made under this action key",
        )),
        Err(e) => Err(rpc_error(RpcErrorClass::Unavailable, &format!("{e:?}"))),
    }
}

/// Moves a journaled call that is still `Dispatching` to its outcome (a replayed call's earlier
/// outcome stays).
fn settle(
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

/// Publishes a call's typed reply as an artifact owned by the invoking principal and settles its
/// journal in the same transaction.
fn publish(
    host: &PackageHost,
    artifacts: &Arc<ArtifactService>,
    principal: &InvocationContext,
    operation: &OperationId,
    state: OperationState,
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
    let committed = run_write(&*host.store, WriteOptions::default(), move |tx| {
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
        settle(tx, &operation, state, json!({"result": produced}))?;
        Ok(produced)
    });
    artifacts.settle_host_bytes(&*host.store, &staged, committed.is_ok());
    committed
}
