use super::{PackageHost, SlotContext, session::rpc_error};
use devworks_core::context::InvocationContext;
use devworks_core::framing;
use devworks_core::ports::storage::{StorageError, WriteTransaction};
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::services::artifacts::exact_reference_readable_in;
use devworks_extension_protocol::package::{RpcError, RpcErrorClass};
use devworks_extension_protocol::scalars as wire;
use serde_json::{Value, json};

pub(super) struct Prepared {
    reference: ArtifactRef,
    principal: InvocationContext,
}

impl Prepared {
    pub(super) fn readable_in(&self, tx: &mut dyn WriteTransaction) -> Result<bool, StorageError> {
        exact_reference_readable_in(tx, &self.principal, &self.reference)
    }

    pub(super) fn state(&self) -> Value {
        json!({"artifact": self.reference})
    }
}

/// Reads immutable, verified bytes before the write transaction. The write owner rechecks
/// current visibility and liveness before publishing the reference.
pub(super) fn prepare(
    host: &PackageHost,
    ctx: &SlotContext,
    proposal: &wire::ArtifactRef,
) -> Result<Prepared, RpcError> {
    if !ctx.in_scope(proposal) {
        return Err(rpc_error(
            RpcErrorClass::PermissionDenied,
            "the state proposal is not an artifact of this invocation",
        ));
    }
    let unavailable = || {
        rpc_error(
            RpcErrorClass::Unavailable,
            "the verified package state schema is unavailable",
        )
    };
    let file = ctx.state_document.as_ref().ok_or_else(unavailable)?;
    let bound = super::artifact_io::MAX_ARTIFACT as u64;
    let bytes = host
        .service
        .files()
        .read_verified(&ctx.package_id, &ctx.manifest_digest, file, bound)
        .map_err(|_| unavailable())?;
    let schema = framing::parse(&bytes).map_err(|_| unavailable())?;
    let reference = serde_json::to_value(proposal)
        .and_then(serde_json::from_value)
        .map_err(|_| rpc_error(RpcErrorClass::ProtocolViolation, "invalid state proposal"))?;
    let principal = ctx
        .grants
        .principal()
        .cloned()
        .unwrap_or_else(|| host.host_ctx());
    let ports = host.broker.get().ok_or_else(|| {
        rpc_error(
            RpcErrorClass::Unavailable,
            "artifact reads are not composed",
        )
    })?;
    let (bytes, _) = ports
        .artifacts
        .read_exact(&*host.store, &principal, &reference, bound)
        .map_err(|_| {
            rpc_error(
                RpcErrorClass::PermissionDenied,
                "the state proposal is unreadable",
            )
        })?;
    let invalid = || {
        rpc_error(
            RpcErrorClass::ProtocolViolation,
            "the proposal does not match the package state schema",
        )
    };
    let value = framing::parse(&bytes).map_err(|_| invalid())?;
    host.service
        .validator()
        .validate(&schema, &value)
        .map_err(|_| invalid())?;
    Ok(Prepared {
        reference,
        principal,
    })
}
