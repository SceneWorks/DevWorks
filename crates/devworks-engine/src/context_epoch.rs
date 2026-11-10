//! Durable, explicit context changes. The old artifact and transcript remain immutable.
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

/// Constant-size host witness carried by derived contexts; never reconstruct ancestry by
/// scanning an unbounded chain of previous turns.
pub(crate) fn epoch_root(
    sessions: &dyn SessionRead,
    session_id: &Id,
    epoch: i64,
) -> Result<serde_json::Value, StorageError> {
    if epoch == 0 {
        return Ok(serde_json::Value::Null);
    }
    Ok(sessions
        .epoch(session_id, epoch)?
        .ok_or(StorageError::ScopeDenied)?
        .context)
}

/// Only the immutable first version of a host-published continuation can carry an epoch
/// witness. User-created copies, later edited versions and foreign bindings are not descendants.
#[allow(clippy::too_many_arguments)]
pub(crate) fn verify_descendant(
    store: &dyn ApplicationStore,
    artifacts: &ArtifactService,
    ctx: &InvocationContext,
    session_id: &Id,
    reference: &ArtifactRef,
    root: &ArtifactRef,
    epoch: i64,
    connection_id: &Id,
    model_id: &Id,
    profile: &str,
) -> Result<(), StorageError> {
    let (_, provenance) = artifacts
        .read_exact(store, ctx, reference, 8 * 1024 * 1024)
        .map_err(|_| StorageError::ScopeDenied)?;
    read_snapshot(store, &ReadOptions::default(), |read| {
        let version = read
            .artifacts()
            .version(&reference.version_id)?
            .ok_or(StorageError::ScopeDenied)?;
        let session = read
            .sessions()
            .session(session_id)?
            .ok_or(StorageError::ScopeDenied)?;
        let derived = &provenance["derived"];
        let binding: NativeBinding = serde_json::from_value(derived["binding"].clone())
            .map_err(|_| StorageError::ScopeDenied)?;
        let host = store.info().host_actor.0;
        if !version.live()
            || version.version_no != 1
            || version.source["principalKind"] != "Host"
            || version.source["producedBy"] != host.as_str()
            || provenance["principalKind"] != "Host"
            || provenance["producedBy"] != host.as_str()
            || !matches!(
                derived["kind"].as_str(),
                Some("ProviderTurnContinuation" | "ProviderOperationUpdateContext")
            )
            || derived["sessionId"] != session_id.as_str()
            || derived["epochRoot"] != json!(root)
            || !derived["observedSequence"]
                .as_i64()
                .is_some_and(|sequence| {
                    sequence >= 0 && sequence <= session.last_durable_sequence()
                })
            || ctx.session_id() != Some(session_id)
            || binding.context_epoch.get() != epoch
            || binding.connection_id != *connection_id
            || binding.model_id != *model_id
            || format!("{:?}", binding.endpoint_profile) != profile
        {
            return Err(StorageError::ScopeDenied);
        }
        Ok(())
    })
}

pub(crate) struct EpochChange {
    pub previous_epoch: i64,
    pub binding: NativeBinding,
    pub observed_sequence: Option<i64>,
    pub source: ArtifactRef,
    pub reason: ContextChangeReason,
    pub context: ProviderContext,
    pub omitted: Vec<ArtifactRef>,
    /// A `SkillRevisionsAdopted` session event recorded atomically with the new epoch.
    pub adoption: Option<serde_json::Value>,
}

pub(crate) struct EpochSource {
    pub context: ProviderContext,
    pub artifact: ArtifactRef,
    pub observed_sequence: Option<i64>,
}

/// Bring recorded operation facts into a tracked source before changing its epoch. A later
/// transcript entry other than an operation update forces redirected context reconstruction.
pub(crate) fn current_source(
    store: &dyn ApplicationStore,
    artifacts: &Arc<ArtifactService>,
    ctx: &InvocationContext,
    session_id: &Id,
    source: &ArtifactRef,
    binding: &NativeBinding,
) -> Result<EpochSource, StorageError> {
    // Capture before reading the source artifact so an untracked initial context cannot cross
    // a transcript change that lands during materialization.
    let initial_sequence = read_snapshot(store, &ReadOptions::default(), |read| {
        let session = read
            .sessions()
            .session(session_id)?
            .ok_or(StorageError::ScopeDenied)?;
        if session.closed() || ctx.session_id() != Some(session_id) {
            return Err(StorageError::ScopeDenied);
        }
        Ok(session.last_durable_sequence())
    })?;
    let (bytes, provenance) = artifacts
        .read_exact(store, ctx, source, 8 * 1024 * 1024)
        .map_err(|_| StorageError::ScopeDenied)?;
    let tracked = matches!(
        provenance["derived"]["kind"].as_str(),
        Some(
            "ProviderTurnContinuation"
                | "ProviderToolContinuation"
                | "ProviderOperationUpdateContext"
                | "ProviderContextEpoch"
        )
    );
    let (bytes, artifact, observed_sequence) = if tracked {
        let latest =
            crate::tool_turn::append_operation_updates(store, artifacts, ctx, session_id, source)?;
        let (bytes, provenance) = artifacts
            .read_exact(store, ctx, &latest, 8 * 1024 * 1024)
            .map_err(|_| StorageError::ScopeDenied)?;
        let derived = &provenance["derived"];
        if derived["binding"] != json!(binding) {
            return Err(StorageError::ScopeDenied);
        }
        let observed = derived["observedSequence"]
            .as_i64()
            .ok_or(StorageError::ScopeDenied)?;
        (bytes, latest, Some(observed))
    } else {
        (bytes, source.clone(), Some(initial_sequence))
    };
    let context: ProviderContext =
        serde_json::from_slice(&bytes).map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
    context
        .validate()
        .map_err(|e| StorageError::InvalidRecord(format!("epoch context: {e:?}")))?;
    Ok(EpochSource {
        context,
        artifact,
        observed_sequence,
    })
}

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
    let source = current_source(
        store,
        artifacts,
        ctx,
        session_id,
        previous_context,
        previous_binding,
    )?;
    let (converted, omitted) = source
        .context
        .converted_for_new_epoch(previous_binding, next_binding)
        .map_err(|e| StorageError::InvalidRecord(format!("provider context conversion: {e:?}")))?;
    publish(
        store,
        artifacts,
        ctx,
        session_id,
        EpochChange {
            previous_epoch,
            binding: next_binding.clone(),
            observed_sequence: source.observed_sequence,
            source: source.artifact,
            reason: ContextChangeReason::ProviderChange,
            context: converted,
            omitted,
            adoption: None,
        },
    )
}

pub(crate) fn publish(
    store: &dyn ApplicationStore,
    artifacts: &Arc<ArtifactService>,
    ctx: &InvocationContext,
    session_id: &Id,
    change: EpochChange,
) -> Result<ArtifactRef, StorageError> {
    let EpochChange {
        previous_epoch,
        binding,
        observed_sequence,
        source,
        reason,
        context: converted,
        omitted,
        adoption,
    } = change;
    if ctx.session_id() != Some(session_id) || previous_epoch < 0 {
        return Err(StorageError::ScopeDenied);
    }
    let next_epoch = previous_epoch
        .checked_add(1)
        .ok_or_else(|| StorageError::InvalidRecord("context epoch overflow".into()))?;
    if binding.context_epoch.get() != next_epoch {
        return Err(StorageError::InvalidRecord(
            "context epoch binding mismatch".into(),
        ));
    }
    converted
        .validate()
        .map_err(|e| StorageError::InvalidRecord(format!("context epoch: {e:?}")))?;
    let reason_name = match &reason {
        ContextChangeReason::ProviderChange => "ProviderChange",
        ContextChangeReason::InstructionAdoption => "InstructionAdoption",
        ContextChangeReason::Compaction => "Compaction",
    };
    if omitted.len() > 127 {
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
            json!({"kind":"ProviderContextEpoch","reason":reason_name,
            "sessionId":session_id,"previousEpoch":previous_epoch,"nextEpoch":next_epoch,
            "omittedNative":omitted}),
        )?
        .map_err(StorageError::InvalidRecord)?;
    let staged_for_write = staged.clone();
    let artifacts_for_write = artifacts.clone();
    let ctx_for_write = ctx.clone();
    let session_for_write = session_id.clone();
    let omitted_for_write = omitted.clone();
    let source_for_write = source.clone();
    let binding_for_write = binding.clone();
    let committed = run_write(store, WriteOptions::default(), move |tx| {
        let session = tx
            .sessions()
            .session(&session_for_write)?
            .ok_or(StorageError::ScopeDenied)?;
        if session.closed()
            || session.context_epoch != previous_epoch
            || observed_sequence.is_some_and(|sequence| session.last_durable_sequence() != sequence)
        {
            return Err(StorageError::ScopeDenied);
        }
        let source_version = tx
            .artifacts()
            .version(&source_for_write.version_id)?
            .filter(|version| version.live() && version.artifact_id == source_for_write.artifact_id)
            .ok_or(StorageError::ScopeDenied)?;
        // A context boundary must not invalidate an admitted native request before its
        // transport and accounting have become locally quiescent. A later admission against the
        // old epoch is rejected by the provider send gate after this transaction commits.
        let filter = UsageAttemptFilter {
            session_id: Some(session_for_write.clone()),
            ..Default::default()
        };
        let mut after = None;
        let mut inspected = 0usize;
        loop {
            let page = tx
                .usage()
                .inference_attempts(&filter, after.as_ref(), 100)?;
            if page.is_empty() {
                break;
            }
            inspected += page.len();
            if inspected > 10_000 {
                return Err(StorageError::InvalidRecord(
                    "context epoch attempt scan bound".into(),
                ));
            }
            for attempt in &page {
                if !tx
                    .usage()
                    .attempt_balance(&attempt.attempt_id)?
                    .is_some_and(|balance| balance.local_quiescent)
                {
                    return Err(StorageError::unavailable(
                        Unavailable::UnsupportedRuntime,
                        "context epoch waits for active inference",
                    ));
                }
            }
            after = page.last().map(|attempt| attempt.attempt_id.clone());
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
                provenance: json!({"kind":"ProviderContextEpoch","reason":reason_name,
                    "sessionId":session_for_write,"previousEpoch":previous_epoch,
                    "binding":binding_for_write,"observedSequence":session.next_sequence,
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
                reason: reason.clone(),
                context: reference.clone(),
            }],
            || tx.new_id(IdKind::TranscriptEntry),
        );
        let mut record = SessionBatch::entries(batch);
        record.context_sources = std::iter::once(ContextSourceInput {
            source_id: source_for_write.artifact_id.clone(),
            kind: "provider_context".into(),
            source_revision: source_version.version_no,
            artifact: Some(source_for_write.clone()),
            token_estimate: source_for_write.byte_length.get() / 4
                + i64::from(source_for_write.byte_length.get() % 4 != 0),
            disposition: SourceDisposition::Included,
        })
        .chain(omitted_for_write.iter().map(|artifact| ContextSourceInput {
            source_id: artifact.artifact_id.clone(),
            kind: "provider_native".into(),
            source_revision: 1,
            artifact: Some(artifact.clone()),
            token_estimate: 0,
            disposition: SourceDisposition::Omitted,
        }))
        .collect();
        match SessionWriter::append_in(tx, &ctx_for_write, &record)? {
            BatchOutcome::Accepted(_) => {
                if let Some(payload) = adoption {
                    let session = tx
                        .sessions()
                        .session(&session_for_write)?
                        .ok_or(StorageError::ScopeDenied)?;
                    let id = devworks_core::protocol::EventId(tx.new_id(IdKind::Event));
                    tx.history()
                        .append(devworks_core::ports::storage::NewEvent {
                            id,
                            actor: ctx_for_write.principal().clone(),
                            project_id: session.project_id.clone(),
                            root_id: session.root_id.clone(),
                            operation_id: None,
                            aggregate_kind:
                                devworks_core::services::sessions::transcript::SESSION_AGGREGATE
                                    .into(),
                            aggregate_id: session.id.as_str().to_owned(),
                            aggregate_revision: session.revision,
                            kind: "SkillRevisionsAdopted".into(),
                            payload,
                        })?;
                }
                Ok(reference)
            }
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
