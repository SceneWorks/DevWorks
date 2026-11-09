//! Durable, explicit provider changes. The old artifact and transcript remain immutable.
use devworks_core::{
    context::InvocationContext,
    ports::{IdKind, storage::*},
    protocol::{
        Counter, Id,
        provider::{ArtifactRef, ContextChangeReason, NativeBinding, TranscriptBody},
    },
    providers::context::ProviderContext,
    services::{
        artifacts::{ArtifactService, host::HostArtifactPublication, version_ref},
        sessions::{
            transcript::{
                BatchOutcome, ContextSourceInput, SessionBatch, SessionWriter, SourceDisposition,
            },
            turn,
        },
    },
};
use serde_json::json;
use std::sync::Arc;

pub(crate) fn provider_change(
    store: &dyn ApplicationStore,
    artifacts: &Arc<ArtifactService>,
    ctx: &InvocationContext,
    session_id: &Id,
    previous_context: &ArtifactRef,
    previous_binding: &NativeBinding,
    next_binding: &NativeBinding,
) -> Result<ArtifactRef, StorageError> {
    if ctx.session_id() != Some(session_id) {
        return Err(StorageError::ScopeDenied);
    }
    let previous_epoch = previous_binding.context_epoch.get();
    let next_epoch = next_binding.context_epoch.get();
    if previous_epoch.checked_add(1) != Some(next_epoch) {
        return Err(StorageError::InvalidRecord(
            "nonsequential provider epoch".into(),
        ));
    }
    read_snapshot(store, &ReadOptions::default(), |read| {
        let session = read
            .sessions()
            .session(session_id)?
            .ok_or(StorageError::ScopeDenied)?;
        if session.closed() || session.context_epoch != previous_epoch {
            return Err(StorageError::ScopeDenied);
        }
        Ok(())
    })?;
    let bytes = artifacts
        .read_exact(store, ctx, previous_context, 8 * 1024 * 1024)
        .map_err(|_| StorageError::ScopeDenied)?
        .0;
    let context: ProviderContext =
        serde_json::from_slice(&bytes).map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
    let (converted, omitted) = context
        .converted_for_new_epoch(previous_binding, next_binding)
        .map_err(|e| StorageError::InvalidRecord(format!("provider context conversion: {e:?}")))?;
    if omitted.len() > 128 {
        return Err(StorageError::InvalidRecord(
            "too many omitted native blocks".into(),
        ));
    }
    let bytes =
        serde_json::to_vec(&converted).map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
    let staged = artifacts
        .stage_host_bytes(
            store,
            &bytes,
            json!({"kind":"ProviderContextEpoch","reason":"ProviderChange",
            "sessionId":session_id,"previousEpoch":previous_epoch,"nextEpoch":next_epoch,
            "omittedNative":omitted}),
        )?
        .map_err(StorageError::InvalidRecord)?;
    let staged_for_write = staged.clone();
    let artifacts_for_write = artifacts.clone();
    let ctx_for_write = ctx.clone();
    let session_for_write = session_id.clone();
    let omitted_for_write = omitted.clone();
    let committed = run_write(store, WriteOptions::default(), move |tx| {
        let session = tx
            .sessions()
            .session(&session_for_write)?
            .ok_or(StorageError::ScopeDenied)?;
        if session.closed() || session.context_epoch != previous_epoch {
            return Err(StorageError::ScopeDenied);
        }
        let scope = session
            .project_id
            .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
        let version = artifacts_for_write.commit_host_bytes_in_scope(
            tx,
            &staged_for_write,
            HostArtifactPublication {
                title: "Provider context epoch",
                media_type: "application/vnd.devworks.provider-context+json",
                provenance: json!({"kind":"ProviderContextEpoch","reason":"ProviderChange",
                    "sessionId":session_for_write,"previousEpoch":previous_epoch,
                    "omittedNative":omitted_for_write}),
                scope,
                owner: ctx_for_write.principal().clone(),
            },
        )?;
        let reference = version_ref(&version);
        let batch = turn::batch_of(
            &session_for_write,
            tx.new_id(IdKind::TranscriptBatch),
            next_epoch,
            session.next_sequence,
            vec![TranscriptBody::ContextEpochChanged {
                previous_epoch: Counter::new(previous_epoch)
                    .map_err(|e| StorageError::InvalidRecord(e.into()))?,
                next_epoch: Counter::new(next_epoch)
                    .map_err(|e| StorageError::InvalidRecord(e.into()))?,
                reason: ContextChangeReason::ProviderChange,
                context: reference.clone(),
            }],
            || tx.new_id(IdKind::TranscriptEntry),
        );
        let mut record = SessionBatch::entries(batch);
        record.context_sources = omitted_for_write
            .iter()
            .map(|artifact| ContextSourceInput {
                source_id: artifact.artifact_id.clone(),
                kind: "provider_native".into(),
                source_revision: 1,
                artifact: Some(artifact.clone()),
                token_estimate: 0,
                disposition: SourceDisposition::Omitted,
            })
            .collect();
        match SessionWriter::append_in(tx, &ctx_for_write, &record)? {
            BatchOutcome::Accepted(_) => Ok(reference),
            BatchOutcome::Replayed(_) => Err(StorageError::InvalidRecord(
                "provider context epoch batch unexpectedly replayed".into(),
            )),
            BatchOutcome::Refused(reason) => Err(StorageError::InvalidRecord(format!(
                "provider context epoch refused: {reason:?}"
            ))),
        }
    });
    artifacts.settle_host_bytes(store, &staged, committed.is_ok());
    committed
}
