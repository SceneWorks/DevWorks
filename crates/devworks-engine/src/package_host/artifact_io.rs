//! Artifact bytes over `broker.call`: an invocation reads the artifacts it was given (its input
//! and the broker's results for it) in bounded chunks, and writes new artifacts the same way.
//!
//! The extension protocol carries only artifact references, so bytes move in bounded chunks of
//! the broker's own JSON-RPC frames (base64url, at most [`ARTIFACT_CHUNK_BYTES`] decoded each):
//!
//! * `ArtifactRead { artifact, offset }` answers one chunk from `offset` and says when it ends. The
//!   artifact must be the invocation's, and at most [`MAX_ARTIFACT`] bytes long.
//! * `ArtifactAppend { writeId, data }` adds a chunk to a pending write the invocation names; the
//!   first append opens it. An invocation holds at most [`MAX_PENDING`] pending writes of at most
//!   [`MAX_ARTIFACT`] bytes each, kept by the host and dropped with the invocation.
//! * `ArtifactPublish { writeId, mediaType, sha256 }` publishes the pending write, when its bytes
//!   hash to `sha256`, as a new artifact owned by the invoking principal (the host when there is
//!   none) in its scope. The artifact becomes one of the invocation's, so its result may name it.
use super::broker::host_failure;
use super::session::Slot;
use super::session::rpc_error;
use super::{PackageHost, SlotContext};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::http::{base64url, base64url_decode};
use devworks_core::ports::storage::{ArtifactScope, StorageError, WriteOptions, run_write};
use devworks_core::protocol::provider::ArtifactRef as HostArtifact;
use devworks_core::services::artifacts::{host::HostArtifactPublication, version_ref};
use devworks_extension_protocol::package::{
    ARTIFACT_CHUNK_BYTES, RpcError, RpcErrorClass, RpcResult,
};
use devworks_extension_protocol::scalars as wire;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Largest artifact an invocation reads or writes through the broker.
pub const MAX_ARTIFACT: usize = 8 * 1024 * 1024;
/// Most pending writes one invocation holds at once.
pub const MAX_PENDING: usize = 4;

fn denied(message: &str) -> RpcError {
    rpc_error(RpcErrorClass::PermissionDenied, message)
}

/// Answers one chunk of an artifact of the invocation.
pub(super) fn read(
    host: &PackageHost,
    ctx: &SlotContext,
    artifact: &wire::ArtifactRef,
    offset: Option<&wire::Counter>,
) -> Result<RpcResult, RpcError> {
    if !ctx.in_scope(artifact) {
        return Err(denied("the artifact is not an artifact of this invocation"));
    }
    let Some(ports) = host.broker.get() else {
        return Err(rpc_error(
            RpcErrorClass::Unavailable,
            "artifact reads are not composed in this host",
        ));
    };
    let offset = offset.map_or(Ok(0), |o| String::from(o.clone()).parse::<usize>());
    let Ok(offset) = offset else {
        return Err(rpc_error(
            RpcErrorClass::ProtocolViolation,
            "invalid offset",
        ));
    };
    let reference: HostArtifact = serde_json::to_value(artifact)
        .and_then(serde_json::from_value)
        .map_err(|_| rpc_error(RpcErrorClass::ProtocolViolation, "invalid artifact"))?;
    let reader = ctx
        .grants
        .principal()
        .cloned()
        .unwrap_or_else(|| host.host_ctx());
    let bytes = ports
        .artifacts
        .read_exact(&*host.store, &reader, &reference, MAX_ARTIFACT as u64)
        .map_err(|_| denied("the artifact is not readable or is larger than the bound"))?
        .0;
    if offset > bytes.len() {
        return Err(rpc_error(
            RpcErrorClass::ProtocolViolation,
            "the offset is past the artifact's end",
        ));
    }
    let end = (offset + ARTIFACT_CHUNK_BYTES).min(bytes.len());
    Ok(RpcResult::ArtifactChunk {
        artifact: artifact.clone(),
        offset: wire::Counter::try_from(offset.to_string())
            .map_err(|_| rpc_error(RpcErrorClass::ProtocolViolation, "invalid offset"))?,
        data: base64url(&bytes[offset..end]),
        eof: end == bytes.len(),
    })
}

/// The invocation's pending writes and the artifacts its publishes produced, by action key.
#[derive(Default)]
pub(super) struct Writes {
    pending: BTreeMap<String, PendingWrite>,
    published: BTreeMap<String, wire::ArtifactRef>,
}
#[derive(Default)]
struct PendingWrite {
    bytes: Vec<u8>,
    /// Each append's action key with its chunk's digest and the length it acknowledged: an
    /// identical retry is answered again unchanged, a different chunk under the key refused.
    actions: BTreeMap<String, (String, usize)>,
}

/// Adds a chunk to the invocation's pending write `write_id`, once per action key.
pub(super) fn append(
    ctx: &SlotContext,
    action_key: &str,
    write_id: &str,
    data: &str,
) -> Result<RpcResult, RpcError> {
    let Some(chunk) = base64url_decode(data).filter(|c| c.len() <= ARTIFACT_CHUNK_BYTES) else {
        return Err(rpc_error(
            RpcErrorClass::ProtocolViolation,
            "the chunk is not base64url of at most one chunk's bytes",
        ));
    };
    let mut writes = ctx.writes.lock().unwrap_or_else(|p| p.into_inner());
    if !writes.pending.contains_key(write_id) && writes.pending.len() >= MAX_PENDING {
        return Err(denied(
            "the invocation already holds its most pending writes",
        ));
    }
    let pending = writes.pending.entry(write_id.to_owned()).or_default();
    let digest = sha256_hex(&chunk);
    let length = match pending.actions.get(action_key) {
        // A retried append (its answer was lost): the original answer, nothing appended again.
        Some((seen, length)) if *seen == digest => *length,
        Some(_) => {
            return Err(rpc_error(
                RpcErrorClass::Conflict,
                "a different chunk was already appended under this action key",
            ));
        }
        None => {
            if pending.bytes.len() + chunk.len() > MAX_ARTIFACT {
                return Err(denied(
                    "the write exceeds the largest artifact a package may write",
                ));
            }
            pending.bytes.extend_from_slice(&chunk);
            let length = pending.bytes.len();
            pending
                .actions
                .insert(action_key.to_owned(), (digest, length));
            length
        }
    };
    Ok(RpcResult::ArtifactAppended {
        write_id: wire::Id::try_from(write_id.to_owned())
            .map_err(|_| rpc_error(RpcErrorClass::ProtocolViolation, "invalid write ID"))?,
        byte_length: wire::Counter::try_from(length.to_string())
            .map_err(|_| rpc_error(RpcErrorClass::ProtocolViolation, "invalid length"))?,
    })
}

/// Publishes the pending write `write_id` as a new artifact of the invocation; a retried publish
/// under the same action key answers the artifact it produced. The publication commits only while
/// the invocation is live (serialized with its revocation).
pub(super) fn publish(
    host: &PackageHost,
    ctx: &SlotContext,
    slot: &Arc<Slot>,
    action_key: &str,
    write_id: &str,
    media_type: &str,
    sha256: &wire::Digest,
) -> Result<RpcResult, RpcError> {
    if let Some(artifact) = ctx
        .writes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .published
        .get(action_key)
    {
        return Ok(RpcResult::BrokerCompleted {
            artifact: artifact.clone(),
        });
    }
    let Some(ports) = host.broker.get() else {
        return Err(rpc_error(
            RpcErrorClass::Unavailable,
            "artifact writes are not composed in this host",
        ));
    };
    let bytes = {
        let writes = ctx.writes.lock().unwrap_or_else(|p| p.into_inner());
        let Some(pending) = writes.pending.get(write_id) else {
            return Err(rpc_error(
                RpcErrorClass::ProtocolViolation,
                "no pending write of this ID",
            ));
        };
        if sha256_hex(&pending.bytes) != String::from(sha256.clone()) {
            return Err(rpc_error(
                RpcErrorClass::ProtocolViolation,
                "the written bytes do not hash to the digest",
            ));
        }
        pending.bytes.clone()
    };
    let owner = ctx
        .grants
        .principal()
        .cloned()
        .unwrap_or_else(|| host.host_ctx());
    let provenance = json!({"kind": "PackageArtifact", "packageId": ctx.package_id,
        "invocationOperationId": ctx.operation_id});
    let failed = |e: &dyn std::fmt::Debug| {
        host_failure(
            host,
            &ctx.package_id,
            RpcErrorClass::Unavailable,
            "the artifact could not be published",
            e,
        )
    };
    let staged = ports
        .artifacts
        .stage_host_bytes(&*host.store, &bytes, provenance.clone())
        .map_err(|e| failed(&e))?
        .map_err(|e| failed(&e))?;
    let (artifacts, pending, media_type) = (
        ports.artifacts.clone(),
        staged.clone(),
        media_type.to_owned(),
    );
    let scope = owner
        .project_id()
        .cloned()
        .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
    let principal = owner.principal().clone();
    let live = slot.clone();
    let committed = run_write(&*host.store, WriteOptions::observed(), move |tx| {
        // Serialized with revocation: a cancelled or expired invocation publishes nothing.
        let _guard = live.state.lock().unwrap_or_else(|p| p.into_inner());
        if live.revoked.load(Ordering::SeqCst) {
            return Err(StorageError::ScopeDenied);
        }
        let version = artifacts.commit_host_bytes_in_scope(
            tx,
            &pending,
            HostArtifactPublication {
                title: "Package artifact",
                media_type: &media_type,
                provenance,
                scope,
                owner: principal,
            },
        )?;
        Ok(version_ref(&version))
    });
    ports
        .artifacts
        .settle_host_bytes(&*host.store, &staged, committed.is_ok());
    let produced = match committed {
        Ok(produced) => produced,
        Err(StorageError::ScopeDenied) => {
            return Err(denied("the invocation is no longer live"));
        }
        Err(e) => return Err(failed(&e)),
    };
    let produced: wire::ArtifactRef = serde_json::to_value(&produced)
        .and_then(serde_json::from_value)
        .map_err(|_| rpc_error(RpcErrorClass::Unavailable, "invalid artifact"))?;
    {
        let mut writes = ctx.writes.lock().unwrap_or_else(|p| p.into_inner());
        writes.pending.remove(write_id);
        writes
            .published
            .insert(action_key.to_owned(), produced.clone());
    }
    ctx.admit_artifact(&produced);
    Ok(RpcResult::BrokerCompleted { artifact: produced })
}
